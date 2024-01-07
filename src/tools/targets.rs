use crate::graph::FileId;
use crate::load;
use std::collections::BTreeSet;
use anyhow::bail;

// Implements the "targets" tool.
//
// The targets rule is rather convoluted. It has 3 modes. The mode to use is
// the first argument, and the default is "depth".
//   - depth: prints a tree of files and their dependencies, starting from all
//            of the root nodes in the graph. An argument can be given to
//            specify the maximum depth to print out, the default is 1.
//   - rule: the rule mode takes an argument that's the name of a rule. It will
//           print out all the files produced by builds using that rule. If the
//           rule argument is not given, it will instead print out all
//           "source files" in the graph, that is, files that are not produced
//           by any build.
//   - all: prints out the output files of all builds and the name of the rule
//          used to produce them.
pub fn tool_targets(build_file: &str, args: &Vec<String>) -> anyhow::Result<i32> {
    let state = load::read(build_file, load::Options {
        record_rule_in_builds: true,
        ..load::Options::default()
    })?;
    match args.first().map(|x| x.as_str()) {
        Some("rule") => {
            match args.len() {
                0 => unreachable!(),
                1 => {
                    for build in state.graph.builds.values() {
                        for &id in &build.ins.ids {
                            let file = state.graph.file(id);
                            if file.input.is_none() {
                                println!("{}", file.name);
                            }
                        }
                    }
                },
                2 => {
                    let rule = &args[1];
                    let mut outputs = BTreeSet::new();
                    for build in state.graph.builds.values() {
                        if build.rule.as_ref() == Some(rule) {
                            for &id in &build.outs.ids {
                                outputs.insert(state.graph.file(id).name.clone());
                            }
                        }
                    }
                    for output in outputs {
                        println!("{}", output);
                    }
                },
                _ => bail!("too many arguments to targets tool"),
            };
        },
        Some("depth") | None => {
            let max_depth = match args.len() {
                0 | 1 => 1,
                2 => args[1].parse::<i32>()?,
                _ => bail!("too many arguments to targets tool"),
            };
            // The graph contains file entries for included ninja files, so
            // we only consider files that are produced by a build.
            let mut root_nodes = Vec::new();
            for build in state.graph.builds.values() {
                for &file_id in &build.outs.ids {
                    if state.graph.file(file_id).dependents.is_empty() {
                        root_nodes.push(file_id);
                    }
                }
            }
            print_files_recursively(&state, &root_nodes, 0, max_depth);
        },
        Some("all") => {
            if args.len() > 1 {
                bail!("too many arguments to targets tool");
            }
            for build in state.graph.builds.values() {
                for &id in &build.outs.ids {
                    println!("{}: {}", state.graph.file(id).name, build.rule.as_ref().unwrap())
                }
            }
        },
        Some(mode) => bail!("unknown target tool mode {:?}, valid modes are \"rule\", \"depth\", or \"all\".", mode),
    }
    Ok(0)
}

/// print all the files in the `files` argument, and then all of their
/// dependencies, recursively. `depth` is used to determine the indentation,
/// start it at 0. max_depth limits the recursion depth, but if it's <= 0,
/// recursion depth will not be limited.
fn print_files_recursively(state: &load::State, files: &[FileId], depth: i32, max_depth: i32) {
    for &file_id in files {
        for _ in 0..depth {
            print!("  ");
        }
        let file = state.graph.file(file_id);
        if let Some(build_id) = file.input {
            let build = state.graph.builds.lookup(build_id).unwrap();
            println!("{}: {}", &file.name, build.rule.as_ref().unwrap());
            if max_depth <= 0 || depth < max_depth - 1 {
                print_files_recursively(state, build.ordering_ins(), depth+1, max_depth);
            }
        } else {
            println!("{}", &file.name);
        }
    }
}
