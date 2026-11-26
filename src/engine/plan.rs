//! The compile order for one target's source files.
//!
//! Files that provide a module (interfaces and partitions) are compiled first, each
//! after the modules it imports, so its compiled interface (`.pcm`) exists when an
//! importer needs it. Every other file comes after them, in path order. The build
//! graph (#24) and the parallel scheduler (#25) replace this ordering.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::hash::BuildHasher;
use std::path::Path;

use super::scan::{Import, ModuleDecl, Scan};

/// The `std` module, which the build compiles from libc++ itself.
pub const STD_MODULE: &str = "std";
const STD_COMPAT_MODULE: &str = "std.compat";

/// One of a target's source files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFile {
    /// Relative to the project root, with `/` separators.
    pub path: String,
    pub scan: Scan,
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

    #[error("modules in {target} import each other in a cycle: {}", quoted(.modules))]
    Cycle {
        target: String,
        modules: Vec<String>,
    },

    #[error(
        "`{file}` imports a header unit, which yam doesn't support yet\n\n\
         help: use `#include` for headers"
    )]
    HeaderUnit { file: String },

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

/// Order `files`, which must be sorted by path.
///
/// `target` names the target in messages, `is_bin` turns on the one-`main()` check,
/// and `available` holds the modules already built for it (the project library's,
/// for a bin that links it).
///
/// # Errors
///
/// Fails if a `.cppm` file declares no module, two files declare the same module, a
/// file imports a module nothing provides, interfaces import each other in a cycle,
/// a file imports a header unit or `std.compat`, or a bin defines `main()` twice.
pub fn plan<S: BuildHasher>(
    target: &str,
    files: &[SourceFile],
    is_bin: bool,
    available: &HashSet<String, S>,
) -> Result<Plan, PlanError> {
    if is_bin {
        let mains: Vec<String> = files
            .iter()
            .filter(|f| f.scan.defines_main)
            .map(|f| f.path.clone())
            .collect();
        if mains.len() > 1 {
            return Err(PlanError::TwoMains {
                target: target.to_string(),
                files: mains,
            });
        }
    }

    let mut provider_of: HashMap<&str, usize> = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        let provides = file.scan.module.as_ref().and_then(ModuleDecl::provides);
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
        let implemented = match &file.scan.module {
            Some(ModuleDecl::Implementation(module)) => Some(module.as_str()),
            _ => None,
        };
        let imported = file.scan.imports.iter().map(|import| match import {
            Import::Module(module) => Ok(module.as_str()),
            Import::HeaderUnit => Err(PlanError::HeaderUnit {
                file: file.path.clone(),
            }),
        });
        for module in implemented.into_iter().map(Ok).chain(imported) {
            let module = module?;
            match module {
                STD_MODULE => imports_std = true,
                STD_COMPAT_MODULE => {
                    return Err(PlanError::StdCompat {
                        file: file.path.clone(),
                    });
                }
                _ => match provider_of.get(module) {
                    Some(&provider) => {
                        needs[index].insert(provider);
                    }
                    None if available.contains(module) => {}
                    None => {
                        return Err(PlanError::UnknownModule {
                            target: target.to_string(),
                            file: file.path.clone(),
                            module: module.to_string(),
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
            .filter(|(_, f)| {
                f.scan
                    .module
                    .as_ref()
                    .and_then(ModuleDecl::provides)
                    .is_none()
            })
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
            provides: files[file]
                .scan
                .module
                .as_ref()
                .and_then(ModuleDecl::provides)
                .map(String::from),
        });
    }

    if !waiting.is_empty() {
        let mut modules: Vec<String> = waiting
            .keys()
            .filter_map(|&file| {
                files[file]
                    .scan
                    .module
                    .as_ref()?
                    .provides()
                    .map(String::from)
            })
            .collect();
        modules.sort();
        return Err(PlanError::Cycle {
            target: target.to_string(),
            modules,
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

    fn file(path: &str, text: &str) -> SourceFile {
        SourceFile {
            path: path.into(),
            scan: Scan::of(text),
        }
    }

    fn order(files: &[SourceFile]) -> Vec<&str> {
        let plan = plan("lib target `t`", files, false, &HashSet::new()).unwrap();
        plan.steps
            .iter()
            .map(|step| files[step.file].path.as_str())
            .collect()
    }

    fn error(files: &[SourceFile], is_bin: bool) -> String {
        plan("bin target `app`", files, is_bin, &HashSet::new())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn interfaces_come_after_what_they_import_then_everything_else() {
        let files = [
            file("src/a.cpp", "module a;\nimport std;\n"),
            file("src/a.cppm", "export module a;\nimport b;\nimport c;\n"),
            file("src/b.cppm", "export module b;\nimport c;\n"),
            file("src/c.cppm", "export module c;\n"),
            file("src/main.cpp", "import a;\nint main() {}\n"),
            file("src/plain.cpp", "#include <cstdio>\n"),
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
        let plan = plan("lib target `t`", &files, false, &HashSet::new()).unwrap();
        assert!(plan.imports_std);
        assert_eq!(plan.steps[0].provides.as_deref(), Some("c"));
        assert_eq!(plan.steps[3].provides, None);
    }

    #[test]
    fn partitions_come_before_their_primary_interface() {
        let files = [
            file(
                "m.cppm",
                "export module m;\nexport import :api;\nimport :detail;\n",
            ),
            file("m-api.cppm", "export module m:api;\nimport :detail;\n"),
            file("m-detail.cpp", "module m:detail;\n"),
        ];
        assert_eq!(order(&files), ["m-detail.cpp", "m-api.cppm", "m.cppm"]);
    }

    #[test]
    fn available_modules_satisfy_imports() {
        let files = [file("src/main.cpp", "import core;\nint main() {}\n")];
        let available = HashSet::from(["core".to_string()]);
        let plan = plan("bin target `app`", &files, true, &available).unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert!(!plan.imports_std);
    }

    #[test]
    fn unknown_modules_are_errors() {
        let files = [file("src/main.cpp", "import missing;\nint main() {}\n")];
        assert_eq!(
            error(&files, true),
            "`src/main.cpp` imports module `missing`, but no source file of bin target \
             `app` declares it\n\nhelp: add the file that declares it to the target's `sources`"
        );
        let files = [file("src/x.cpp", "module gone;\n")];
        assert!(error(&files, false).contains("imports module `gone`"));
    }

    #[test]
    fn cycles_are_errors() {
        let files = [
            file("a.cppm", "export module a;\nimport b;\n"),
            file("b.cppm", "export module b;\nimport a;\n"),
            file("c.cppm", "export module c;\n"),
        ];
        assert_eq!(
            error(&files, false),
            "modules in bin target `app` import each other in a cycle: `a`, `b`"
        );
    }

    #[test]
    fn interface_files_must_declare_a_module() {
        let files = [file("src/lib.cppm", "int x;\n")];
        assert_eq!(
            error(&files, false),
            "`src/lib.cppm` has no `export module <name>;` declaration"
        );
    }

    #[test]
    fn duplicate_modules_are_errors() {
        let files = [
            file("a.cppm", "export module m;\n"),
            file("b.cppm", "export module m;\n"),
        ];
        assert_eq!(
            error(&files, false),
            "module `m` is declared by both `a.cppm` and `b.cppm`"
        );
    }

    #[test]
    fn two_mains_in_a_bin_are_an_error() {
        let files = [
            file("src/main.cpp", "int main() {}\n"),
            file("src/other.cpp", "int main(int, char**) { return 0; }\n"),
        ];
        assert!(
            error(&files, true).starts_with(
                "bin target `app` has more than one `main()`: `src/main.cpp`, `src/other.cpp`"
            ),
            "{}",
            error(&files, true)
        );
        // A lib isn't checked.
        assert_eq!(order(&files).len(), 2);
    }

    #[test]
    fn header_units_and_std_compat_are_unsupported() {
        let files = [file("a.cpp", "import <vector>;\n")];
        assert!(error(&files, false).contains("imports a header unit"));
        let files = [file("a.cpp", "import std.compat;\n")];
        assert!(error(&files, false).contains("imports `std.compat`"));
    }
}
