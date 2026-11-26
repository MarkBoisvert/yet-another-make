//! The compile order for one target's source files, from their scanned module
//! dependencies.
//!
//! Files that provide a module (interfaces and partitions) are compiled first, each
//! after the modules it imports, so its compiled interface (`.pcm`) exists when an
//! importer needs it. Every other file comes after them, in path order. The build
//! graph (#24) and the parallel scheduler (#25) replace this ordering.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::hash::BuildHasher;
use std::path::Path;

use super::scan::Deps;

/// The `std` module, which the build compiles from libc++ itself.
pub const STD_MODULE: &str = "std";
const STD_COMPAT_MODULE: &str = "std.compat";

/// One of a target's source files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFile {
    /// Relative to the project root, with `/` separators.
    pub path: String,
    pub deps: Deps,
}

impl SourceFile {
    /// The module or partition the file provides.
    fn provides(&self) -> Option<&str> {
        self.deps.provides.as_ref().map(|p| p.name.as_str())
    }
}

/// A target's files in compile order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Whether any file imports `std`.
    pub imports_std: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// Index into the files the plan was made from.
    pub file: usize,
    /// The module or partition the file provides.
    pub provides: Option<String>,
}

/// Why a target's files can't be ordered. `target` reads like ``lib target `core` ``.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("`{file}` has no `export module <name>;` declaration")]
    MissingModuleDecl { file: String },

    #[error("module `{module}` is declared by both `{first}` and `{second}`")]
    DuplicateModule {
        module: String,
        first: String,
        second: String,
    },

    #[error(
        "`{file}` imports module `{module}`, but no source file of {target} declares it\n\n\
         help: add the file that declares it to the target's `sources`"
    )]
    UnknownModule {
        target: String,
        file: String,
        module: String,
    },

    #[error("modules in {target} import each other in a cycle: {}", chain(.modules))]
    Cycle {
        target: String,
        /// The cycle, starting and ending with the same module.
        modules: Vec<String>,
    },

    #[error(
        "`{file}` imports `std.compat`, which yam doesn't build yet\n\n\
         help: use `import std;`, and include C headers such as <cstdio> where needed"
    )]
    StdCompat { file: String },

    #[error(
        "{target} has more than one `main()`: {}\n\n\
         help: a bin has one entry file; give each other program its own [[bin]], \
         or remove the extra files from its `sources`",
        quoted(.files)
    )]
    TwoMains { target: String, files: Vec<String> },
}

/// `` `a` → `b` → `a` ``.
fn chain(modules: &[String]) -> String {
    modules
        .iter()
        .map(|module| format!("`{module}`"))
        .collect::<Vec<_>>()
        .join(" → ")
}

fn quoted(items: &[String]) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "`{item}`");
    }
    out
}

/// A bin has one entry file: fail if more than one of its files, `mains`, defines
/// `main()`. `target` names the bin in the message.
///
/// # Errors
///
/// Fails with [`PlanError::TwoMains`] when `mains` has more than one file.
pub fn check_one_main(target: &str, mains: Vec<String>) -> Result<(), PlanError> {
    if mains.len() > 1 {
        return Err(PlanError::TwoMains {
            target: target.to_string(),
            files: mains,
        });
    }
    Ok(())
}

