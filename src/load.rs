//! Graph loading: runs .ninja parsing and constructs the build graph from it.

use crate::{
    canon::canon_path,
    densemap::Index,
    eval::{EvalPart, EvalString},
    file_pool::FilePool,
    graph::{BuildId, FileId, Graph, RspFile},
    parse::{Build, DefaultStmt, IncludeOrSubninja, Rule, Statement, VariableAssignment},
    scanner,
    scanner::ParseResult,
    smallmap::SmallMap,
    {db, eval, graph, parse, trace},
};
use anyhow::{anyhow, bail};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::{borrow::Cow, path::Path, sync::{atomic::AtomicUsize, mpsc::TryRecvError}};
use std::{
    cell::UnsafeCell,
    cmp::Ordering,
    collections::{hash_map::Entry, HashMap},
    sync::{Arc, Mutex},
    thread::available_parallelism,
};
use std::{path::PathBuf, sync::atomic::AtomicU32};

/// A variable lookup environment for magic $in/$out variables.
struct BuildImplicitVars<'a> {
    explicit_ins: &'a [String],
    explicit_outs: &'a [String],
}
impl<'text> eval::Env for BuildImplicitVars<'text> {
    fn get_var(&self, var: &str) -> Option<EvalString<Cow<str>>> {
        let string_to_evalstring =
            |s: String| Some(EvalString::new(vec![EvalPart::Literal(Cow::Owned(s))]));
        match var {
            "in" => string_to_evalstring(self.explicit_ins.join(" ")),
            "in_newline" => string_to_evalstring(self.explicit_ins.join("\n")),
            "out" => string_to_evalstring(self.explicit_outs.join(" ")),
            "out_newline" => string_to_evalstring(self.explicit_outs.join("\n")),
            _ => None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ScopePosition(pub usize);

pub struct ParentScopeReference<'text>(pub Arc<Scope<'text>>, pub ScopePosition);

pub struct Scope<'text> {
    parent: Option<ParentScopeReference<'text>>,
    rules: HashMap<&'text str, Rule<'text>>,
    variables: FxHashMap<&'text str, Vec<VariableAssignment<'text>>>,
    next_free_position: ScopePosition,
}

impl<'text> Scope<'text> {
    pub fn new(parent: Option<ParentScopeReference<'text>>) -> Self {
        Self {
            parent,
            rules: HashMap::new(),
            variables: FxHashMap::default(),
            next_free_position: ScopePosition(0),
        }
    }

    pub fn get_and_inc_scope_position(&mut self) -> ScopePosition {
        let result = self.next_free_position;
        self.next_free_position.0 += 1;
        result
    }

    pub fn get_last_scope_position(&self) -> ScopePosition {
        self.next_free_position
    }

    pub fn get_rule(&self, name: &'text str, position: ScopePosition) -> Option<&Rule> {
        match self.rules.get(name) {
            Some(rule) if rule.scope_position.0 < position.0 => Some(rule),
            Some(_) | None => self
                .parent
                .as_ref()
                .map(|p| p.0.get_rule(name, p.1))
                .flatten(),
        }
    }

