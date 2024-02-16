//! The build graph, a graph between files and commands.

use rustc_hash::{FxHashMap, FxHasher};

use crate::{
    concurrent_linked_list::ConcurrentLinkedList,
    densemap::{self, DenseMap},
    hash::BuildHash,
    trace,
};
use std::time::SystemTime;
use std::{collections::HashMap, sync::Arc};
use std::{
    hash::BuildHasherDefault,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Id for File nodes in the Graph.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct FileId(u32);
impl densemap::Index for FileId {
    fn index(&self) -> usize {
        self.0 as usize
    }
}
impl From<usize> for FileId {
    fn from(u: usize) -> FileId {
        FileId(u as u32)
    }
}
impl From<u32> for FileId {
    fn from(u: u32) -> FileId {
        FileId(u)
    }
}

/// Id for Build nodes in the Graph.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct BuildId(u32);
impl densemap::Index for BuildId {
    fn index(&self) -> usize {
        self.0 as usize
    }
}
impl From<usize> for BuildId {
    fn from(u: usize) -> BuildId {
        BuildId(u as u32)
    }
}

/// A single file referenced as part of a build.
#[derive(Debug, Default)]
pub struct File {
    /// Canonical path to the file.
    pub name: Arc<String>,
    /// The Build that generates this file, if any.
    pub input: Mutex<Option<BuildId>>,
    /// The Builds that depend on this file as an input.
    pub dependents: ConcurrentLinkedList<BuildId>,
}

impl File {
    pub fn path(&self) -> &Path {
        Path::new(self.name.as_ref())
    }
}

/// A textual location within a build.ninja file, used in error messages.
#[derive(Debug, Clone)]
pub struct FileLoc {
    pub filename: Arc<PathBuf>,
    pub line: usize,
}
impl std::fmt::Display for FileLoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{}:{}", self.filename.display(), self.line)
    }
}

#[derive(Debug, Clone, Hash)]
pub struct RspFile {
    pub path: std::path::PathBuf,
    pub content: String,
}

/// Input files to a Build.
pub struct BuildIns {
    /// Internally we stuff explicit/implicit/order-only ins all into one Vec.
    /// This is mostly to simplify some of the iteration and is a little more
    /// memory efficient than three separate Vecs, but it is kept internal to
    /// Build and only exposed via methods on Build.
    pub ids: Vec<Arc<File>>,
    pub explicit: usize,
    pub implicit: usize,
    pub order_only: usize,
    // validation count implied by other counts.
    // pub validation: usize,
}

/// Output files from a Build.
pub struct BuildOuts {
    /// Similar to ins, we keep both explicit and implicit outs in one Vec.
    pub ids: Vec<Arc<File>>,
    pub explicit: usize,
}

impl BuildOuts {
    /// CMake seems to generate build files with the same output mentioned
    /// multiple times in the outputs list.  Given that Ninja accepts these,
    /// this function removes duplicates from the output list.
    pub fn remove_duplicates(&mut self) {
        let mut ids = Vec::new();
        for (i, id) in self.ids.iter().enumerate() {
            if self.ids[0..i]
                .iter()
                .any(|prev| std::ptr::eq(prev.as_ref(), id.as_ref()))
            {
                // Skip over duplicate.
                if i < self.explicit {
                    self.explicit -= 1;
                }
                continue;
            }
            ids.push(id.clone());
        }
        self.ids = ids;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_file_arc_vecs_equal(a: Vec<Arc<File>>, b: Vec<Arc<File>>) {
        for (x, y) in a.into_iter().zip(b.into_iter()) {
            if !Arc::ptr_eq(&x, &y) {
                panic!("File vecs not equal");
            }
        }
    }

    #[test]
    fn remove_dups_explicit() {
        let file1 = Arc::new(File::default());
        let file2 = Arc::new(File::default());
        let mut outs = BuildOuts {
            ids: vec![file1.clone(), file1.clone(), file2.clone()],
            explicit: 2,
        };
        outs.remove_duplicates();
        assert_file_arc_vecs_equal(outs.ids, vec![file1, file2]);
        assert_eq!(outs.explicit, 1);
    }

    #[test]
    fn remove_dups_implicit() {
        let file1 = Arc::new(File::default());
        let file2 = Arc::new(File::default());
        let mut outs = BuildOuts {
            ids: vec![file1.clone(), file2.clone(), file1.clone()],
            explicit: 2,
        };
        outs.remove_duplicates();
        assert_file_arc_vecs_equal(outs.ids, vec![file1, file2]);
        assert_eq!(outs.explicit, 2);
    }
}

/// A single build action, generating File outputs from File inputs with a command.
pub struct Build {
    pub id: BuildId,

    /// Source location this Build was declared.
    pub location: FileLoc,

