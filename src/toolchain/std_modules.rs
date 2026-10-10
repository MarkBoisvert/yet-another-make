//! libc++'s `std` and `std.compat` module sources, found through the
//! `libc++.modules.json` manifest that `clang++ -print-file-name` reports.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::{Tool, ToolchainError};

const MANIFEST_NAME: &str = "libc++.modules.json";

/// The located module sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdModules {
    /// The `libc++.modules.json` they came from.
    pub manifest: PathBuf,
    pub std: ModuleSource,
    pub std_compat: Option<ModuleSource>,
}

/// One module's interface source and the system include directories it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleSource {
    pub source: PathBuf,
    pub system_include_dirs: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct Manifest {
    modules: Vec<Module>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Module {
    logical_name: String,
    source_path: PathBuf,
    #[serde(default)]
    local_arguments: LocalArguments,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct LocalArguments {
    #[serde(default)]
    system_include_directories: Vec<PathBuf>,
}

/// Ask `cxx` where libc++'s module manifest is, then read it.
pub(super) fn locate(cxx: &Tool) -> Result<StdModules, ToolchainError> {
    let none = || ToolchainError::NoStdModules {
        cxx: cxx.path.clone(),
        major: cxx.version.major,
    };
    let output = Command::new(&cxx.path)
        .args([
            "-stdlib=libc++",
            &format!("-print-file-name={MANIFEST_NAME}"),
        ])
        .output()
        .map_err(|_| none())?;
    let reported = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    // `<prefix>/bin/clang++` → `<prefix>/lib`.
    let install_lib = fs::canonicalize(&cxx.path)
        .ok()
        .and_then(|real| Some(real.parent()?.parent()?.join("lib")));
    // Clang echoes the bare name back when it can't find the file. Some installs keep
    // it where the driver doesn't look, e.g. Homebrew's `lib/c++/`.
    let manifest = (output.status.success() && reported.is_absolute() && reported.is_file())
        .then_some(reported)
        .or_else(|| install_lib.as_deref().and_then(find_in_lib))
        .ok_or_else(none)?;
    from_manifest(&manifest, install_lib.as_deref())
}

/// `lib/libc++.modules.json`, or the first `lib/*/libc++.modules.json` (Homebrew's
/// `lib/c++/`, or a per-triple runtime directory such as `lib/x86_64-unknown-linux-gnu/`).
fn find_in_lib(lib: &Path) -> Option<PathBuf> {
    let direct = lib.join(MANIFEST_NAME);
    if direct.is_file() {
        return Some(direct);
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(lib)
        .ok()?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|dir| dir.join(MANIFEST_NAME))
        .find(|path| path.is_file())
}

/// Read the manifest at `path`.
///
/// Its paths are relative, but not always to the file clang reports: Debian's LLVM
/// packages install the real file in `/usr/lib/<triple>/` with paths relative to a
/// symlink to it in `/usr/lib/llvm-22/lib/`, and clang reports the former. So the
/// paths are tried against the reported directory, the file's real directory, and
/// the toolchain's own `lib/` (`install_lib`), and the first that has the `std`
/// source wins.
pub(super) fn from_manifest(
    path: &Path,
    install_lib: Option<&Path>,
) -> Result<StdModules, ToolchainError> {
    let error = |reason: String| ToolchainError::StdManifest {
        manifest: path.to_path_buf(),
        reason,
    };
    let text = fs::read_to_string(path).map_err(|err| error(err.to_string()))?;
    let manifest: Manifest = serde_json::from_str(&text).map_err(|err| error(err.to_string()))?;
    let module = |name: &str| manifest.modules.iter().find(|m| m.logical_name == name);
    let std = module("std").ok_or_else(|| error("it has no `std` module".into()))?;

    let mut bases: Vec<PathBuf> = Vec::new();
    let real_dir = fs::canonicalize(path)
        .ok()
        .and_then(|real| real.parent().map(Path::to_path_buf));
    for base in [
        path.parent().map(Path::to_path_buf),
        real_dir,
        install_lib.map(Path::to_path_buf),
    ]
    .into_iter()
    .flatten()
    {
        if !bases.contains(&base) {
            bases.push(base);
        }
    }
    let base = bases
        .into_iter()
        .find(|base| normalize(&base.join(&std.source_path)).is_file())
        .ok_or_else(|| ToolchainError::StdSourceMissing {
            manifest: path.to_path_buf(),
            source_path: std.source_path.clone(),
        })?;

    let resolve = |module: &Module| ModuleSource {
        source: normalize(&base.join(&module.source_path)),
        system_include_dirs: module
            .local_arguments
            .system_include_directories
            .iter()
            .map(|dir| normalize(&base.join(dir)))
            .collect(),
    };
    Ok(StdModules {
        manifest: path.to_path_buf(),
        std: resolve(std),
        std_compat: module("std.compat").map(resolve),
    })
}

/// Remove `.` and `..` segments without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir
                if matches!(out.components().next_back(), Some(Component::Normal(_))) =>
            {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const MANIFEST: &str = r#"{
      "version": 1,
      "revision": 1,
      "modules": [
        {
          "logical-name": "std",
          "source-path": "../share/libc++/v1/std.cppm",
          "is-std-library": true,
          "local-arguments": { "system-include-directories": ["../share/libc++/v1"] }
        },
        {
          "logical-name": "std.compat",
          "source-path": "../share/libc++/v1/std.compat.cppm",
          "is-std-library": true,
          "local-arguments": { "system-include-directories": ["../share/libc++/v1"] }
        }
      ]
    }"#;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn resolves_paths_relative_to_the_manifest() {
        let temp = tempdir().unwrap();
        let prefix = temp.path().join("llvm");
        let manifest = prefix.join("lib/libc++.modules.json");
        write(&manifest, MANIFEST);
        write(&prefix.join("share/libc++/v1/std.cppm"), "");
        let found = from_manifest(&manifest, None).unwrap();
        let v1 = prefix.join("share").join("libc++").join("v1");
        assert_eq!(
            found.std,
            ModuleSource {
                source: v1.join("std.cppm"),
                system_include_dirs: vec![v1.clone()],
            }
        );
        assert_eq!(found.std_compat.unwrap().source, v1.join("std.compat.cppm"));
    }

    /// Debian: clang reports `/usr/lib/<triple>/libc++.modules.json`, whose paths are
    /// relative to `/usr/lib/llvm-22/lib/`.
    #[test]
    fn falls_back_to_the_toolchain_lib_dir() {
        let temp = tempdir().unwrap();
        let usr_lib = temp.path().join("usr/lib");
        let manifest = usr_lib.join("x86_64-linux-gnu/libc++.modules.json");
        write(&manifest, MANIFEST);
        write(&usr_lib.join("llvm-22/share/libc++/v1/std.cppm"), "");
        let llvm_lib = usr_lib.join("llvm-22/lib");

        let err = from_manifest(&manifest, None).unwrap_err().to_string();
        assert!(err.contains("which doesn't exist"), "{err}");

        let found = from_manifest(&manifest, Some(&llvm_lib)).unwrap();
        assert_eq!(
            found.std.source,
            usr_lib
                .join("llvm-22")
                .join("share")
                .join("libc++")
                .join("v1")
                .join("std.cppm")
        );
    }

    #[test]
    fn rejects_a_manifest_without_std() {
        let temp = tempdir().unwrap();
        let manifest = temp.path().join("libc++.modules.json");
        write(&manifest, r#"{"version":1,"revision":1,"modules":[]}"#);
        let err = from_manifest(&manifest, None).unwrap_err().to_string();
        assert!(err.ends_with("it has no `std` module"), "{err}");

        write(&manifest, "not json");
        assert!(from_manifest(&manifest, None).is_err());
    }

    #[test]
    fn finds_the_manifest_under_the_toolchain_lib_dir() {
        let temp = tempdir().unwrap();
        let lib = temp.path().join("lib");
        fs::create_dir_all(lib.join("clang")).unwrap();
        assert_eq!(find_in_lib(&lib), None);

        write(&lib.join("c++").join(MANIFEST_NAME), MANIFEST);
        assert_eq!(find_in_lib(&lib), Some(lib.join("c++").join(MANIFEST_NAME)));

        write(&lib.join(MANIFEST_NAME), MANIFEST);
        assert_eq!(find_in_lib(&lib), Some(lib.join(MANIFEST_NAME)));
    }

    #[test]
    fn normalize_removes_dot_segments() {
        assert_eq!(
            normalize(Path::new("/usr/lib/llvm-22/lib/../share/./libc++")),
            Path::new("/usr/lib/llvm-22/share/libc++")
        );
        assert_eq!(normalize(Path::new("../a/../b")), Path::new("../b"));
    }
}