    pub fn evaluate(&self, result: &mut String, varname: &'text str, position: ScopePosition) {
        if let Some(variables) = self.variables.get(varname) {
            let i = variables
                .binary_search_by(|x| {
                    if x.scope_position.0 < position.0 {
                        Ordering::Less
                    } else if x.scope_position.0 > position.0 {
                        Ordering::Greater
                    } else {
                        // If we're evaluating a variable assignment, we don't want to
                        // get the same assignment, but instead, we want the one just
                        // before it. So return Greater instead of Equal.
                        Ordering::Greater
                    }
                })
                .unwrap_err();
            let i = std::cmp::min(i, variables.len() - 1);
            if variables[i].scope_position.0 < position.0 {
                variables[i].evaluate(result, &self);
                return;
            }
            // We couldn't find a variable assignment before the input
            // position, so check the parent scope if there is one.
        }
        if let Some(parent) = &self.parent {
            parent.0.evaluate(result, varname, position);
        }
    }
}

fn add_build<'text>(
    files: &Files,
    filename: Arc<PathBuf>,
    scope: &Scope,
    b: parse::Build,
) -> anyhow::Result<SubninjaResults<'text>> {
    let ins: Vec<_> = b
        .ins
        .iter()
        .map(|x| canon_path(x.evaluate(&[&b.vars], scope, b.scope_position)))
        .collect();
    let outs: Vec<_> = b
        .outs
        .iter()
        .map(|x| canon_path(x.evaluate(&[&b.vars], scope, b.scope_position)))
        .collect();

    let rule = match scope.get_rule(b.rule, b.scope_position) {
        Some(r) => r,
        None => bail!("unknown rule {:?}", b.rule),
    };

    let implicit_vars = BuildImplicitVars {
        explicit_ins: &ins[..b.explicit_ins],
        explicit_outs: &outs[..b.explicit_outs],
    };

    // temp variable in order to not move all of b into the closure
    let build_vars = &b.vars;
    let lookup = |key: &str| -> Option<String> {
        // Look up `key = ...` binding in build and rule block.
        Some(match rule.vars.get(key) {
            Some(val) => val.evaluate(&[&implicit_vars, build_vars], scope, b.scope_position),
            None => build_vars.get(key)?.evaluate(&[], scope, b.scope_position),
        })
    };

    let cmdline = lookup("command");
    let desc = lookup("description");
    let depfile = lookup("depfile");
    let parse_showincludes = match lookup("deps").as_deref() {
        None => false,
        Some("gcc") => false,
        Some("msvc") => true,
        Some(other) => bail!("invalid deps attribute {:?}", other),
    };
    let pool = lookup("pool");

    let rspfile_path = lookup("rspfile");
    let rspfile_content = lookup("rspfile_content");
    let rspfile = match (rspfile_path, rspfile_content) {
        (None, None) => None,
        (Some(path), Some(content)) => Some(RspFile {
            path: std::path::PathBuf::from(path),
            content,
        }),
        _ => bail!("rspfile and rspfile_content need to be both specified"),
    };

    let build_id = files.create_build_id();

    let ins = graph::BuildIns {
        ids: ins
            .into_iter()
            .map(|x| files.id_from_canonical_and_add_dependant(x, build_id))
            .collect(),
        explicit: b.explicit_ins,
        implicit: b.implicit_ins,
        order_only: b.order_only_ins,
        // validation is implied by the other counts
    };
    let outs = graph::BuildOuts {
        ids: outs
            .into_iter()
            .map(|x| files.id_from_canonical(x))
            .collect(),
        explicit: b.explicit_outs,
    };
    let mut build = graph::Build::new(
        build_id,
        graph::FileLoc {
            filename,
            line: b.line,
        },
        ins,
        outs,
    );

    build.cmdline = cmdline;
    build.desc = desc;
    build.depfile = depfile;
    build.parse_showincludes = parse_showincludes;
    build.rspfile = rspfile;
    build.pool = pool;

    graph::Graph::initialize_build(&files.by_id, &mut build)?;

    Ok(SubninjaResults {
        builds: vec![build],
        ..SubninjaResults::default()
    })
}

struct Files {
    by_name: dashmap::DashMap<String, FileId>,
    by_id: dashmap::DashMap<FileId, graph::File>,
    next_id: AtomicU32,
    next_build_id: AtomicUsize,
}
impl Files {
    pub fn new() -> Self {
        Self {
            by_name: dashmap::DashMap::new(),
            by_id: dashmap::DashMap::new(),
            next_id: AtomicU32::new(0),
            next_build_id: AtomicUsize::new(0),
        }
    }

    pub fn id_from_canonical(&self, file: String) -> FileId {
        match self.by_name.entry(file) {
            dashmap::mapref::entry::Entry::Occupied(o) => *o.get(),
            dashmap::mapref::entry::Entry::Vacant(v) => {
                let id = self
                    .next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let id = FileId::from(id);
                let mut f = graph::File::default();
                f.name = v.key().clone();
                self.by_id.insert(id, f);
                v.insert(id);
                id
            }
        }
    }

    pub fn id_from_canonical_and_add_dependant(&self, file: String, build: BuildId) -> FileId {
        match self.by_name.entry(file) {
            dashmap::mapref::entry::Entry::Occupied(o) => {
                let id = *o.get();
                self.by_id.get(&id).unwrap().dependents.prepend(build);
                id
            },
            dashmap::mapref::entry::Entry::Vacant(v) => {
                let id = self
                    .next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let id = FileId::from(id);
                let mut f = graph::File::default();
                f.name = v.key().clone();
                f.dependents.prepend(build);
                self.by_id.insert(id, f);
                v.insert(id);
                id
            }
        }
    }

    pub fn into_maps(
        self,
    ) -> (
        dashmap::DashMap<String, FileId>,
        dashmap::DashMap<FileId, graph::File>,
    ) {
        (self.by_name, self.by_id)
    }

    pub fn create_build_id(&self) -> BuildId {
        let id = self
            .next_build_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        BuildId::from(id)
    }
}

#[derive(Default)]
struct SubninjaResults<'text> {
    pub builds: Vec<graph::Build>,
    defaults: Vec<FileId>,
    builddir: Option<String>,
    pools: SmallMap<&'text str, usize>,
}