    /// User-provided description of the build step.
    pub desc: Option<String>,

    /// Command line to run.  Absent for phony builds.
    pub cmdline: Option<String>,

    /// Path to generated `.d` file, if any.
    pub depfile: Option<String>,

    /// If true, extract "/showIncludes" lines from output.
    pub parse_showincludes: bool,

    // Struct that contains the path to the rsp file and its contents, if any.
    pub rspfile: Option<RspFile>,

    /// Pool to execute this build in, if any.
    pub pool: Option<String>,

    pub ins: BuildIns,

    /// Additional inputs discovered from a previous build.
    discovered_ins: Vec<Arc<File>>,

    /// Output files.
    pub outs: BuildOuts,
}
impl Build {
    pub fn new(id: BuildId, loc: FileLoc, ins: BuildIns, outs: BuildOuts) -> Self {
        Build {
            id,
            location: loc,
            desc: None,
            cmdline: None,
            depfile: None,
            parse_showincludes: false,
            rspfile: None,
            pool: None,
            ins,
            discovered_ins: Vec::new(),
            outs,
        }
    }

    /// Input paths that appear in `$in`.
    pub fn explicit_ins(&self) -> &[Arc<File>] {
        &self.ins.ids[0..self.ins.explicit]
    }

    /// Input paths that, if changed, invalidate the output.
    /// Note this omits discovered_ins, which also invalidate the output.
    pub fn dirtying_ins(&self) -> &[Arc<File>] {
        &self.ins.ids[0..(self.ins.explicit + self.ins.implicit)]
    }

    /// Inputs that are needed before building.
    /// Distinct from dirtying_ins in that it includes order-only dependencies.
    /// Note that we don't order on discovered_ins, because they're not allowed to
    /// affect build order.
    pub fn ordering_ins(&self) -> &[Arc<File>] {
        &self.ins.ids[0..(self.ins.order_only + self.ins.explicit + self.ins.implicit)]
    }

    /// Inputs that are needed before validating information.
    /// Validation inputs will be built whenever this Build is built, but this Build will not
    /// wait for them to complete before running. The validation inputs can fail to build, which
    /// will cause the overall build to fail.
    pub fn validation_ins(&self) -> &[Arc<File>] {
        &self.ins.ids[(self.ins.order_only + self.ins.explicit + self.ins.implicit)..]
    }

    fn vecs_of_arcs_eq<T>(a: &Vec<Arc<T>>, b: &Vec<Arc<T>>) -> bool {
        if a.len() != b.len() {
            return false;
        }
        for (x, y) in a.iter().zip(b.iter()) {
            if !Arc::ptr_eq(x, y) {
                return false;
            }
        }
        return true;
    }

    /// Potentially update discovered_ins with a new set of deps, returning true if they changed.
    pub fn update_discovered(&mut self, deps: Vec<Arc<File>>) -> bool {
        if Self::vecs_of_arcs_eq(&deps, &self.discovered_ins) {
            false
        } else {
            self.set_discovered_ins(deps);
            true
        }
    }

    pub fn set_discovered_ins(&mut self, deps: Vec<Arc<File>>) {
        self.discovered_ins = deps;
    }

    /// Input paths that were discovered after building, for use in the next build.
    pub fn discovered_ins(&self) -> &[Arc<File>] {
        &self.discovered_ins
    }

    /// Output paths that appear in `$out`.
    pub fn explicit_outs(&self) -> &[Arc<File>] {
        &self.outs.ids[0..self.outs.explicit]
    }

    /// Output paths that are updated when the build runs.
    pub fn outs(&self) -> &[Arc<File>] {
        &self.outs.ids
    }
}

/// The build graph: owns Files/Builds and maps FileIds/BuildIds to them.
#[derive(Default)]
pub struct Graph {
    pub builds: DenseMap<BuildId, Build>,
    pub files: GraphFiles,
}

/// Files identified by FileId, as well as mapping string filenames to them.
/// Split from Graph for lifetime reasons.
#[derive(Default)]
pub struct GraphFiles {
    by_name: dashmap::DashMap<Arc<String>, Arc<File>>,
}

impl Graph {
    pub fn from_uninitialized_builds_and_files(
        builds: Vec<Build>,
        files: dashmap::DashMap<Arc<String>, Arc<File>>,
    ) -> anyhow::Result<Self> {
        let result = Graph {
            builds: DenseMap::from_vec(builds),
            files: GraphFiles { by_name: files },
        };
        Ok(result)
    }