/// Order `files`, which must be sorted by path.
///
/// `target` names the target in messages, and `available` holds the modules already
/// built for it (the project library's, for a bin that links it).
///
/// # Errors
///
/// Fails if a `.cppm` file declares no module, two files declare the same module, a
/// file imports a module nothing provides, interfaces import each other in a cycle,
/// or a file imports `std.compat`.
pub fn plan<S: BuildHasher>(
    target: &str,
    files: &[SourceFile],
    available: &HashSet<String, S>,
) -> Result<Plan, PlanError> {
    let mut provider_of: HashMap<&str, usize> = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        let provides = file.provides();
        if provides.is_none() && is_interface_file(&file.path) {
            return Err(PlanError::MissingModuleDecl {
                file: file.path.clone(),
            });
        }
        if let Some(module) = provides
            && let Some(&first) = provider_of.get(module)
        {
            return Err(PlanError::DuplicateModule {
                module: module.to_string(),
                first: files[first].path.clone(),
                second: file.path.clone(),
            });
        }
        if let Some(module) = provides {
            provider_of.insert(module, index);
        }
    }

    // The providers each file needs compiled before it.
    let mut imports_std = false;
    let mut needs: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); files.len()];
    for (index, file) in files.iter().enumerate() {
        for module in &file.deps.requires {
            match module.as_str() {
                STD_MODULE => imports_std = true,
                STD_COMPAT_MODULE => {
                    return Err(PlanError::StdCompat {
                        file: file.path.clone(),
                    });
                }
                _ => match provider_of.get(module.as_str()) {
                    Some(&provider) => {
                        needs[index].insert(provider);
                    }
                    None if available.contains(module) => {}
                    None => {
                        return Err(PlanError::UnknownModule {
                            target: target.to_string(),
                            file: file.path.clone(),
                            module: module.clone(),
                        });
                    }
                },
            }
        }
    }

    let mut steps = order_providers(target, files, &provider_of, &needs)?;
    steps.extend(
        files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.provides().is_none())
            .map(|(file, _)| Step {
                file,
                provides: None,
            }),
    );
    Ok(Plan { steps, imports_std })
}

/// Providers in dependency order, ties broken by path.
fn order_providers(
    target: &str,
    files: &[SourceFile],
    providers: &HashMap<&str, usize>,
    needs: &[BTreeSet<usize>],
) -> Result<Vec<Step>, PlanError> {
    let mut waiting: BTreeMap<usize, usize> = BTreeMap::new();
    let mut dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for &provider in providers.values() {
        waiting.insert(provider, needs[provider].len());
        for &needed in &needs[provider] {
            dependents.entry(needed).or_default().push(provider);
        }
    }

    let mut ready: BTreeSet<usize> = waiting
        .iter()
        .filter(|&(_, &count)| count == 0)
        .map(|(&file, _)| file)
        .collect();
    let mut steps = Vec::with_capacity(providers.len());
    while let Some(file) = ready.pop_first() {
        waiting.remove(&file);
        for dependent in dependents.get(&file).into_iter().flatten() {
            if let Some(count) = waiting.get_mut(dependent) {
                *count -= 1;
                if *count == 0 {
                    ready.insert(*dependent);
                }
            }
        }
        steps.push(Step {
            file,
            provides: files[file].provides().map(String::from),
        });
    }

    if let Some(&start) = waiting.keys().next() {
        // Every file still waiting needs another waiting file, so following those
        // needs from any of them must come back around.
        let mut path = vec![start];
        loop {
            let last = path[path.len() - 1];
            let Some(&next) = needs[last].iter().find(|n| waiting.contains_key(n)) else {
                break;
            };
            if let Some(seen) = path.iter().position(|&file| file == next) {
                path.drain(..seen);
                path.push(next);
                break;
            }
            path.push(next);
        }
        return Err(PlanError::Cycle {
            target: target.to_string(),
            modules: path
                .iter()
                .filter_map(|&file| files[file].provides().map(String::from))
                .collect(),
        });
    }
    Ok(steps)
}

/// A file that must be a module interface, by its extension.
#[must_use]
pub fn is_interface_file(path: &str) -> bool {
    has_extension(path, "cppm") || has_extension(path, "ixx")
}