fn subninja<'thread, 'text>(
    num_threads: usize,
    files: &'thread Files,
    file_pool: &'text FilePool,
    path: String,
    parent_scope: Option<ParentScopeReference<'text>>,
    executor: &rayon::Scope<'thread>,
) -> anyhow::Result<SubninjaResults<'text>>
where
    'text: 'thread,
{
    let path = PathBuf::from(path);
    let top_level_scope = parent_scope.is_none();
    let mut scope = Scope::new(parent_scope);
    if top_level_scope {
        let position = scope.get_and_inc_scope_position();
        scope.rules.insert(
            "phony",
            Rule {
                name: "phony",
                vars: SmallMap::default(),
                scope_position: position,
            },
        );
    }
    let parse_results = parse(
        num_threads,
        file_pool,
        file_pool.read_file(&path)?,
        &mut scope,
        executor,
    )?;
    let (sender, receiver) = std::sync::mpsc::channel::<anyhow::Result<SubninjaResults<'text>>>();
    let scope = Arc::new(scope);
    for sn in parse_results.subninjas.into_iter() {
        let scope = scope.clone();
        let sender = sender.clone();
        executor.spawn(move |executor| {
            let file = canon_path(sn.file.evaluate(&[], &scope, sn.scope_position));
            sender
                .send(subninja(
                    num_threads,
                    files,
                    file_pool,
                    file,
                    Some(ParentScopeReference(scope, sn.scope_position)),
                    executor,
                ))
                .unwrap();
        });
    }
    let filename = Arc::new(path);
    for build in parse_results.builds.into_iter() {
        let filename = filename.clone();
        let scope = scope.clone();
        let sender = sender.clone();
        executor.spawn(move |_| {
            sender
                .send(add_build(files, filename, &scope, build))
                .unwrap();
        });
    }
    let mut results = SubninjaResults::default();
    results.pools = parse_results.pools;
    for default in parse_results.defaults.into_iter() {
        let scope = scope.clone();
        results.defaults.extend(default.files.iter().map(|x| {
            let path = canon_path(x.evaluate(&[], &scope, default.scope_position));
            files.id_from_canonical(path)
        }));
    }

    // Only the builddir in the outermost scope is respected
    if top_level_scope {
        let mut build_dir = String::new();
        scope.evaluate(&mut build_dir, "builddir", scope.get_last_scope_position());
        if !build_dir.is_empty() {
            results.builddir = Some(build_dir);
        }
    }

    drop(sender);

    let mut err = None;
    loop {
        match receiver.try_recv() {
            Ok(Err(e)) => {
                if err.is_none() {
                    err = Some(Err(e))
                }
            },
            Ok(Ok(new_results)) => {
                results.builds.extend(new_results.builds);
                results.defaults.extend(new_results.defaults);
                for (name, depth) in new_results.pools.into_iter() {
                    add_pool(&mut results.pools, name, depth)?;
                }
            },
            // We can't risk having any tasks blocked on other tasks, lest
            // the thread pool fill up with only blocked tasks.
            Err(TryRecvError::Empty) => {rayon::yield_now();},
            Err(TryRecvError::Disconnected) => break,
        }
    }

    if let Some(e) = err {
        e
    } else {
        Ok(results)
    }
}

fn include<'thread, 'text>(
    num_threads: usize,
    file_pool: &'text FilePool,
    path: String,
    scope: &mut Scope<'text>,
    executor: &rayon::Scope<'thread>,
) -> anyhow::Result<ParseResults<'text>>
where
    'text: 'thread,
{
    let path = PathBuf::from(path);
    parse(
        num_threads,
        file_pool,
        file_pool.read_file(&path)?,
        scope,
        executor,
    )
}

fn add_pool<'text>(
    pools: &mut SmallMap<&'text str, usize>,
    name: &'text str,
    depth: usize,
) -> anyhow::Result<()> {
    if let Some(_) = pools.get(name) {
        bail!("duplicate pool {}", name);
    }
    pools.insert(name, depth);
    Ok(())
}

#[derive(Default)]
struct ParseResults<'text> {
    builds: Vec<Build<'text>>,
    defaults: Vec<DefaultStmt<'text>>,
    subninjas: Vec<IncludeOrSubninja<'text>>,
    pools: SmallMap<&'text str, usize>,
}

impl<'text> ParseResults<'text> {
    pub fn merge(&mut self, other: ParseResults<'text>) -> anyhow::Result<()> {
        self.builds.extend(other.builds);
        self.defaults.extend(other.defaults);
        self.subninjas.extend(other.subninjas);
        for (name, depth) in other.pools.into_iter() {
            add_pool(&mut self.pools, name, depth)?;
        }
        Ok(())
    }
}

