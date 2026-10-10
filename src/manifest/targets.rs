//! Turn a manifest plus the project's files into concrete build targets, applying
//! Cargo-style conventions where the manifest doesn't say otherwise:
//!
//! * `src/main.cpp` is a binary named after the project.
//! * `src/lib.cppm` (or `src/lib.cpp`) is the project library.
//! * each `src/bin/<name>.cpp` is an extra binary named `<name>`.
//!
//! `[lib]` and `[[bin]]` tables override names, entry files and source sets.

use std::path::{Path, PathBuf};

use super::{Manifest, ManifestError, TargetSpec};

/// Library entry files, in order of preference.
const LIB_ENTRIES: [&str; 2] = ["src/lib.cppm", "src/lib.cpp"];
const MAIN_ENTRY: &str = "src/main.cpp";
const BIN_DIR: &str = "src/bin";

/// The library's conventional source set: everything under `src/`, minus the files
/// that belong to binaries.
const LIB_SOURCES: [&str; 2] = ["src/**/*.cppm", "src/**/*.cpp"];
const LIB_EXCLUDES: [&str; 2] = ["src/main.cpp", "src/bin/**"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Lib,
    Bin,
}

/// A fully resolved build target. Paths and globs are relative to the project root,
/// with `/` separators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub kind: TargetKind,
    pub entry: PathBuf,
    /// Globs selecting the target's sources. Expanded by the build engine.
    pub sources: Vec<String>,
    pub exclude: Vec<String>,
    pub include_dirs: Vec<PathBuf>,
    /// For binaries: whether the project library is linked in.
    pub links_lib: bool,
}

/// Resolve the project's targets: the library first (if any), then binaries in
/// manifest order, then binaries discovered by convention, sorted by name.
///
/// Doesn't check that explicit paths exist or that names are unique; manifest
/// validation reports those.
///
/// # Errors
///
/// Fails if `src/bin` can't be listed, or if the project has no targets at all.
pub fn resolve_targets(manifest: &Manifest, root: &Path) -> Result<Vec<Target>, ManifestError> {
    let project_name = &manifest.project.name;
    let mut targets = Vec::new();

    let lib = resolve_lib(manifest.lib.as_ref(), project_name, root);
    let has_lib = lib.is_some();
    targets.extend(lib);

    for spec in &manifest.bins {
        // `Manifest::from_toml_str` guarantees every `[[bin]]` has a name.
        let name = spec.name.clone().unwrap_or_default();
        let default_entry = if name == *project_name {
            PathBuf::from(MAIN_ENTRY)
        } else {
            Path::new(BIN_DIR).join(format!("{name}.cpp"))
        };
        targets.push(bin_target(name, spec, default_entry, has_lib));
    }

    for (name, entry) in discover_bins(project_name, root)? {
        let claimed = manifest.bins.iter().any(|spec| {
            spec.name.as_deref() == Some(name.as_str()) || spec.path.as_deref() == Some(&entry)
        });
        if !claimed {
            targets.push(bin_target(name, &TargetSpec::default(), entry, has_lib));
        }
    }

    if targets.is_empty() {
        return Err(ManifestError::NoTargets {
            root: root.to_path_buf(),
        });
    }
    Ok(targets)
}

fn resolve_lib(spec: Option<&TargetSpec>, project_name: &str, root: &Path) -> Option<Target> {
    let found_entry = LIB_ENTRIES
        .iter()
        .map(PathBuf::from)
        .find(|entry| root.join(entry).is_file());

    // With no `[lib]` table, a library exists only if its entry file does.
    if spec.is_none() && found_entry.is_none() {
        return None;
    }
    let spec = spec.cloned().unwrap_or_default();

    let entry = spec
        .path
        .or(found_entry)
        .unwrap_or_else(|| PathBuf::from(LIB_ENTRIES[0]));
    let (sources, exclude) = match spec.sources {
        Some(sources) => (sources, spec.exclude),
        None => (
            LIB_SOURCES.map(String::from).to_vec(),
            LIB_EXCLUDES
                .map(String::from)
                .into_iter()
                .chain(spec.exclude)
                .collect(),
        ),
    };

    Some(Target {
        name: spec.name.unwrap_or_else(|| project_name.to_string()),
        kind: TargetKind::Lib,
        entry,
        sources,
        exclude,
        include_dirs: spec.include_dirs,
        links_lib: false,
    })
}