/// Whether `path` has the extension `ext`, ignoring ASCII case.
#[must_use]
pub fn has_extension(path: &str, ext: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|actual| actual.eq_ignore_ascii_case(ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::scan::Provided;

    /// A file that provides `provides` (an interface, or an internal partition when
    /// it starts with `~`) and requires `requires`.
    fn file(path: &str, provides: Option<&str>, requires: &[&str]) -> SourceFile {
        SourceFile {
            path: path.into(),
            deps: Deps {
                provides: provides.map(|name| Provided {
                    name: name.trim_start_matches('~').into(),
                    is_interface: !name.starts_with('~'),
                }),
                requires: requires.iter().map(|r| (*r).to_string()).collect(),
            },
        }
    }

    fn order(files: &[SourceFile]) -> Vec<&str> {
        let plan = plan("lib target `t`", files, &HashSet::new()).unwrap();
        plan.steps
            .iter()
            .map(|step| files[step.file].path.as_str())
            .collect()
    }

    fn error(files: &[SourceFile]) -> String {
        plan("bin target `app`", files, &HashSet::new())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn interfaces_come_after_what_they_import_then_everything_else() {
        let files = [
            file("src/a.cpp", None, &["a", "std"]),
            file("src/a.cppm", Some("a"), &["b", "c"]),
            file("src/b.cppm", Some("b"), &["c"]),
            file("src/c.cppm", Some("c"), &[]),
            file("src/main.cpp", None, &["a"]),
            file("src/plain.cpp", None, &[]),
        ];
        assert_eq!(
            order(&files),
            [
                "src/c.cppm",
                "src/b.cppm",
                "src/a.cppm",
                "src/a.cpp",
                "src/main.cpp",
                "src/plain.cpp"
            ]
        );
        let plan = plan("lib target `t`", &files, &HashSet::new()).unwrap();
        assert!(plan.imports_std);
        assert_eq!(plan.steps[0].provides.as_deref(), Some("c"));
        assert_eq!(plan.steps[3].provides, None);
    }

    #[test]
    fn partitions_come_before_their_primary_interface() {
        let files = [
            file("m.cppm", Some("m"), &["m:api", "m:detail"]),
            file("m-api.cppm", Some("m:api"), &["m:detail"]),
            file("m-detail.cpp", Some("~m:detail"), &[]),
        ];
        assert_eq!(order(&files), ["m-detail.cpp", "m-api.cppm", "m.cppm"]);
    }

    #[test]
    fn available_modules_satisfy_imports() {
        let files = [file("src/main.cpp", None, &["core"])];
        let available = HashSet::from(["core".to_string()]);
        let plan = plan("bin target `app`", &files, &available).unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert!(!plan.imports_std);
    }

    #[test]
    fn unknown_modules_are_errors() {
        let files = [file("src/main.cpp", None, &["missing"])];
        assert_eq!(
            error(&files),
            "`src/main.cpp` imports module `missing`, but no source file of bin target \
             `app` declares it\n\nhelp: add the file that declares it to the target's `sources`"
        );
    }

    #[test]
    fn cycles_are_errors() {
        let files = [
            file("a.cppm", Some("a"), &["b"]),
            file("b.cppm", Some("b"), &["a"]),
            file("c.cppm", Some("c"), &[]),
        ];
        assert_eq!(
            error(&files),
            "modules in bin target `app` import each other in a cycle: `a` → `b` → `a`"
        );
        // A longer cycle, reached from a file outside it.
        let files = [
            file("a.cppm", Some("a"), &["b"]),
            file("b.cppm", Some("b"), &["c"]),
            file("c.cppm", Some("c"), &["d"]),
            file("d.cppm", Some("d"), &["b"]),
        ];
        assert_eq!(
            error(&files),
            "modules in bin target `app` import each other in a cycle: `b` → `c` → `d` → `b`"
        );
    }

    #[test]
    fn interface_files_must_declare_a_module() {
        let files = [file("src/lib.cppm", None, &[])];
        assert_eq!(
            error(&files),
            "`src/lib.cppm` has no `export module <name>;` declaration"
        );
    }

    #[test]
    fn duplicate_modules_are_errors() {
        let files = [
            file("a.cppm", Some("m"), &[]),
            file("b.cppm", Some("m"), &[]),
        ];
        assert_eq!(
            error(&files),
            "module `m` is declared by both `a.cppm` and `b.cppm`"
        );
    }

    #[test]
    fn std_compat_is_unsupported() {
        let files = [file("a.cpp", None, &["std.compat"])];
        assert!(error(&files).contains("imports `std.compat`"));
    }

    #[test]
    fn a_bin_has_one_main() {
        assert!(check_one_main("bin target `app`", vec!["src/main.cpp".into()]).is_ok());
        let err = check_one_main(
            "bin target `app`",
            vec!["src/main.cpp".into(), "src/other.cpp".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.starts_with(
                "bin target `app` has more than one `main()`: `src/main.cpp`, `src/other.cpp`"
            ),
            "{err}"
        );
    }
}