fn parse<'thread, 'text>(
    num_threads: usize,
    file_pool: &'text FilePool,
    bytes: &'text [u8],
    scope: &mut Scope<'text>,
    executor: &rayon::Scope<'thread>,
) -> anyhow::Result<ParseResults<'text>>
where
    'text: 'thread,
{
    let chunks = parse::split_manifest_into_chunks(bytes, num_threads);

    let mut receivers = Vec::with_capacity(chunks.len());

    for chunk in chunks.into_iter() {
        let (sender, receiver) = std::sync::mpsc::channel::<ParseResult<Statement<'text>>>();
        receivers.push(receiver);
        executor.spawn(move |_| {
            let mut parser = parse::Parser::new(chunk);
            parser.read_to_channel(sender);
        })
    }

    let mut i = 0;
    let mut results = ParseResults::default();
    while i < receivers.len() {
        match receivers[i].try_recv() {
            Ok(Ok(Statement::VariableAssignment(mut variable_assignment))) => {
                variable_assignment.scope_position = scope.get_and_inc_scope_position();
                match scope.variables.entry(variable_assignment.name) {
                    Entry::Occupied(mut e) => e.get_mut().push(variable_assignment),
                    Entry::Vacant(e) => {
                        e.insert(vec![variable_assignment]);
                    }
                }
            }
            Ok(Ok(Statement::Include(i))) => trace::scope("include", || -> anyhow::Result<()> {
                let evaluated = canon_path(i.file.evaluate(&[], &scope, i.scope_position));
                let new_results = include(num_threads, file_pool, evaluated, scope, executor)?;
                results.merge(new_results)?;
                Ok(())
            })?,
            Ok(Ok(Statement::Subninja(mut subninja))) => trace::scope("subninja", || {
                subninja.scope_position = scope.get_and_inc_scope_position();
                results.subninjas.push(subninja);
            }),
            Ok(Ok(Statement::Default(mut default))) => {
                default.scope_position = scope.get_and_inc_scope_position();
                results.defaults.push(default);
            }
            Ok(Ok(Statement::Rule(mut rule))) => {
                rule.scope_position = scope.get_and_inc_scope_position();
                match scope.rules.entry(rule.name) {
                    Entry::Occupied(_) => bail!("duplicate rule '{}'", rule.name),
                    Entry::Vacant(e) => e.insert(rule),
                };
            }
            Ok(Ok(Statement::Build(mut build))) => {
                build.scope_position = scope.get_and_inc_scope_position();
                results.builds.push(build);
            }
            Ok(Ok(Statement::Pool(pool))) => {
                add_pool(&mut results.pools, pool.name, pool.depth)?;
            }
            // TODO: Call format_parse_error
            Ok(Err(e)) => bail!(e.msg),
            // We can't risk having any tasks blocked on other tasks, lest
            // the thread pool fill up with only blocked tasks.
            Err(TryRecvError::Empty) => {rayon::yield_now();},
            Err(TryRecvError::Disconnected) => {i += 1;},
        };
    }
    Ok(results)
}

/// State loaded by read().
pub struct State {
    pub graph: graph::Graph,
    pub db: db::Writer,
    pub hashes: graph::Hashes,
    pub default: Vec<FileId>,
    pub pools: SmallMap<String, usize>,
}

/// Load build.ninja/.n2_db and return the loaded build graph and state.
pub fn read(build_filename: &str) -> anyhow::Result<State> {
    let build_filename = canon_path(build_filename);
    let file_pool = FilePool::new();
    let files = Files::new();
    let num_threads = available_parallelism()?.get();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build()?;
    let SubninjaResults {
        builds,
        defaults,
        builddir,
        pools,
    } = trace::scope("loader.read_file", || -> anyhow::Result<SubninjaResults> {
        pool.scope(|executor: &rayon::Scope| {
            let mut results = subninja(
                num_threads,
                &files,
                &file_pool,
                build_filename,
                None,
                executor,
            )?;
            results.builds.par_sort_unstable_by_key(|b| b.id.index());
            Ok(results)
        })
    })?;
    let mut graph = trace::scope("loader.from_uninitialized_builds_and_files", || {
        Graph::from_uninitialized_builds_and_files(builds, files.into_maps())
    })?;
    let mut hashes = graph::Hashes::default();
    let db = trace::scope("db::open", || {
        let mut db_path = PathBuf::from(".n2_db");
        if let Some(builddir) = &builddir {
            db_path = Path::new(&builddir).join(db_path);
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        };
        db::open(&db_path, &mut graph, &mut hashes)
    })
    .map_err(|err| anyhow!("load .n2_db: {}", err))?;

    let mut owned_pools = SmallMap::with_capacity(pools.len());
    for pool in pools.iter() {
        owned_pools.insert(pool.0.to_owned(), pool.1);
    }

    Ok(State {
        graph,
        db,
        hashes,
        default: defaults,
        pools: owned_pools,
    })
}