fn bin_target(name: String, spec: &TargetSpec, default_entry: PathBuf, links_lib: bool) -> Target {
    let entry = spec.path.clone().unwrap_or(default_entry);
    let sources = spec
        .sources
        .clone()
        .unwrap_or_else(|| vec![glob_path(&entry)]);
    Target {
        name,
        kind: TargetKind::Bin,
        entry,
        sources,
        exclude: spec.exclude.clone(),
        include_dirs: spec.include_dirs.clone(),
        links_lib,
    }
}

/// Binaries present by convention: `src/main.cpp`, then `src/bin/*.cpp` by name.
fn discover_bins(project_name: &str, root: &Path) -> Result<Vec<(String, PathBuf)>, ManifestError> {
    let mut bins = Vec::new();
    if root.join(MAIN_ENTRY).is_file() {
        bins.push((project_name.to_string(), PathBuf::from(MAIN_ENTRY)));
    }

    let bin_dir = root.join(BIN_DIR);
    if bin_dir.is_dir() {
        let list_err = |source| ManifestError::ListDir {
            path: bin_dir.clone(),
            source,
        };
        let mut extra = Vec::new();
        for entry in std::fs::read_dir(&bin_dir).map_err(list_err)? {
            let path = entry.map_err(list_err)?.path();
            if path.is_file()
                && path.extension().is_some_and(|ext| ext == "cpp")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            {
                extra.push((
                    stem.to_string(),
                    Path::new(BIN_DIR).join(format!("{stem}.cpp")),
                ));
            }
        }
        extra.sort();
        bins.extend(extra);
    }
    Ok(bins)
}