    pub fn initialize_build(build: &mut Build) -> anyhow::Result<()> {
        let new_id = build.id;
        let mut fixup_dups = false;
        for f in &build.outs.ids {
            let mut input = f.input.lock().unwrap();
            match *input {
                Some(prev) if prev == new_id => {
                    fixup_dups = true;
                    println!(
                        "n2: warn: {}: {:?} is repeated in output list",
                        build.location, f.name,
                    );
                }
                Some(prev) => {
                    let location = build.location.clone();
                    anyhow::bail!(
                        "{}: {:?} is already an output at ", // {}
                        location,
                        f.name,
                        // TODO
                        //self.builds[prev].location
                    );
                }
                None => *input = Some(new_id),
            }
        }
        if fixup_dups {
            build.outs.remove_duplicates();
        }
        Ok(())
    }
}

impl GraphFiles {
    /// Look up a file by its name.  Name must have been canonicalized already.
    pub fn lookup(&self, file: String) -> Option<Arc<File>> {
        self.by_name.get(&Arc::new(file)).map(|x| x.clone())
    }

    /// Look up a file by its name, adding it if not already present.
    /// Name must have been canonicalized already. Only accepting an owned
    /// string allows us to avoid a string copy and a hashmap lookup when we
    /// need to create a new id, but would also be possible to create a version
    /// of this function that accepts string references that is more optimized
    /// for the case where the entry already exists. But so far, all of our
    /// usages of this function have an owned string easily accessible anyways.
    pub fn id_from_canonical(&mut self, file: String) -> Arc<File> {
        let file = Arc::new(file);
        // TODO: so many string copies :<
        match self.by_name.entry(file) {
            dashmap::mapref::entry::Entry::Occupied(o) => o.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(v) => {
                let mut f = File::default();
                f.name = v.key().clone();
                let f = Arc::new(f);
                v.insert(f.clone());
                f
            }
        }
    }

    pub fn all_files(&self) -> impl Iterator<Item = Arc<File>> + '_ {
        self.by_name.iter().map(|x| x.clone())
    }

    pub fn num_files(&self) -> usize {
        self.by_name.len()
    }
}

/// MTime info gathered for a file.  This also models "file is absent".
/// It's not using an Option<> just because it makes the code using it easier
/// to follow.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum MTime {
    Missing,
    Stamp(SystemTime),
}

/// stat() an on-disk path, producing its MTime.
pub fn stat(path: &Path) -> std::io::Result<MTime> {
    // TODO: On Windows, use FindFirstFileEx()/FindNextFile() to get timestamps per
    //       directory, for better stat perf.
    Ok(match std::fs::metadata(path) {
        Ok(meta) => MTime::Stamp(meta.modified().unwrap()),
        Err(err) => {
            if err.kind() == std::io::ErrorKind::NotFound {
                MTime::Missing
            } else {
                return Err(err);
            }
        }
    })
}

/// Gathered state of on-disk files.
/// Due to discovered deps this map may grow after graph initialization.
pub struct FileState(FxHashMap<*const File, Option<MTime>>);

impl FileState {
    pub fn new(graph: &Graph) -> Self {
        let hm = HashMap::with_capacity_and_hasher(
            graph.files.num_files(),
            BuildHasherDefault::<FxHasher>::default(),
        );
        FileState(hm)
    }

    pub fn get(&self, id: &File) -> Option<MTime> {
        self.0.get(&(id as *const File)).copied().flatten()
    }

    pub fn stat(&mut self, id: &File, path: &Path) -> anyhow::Result<MTime> {
        let mtime = stat(path).map_err(|err| anyhow::anyhow!("stat {:?}: {}", path, err))?;
        self.0.insert(id as *const File, Some(mtime));
        Ok(mtime)
    }
}

#[derive(Default)]
pub struct Hashes(HashMap<BuildId, BuildHash>);

impl Hashes {
    pub fn set(&mut self, id: BuildId, hash: BuildHash) {
        self.0.insert(id, hash);
    }

    pub fn get(&self, id: BuildId) -> Option<BuildHash> {
        self.0.get(&id).copied()
    }
}

#[test]
fn stat_mtime_resolution() {
    use std::time::Duration;

    let temp_dir = tempfile::tempdir().unwrap();
    let filename = temp_dir.path().join("dummy");

    // Write once and stat.
    std::fs::write(&filename, "foo").unwrap();
    let mtime1 = match stat(&filename).unwrap() {
        MTime::Stamp(mtime) => mtime,
        _ => panic!("File not found: {}", filename.display()),
    };

    // Sleep for a short interval.
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Write twice and stat.
    std::fs::write(&filename, "foo").unwrap();
    let mtime2 = match stat(&filename).unwrap() {
        MTime::Stamp(mtime) => mtime,
        _ => panic!("File not found: {}", filename.display()),
    };

    let diff = mtime2.duration_since(mtime1).unwrap();
    assert!(diff > Duration::ZERO);
    assert!(diff < Duration::from_millis(100));
}