/// A relative path as a glob with `/` separators. Glob metacharacters in file names
/// are escaped so the path matches only itself.
fn glob_path(path: &Path) -> String {
    let mut glob = String::new();
    for (i, component) in path.components().enumerate() {
        if i > 0 {
            glob.push('/');
        }
        for c in component.as_os_str().to_string_lossy().chars() {
            if matches!(c, '*' | '?' | '[' | ']' | '{' | '}') {
                glob.push('\\');
            }
            glob.push(c);
        }
    }
    glob
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        dir
    }

    fn manifest(extra: &str) -> Manifest {
        Manifest::from_toml_str(&format!("[project]\nname = \"pkg\"\n{extra}")).unwrap()
    }

    fn resolve(extra: &str, files: &[&str]) -> Vec<Target> {
        let dir = project(files);
        resolve_targets(&manifest(extra), dir.path()).unwrap()
    }

    fn names(targets: &[Target]) -> Vec<(&str, TargetKind)> {
        targets.iter().map(|t| (t.name.as_str(), t.kind)).collect()
    }

    #[test]
    fn main_cpp_is_a_bin_named_after_the_project() {
        let targets = resolve("", &["src/main.cpp"]);
        assert_eq!(
            targets,
            vec![Target {
                name: "pkg".into(),
                kind: TargetKind::Bin,
                entry: "src/main.cpp".into(),
                sources: vec!["src/main.cpp".into()],
                exclude: vec![],
                include_dirs: vec![],
                links_lib: false,
            }]
        );
    }

    #[test]
    fn lib_cppm_is_the_project_library() {
        let targets = resolve("", &["src/lib.cppm", "src/detail/impl.cpp"]);
        assert_eq!(
            targets,
            vec![Target {
                name: "pkg".into(),
                kind: TargetKind::Lib,
                entry: "src/lib.cppm".into(),
                sources: vec!["src/**/*.cppm".into(), "src/**/*.cpp".into()],
                exclude: vec!["src/main.cpp".into(), "src/bin/**".into()],
                include_dirs: vec![],
                links_lib: false,
            }]
        );
    }

    #[test]
    fn legacy_lib_cpp_is_used_when_there_is_no_lib_cppm() {
        let targets = resolve("", &["src/lib.cpp"]);
        assert_eq!(targets[0].entry, PathBuf::from("src/lib.cpp"));
        let targets = resolve("", &["src/lib.cpp", "src/lib.cppm"]);
        assert_eq!(targets[0].entry, PathBuf::from("src/lib.cppm"));
    }

    #[test]
    fn lib_comes_first_and_bins_link_it() {
        let targets = resolve(
            "",
            &[
                "src/main.cpp",
                "src/lib.cppm",
                "src/bin/zeta.cpp",
                "src/bin/alpha.cpp",
            ],
        );
        assert_eq!(
            names(&targets),
            vec![
                ("pkg", TargetKind::Lib),
                ("pkg", TargetKind::Bin),
                ("alpha", TargetKind::Bin),
                ("zeta", TargetKind::Bin),
            ]
        );
        assert!(targets[1..].iter().all(|t| t.links_lib));
        assert_eq!(targets[2].entry, PathBuf::from("src/bin/alpha.cpp"));
    }

    #[test]
    fn non_cpp_files_in_src_bin_are_ignored() {
        let targets = resolve(
            "",
            &[
                "src/bin/tool.cpp",
                "src/bin/README.md",
                "src/bin/helper.hpp",
            ],
        );
        assert_eq!(names(&targets), vec![("tool", TargetKind::Bin)]);
    }

    #[test]
    fn explicit_bin_replaces_the_conventional_one_with_the_same_name() {
        let targets = resolve(
            "[[bin]]\nname = \"pkg\"\nsources = [\"app/**/*.cpp\"]\n",
            &["src/main.cpp"],
        );
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].entry, PathBuf::from("src/main.cpp"));
        assert_eq!(targets[0].sources, vec!["app/**/*.cpp".to_string()]);
    }

    #[test]
    fn explicit_bin_replaces_the_conventional_one_with_the_same_path() {
        let targets = resolve(
            "[[bin]]\nname = \"renamed\"\npath = \"src/bin/tool.cpp\"\n",
            &["src/bin/tool.cpp"],
        );
        assert_eq!(names(&targets), vec![("renamed", TargetKind::Bin)]);
    }

    #[test]
    fn explicit_bin_entry_defaults_by_name() {
        let targets = resolve("[[bin]]\nname = \"tool\"\n", &[]);
        assert_eq!(targets[0].entry, PathBuf::from("src/bin/tool.cpp"));
        let targets = resolve("[[bin]]\nname = \"pkg\"\n", &[]);
        assert_eq!(targets[0].entry, PathBuf::from("src/main.cpp"));
    }

    #[test]
    fn lib_sources_replace_the_convention_and_keep_only_user_excludes() {
        let targets = resolve(
            "[lib]\npath = \"lib/core.cpp\"\nsources = [\"lib/**/*.cpp\"]\nexclude = [\"lib/test/**\"]\ninclude-dirs = [\"include\"]\n",
            &[],
        );
        let lib = &targets[0];
        assert_eq!(lib.entry, PathBuf::from("lib/core.cpp"));
        assert_eq!(lib.sources, vec!["lib/**/*.cpp".to_string()]);
        assert_eq!(lib.exclude, vec!["lib/test/**".to_string()]);
        assert_eq!(lib.include_dirs, vec![PathBuf::from("include")]);
    }

    #[test]
    fn lib_excludes_add_to_the_convention_when_sources_are_default() {
        let targets = resolve(
            "[lib]\nexclude = [\"src/experimental/**\"]\n",
            &["src/lib.cppm"],
        );
        assert_eq!(
            targets[0].exclude,
            vec![
                "src/main.cpp".to_string(),
                "src/bin/**".to_string(),
                "src/experimental/**".to_string()
            ]
        );
    }

    #[test]
    fn lib_table_without_entry_file_still_declares_a_lib() {
        let targets = resolve("[lib]\nname = \"core\"\n", &[]);
        assert_eq!(names(&targets), vec![("core", TargetKind::Lib)]);
        assert_eq!(targets[0].entry, PathBuf::from("src/lib.cppm"));
    }

    #[test]
    fn no_targets_is_an_error() {
        let dir = project(&["src/notes.txt"]);
        let err = resolve_targets(&manifest(""), dir.path()).unwrap_err();
        assert!(matches!(err, ManifestError::NoTargets { .. }), "{err:?}");
    }

    #[test]
    fn glob_path_uses_forward_slashes_and_escapes_metacharacters() {
        assert_eq!(glob_path(Path::new("src/bin/tool.cpp")), "src/bin/tool.cpp");
        assert_eq!(glob_path(Path::new("src/odd[1].cpp")), "src/odd\\[1\\].cpp");
    }
}
