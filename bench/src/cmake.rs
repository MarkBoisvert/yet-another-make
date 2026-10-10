//! Configuring the CMake + ninja side of a benchmark project with the same compiler
//! and libc++ that yam uses.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use yet_another_make::toolchain::{ModuleSource, Toolchain};

use crate::project::{self, Style};

/// The build directory, next to yam's `target/`.
pub const BUILD_DIR: &str = "build-cmake";

/// The `CMAKE_EXPERIMENTAL_CXX_IMPORT_STD` value for each CMake version, from that
/// version's `Help/dev/experimental.rst`.
const IMPORT_STD_GATES: [(&str, &str); 7] = [
    ("3.30", "0e5b6991-d74f-4b3d-a41c-cf096e0b2508"),
    ("3.31", "0e5b6991-d74f-4b3d-a41c-cf096e0b2508"),
    ("4.0", "a9e1cf81-9932-4810-974b-6eccaf14e457"),
    ("4.1", "d0edc3af-4c50-42ea-a356-e2862fe7a444"),
    ("4.2", "d0edc3af-4c50-42ea-a356-e2862fe7a444"),
    ("4.3", "451f2fe2-a8a2-47c3-bc32-94786d8fc91b"),
    ("4.4", "f35a9ac6-8463-4d38-8eec-5d6008153e7d"),
];

/// Configure `dir/build-cmake` with ninja.
///
/// # Errors
///
/// Fails if the project, toolchain or CMake can't be found, the CMake version has no
/// known `import std` gate (for module projects, unless `gate` is given), or
/// configuring fails.
pub fn configure(dir: &Path, release: bool, gate: Option<&str>) -> Result<PathBuf> {
    let meta = project::load(dir)?;
    let toolchain = Toolchain::discover()?;
    let build = dir.join(BUILD_DIR);

    let mut command = Command::new("cmake");
    command
        .arg("-S")
        .arg(dir)
        .arg("-B")
        .arg(&build)
        .args(["-G", "Ninja", "-Wno-dev"])
        .arg(format!(
            "-DCMAKE_BUILD_TYPE={}",
            if release { "Release" } else { "Debug" }
        ))
        .arg(format!(
            "-DCMAKE_CXX_COMPILER={}",
            toolchain.cxx.path.display()
        ))
        .arg("-DCMAKE_CXX_FLAGS=-stdlib=libc++");

    // Link the libc++ next to the modules manifest, as `yam build` does.
    if let Some(libcxx) = fs::canonicalize(&toolchain.std_modules.manifest)
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    {
        let flags = if cfg!(target_os = "macos") {
            format!("-L{0} -Wl,-rpath,{0}", libcxx.display())
        } else {
            format!("-L{}", libcxx.display())
        };
        command.arg(format!("-DCMAKE_EXE_LINKER_FLAGS={flags}"));
    }

    if meta.style == Style::Modules {
        let gate = match gate {
            Some(gate) => gate.to_string(),
            None => import_std_gate(&cmake_version()?)?.to_string(),
        };
        // CMake resolves the manifest's relative paths against the file it's given,
        // which breaks on Debian's layout, so hand it one with absolute paths.
        let json = dir.join(".bench").join("libc++.modules.json");
        fs::create_dir_all(json.parent().unwrap_or(dir))?;
        fs::write(&json, modules_json(&toolchain))
            .with_context(|| format!("failed to write `{}`", json.display()))?;
        command
            .arg(format!("-DCMAKE_EXPERIMENTAL_CXX_IMPORT_STD={gate}"))
            .arg(format!(
                "-DCMAKE_CXX_STDLIB_MODULES_JSON={}",
                json.display()
            ));
    }

    let status = command
        .status()
        .context("failed to run `cmake`; is it installed?")?;
    if !status.success() {
        bail!("cmake configure failed for `{}`", dir.display());
    }
    Ok(build)
}

/// `major.minor` from `cmake --version`.
fn cmake_version() -> Result<String> {
    let output = Command::new("cmake")
        .arg("--version")
        .output()
        .context("failed to run `cmake --version`; is CMake installed?")?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_cmake_version(&text)
        .with_context(|| format!("unrecognized `cmake --version` output: {text}"))
}

fn parse_cmake_version(text: &str) -> Option<String> {
    let version = text.lines().next()?.strip_prefix("cmake version ")?;
    let mut parts = version.split('.');
    Some(format!("{}.{}", parts.next()?, parts.next()?))
}

fn import_std_gate(version: &str) -> Result<&'static str> {
    IMPORT_STD_GATES
        .iter()
        .find(|(v, _)| *v == version)
        .map(|(_, gate)| *gate)
        .with_context(|| {
            format!(
                "no known `import std` gate for CMake {version}\n\nhelp: find \
                 CMAKE_EXPERIMENTAL_CXX_IMPORT_STD in that version's \
                 Help/dev/experimental.rst and pass it with --import-std-gate"
            )
        })
}

/// A `libc++.modules.json` with absolute paths.
fn modules_json(toolchain: &Toolchain) -> String {
    let entry = |name: &str, module: &ModuleSource| {
        serde_json::json!({
            "logical-name": name,
            "source-path": module.source,
            "is-std-library": true,
            "local-arguments": { "system-include-directories": module.system_include_dirs },
        })
    };
    let modules = &toolchain.std_modules;
    let mut list = vec![entry("std", &modules.std)];
    if let Some(compat) = &modules.std_compat {
        list.push(entry("std.compat", compat));
    }
    serde_json::to_string_pretty(&serde_json::json!({
        "version": 1,
        "revision": 1,
        "modules": list,
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Meta;

    #[test]
    fn parses_cmake_versions() {
        assert_eq!(
            parse_cmake_version("cmake version 4.4.3\n\nCMake suite maintained…"),
            Some("4.4".into())
        );
        assert_eq!(
            parse_cmake_version("cmake version 3.31.6-msvc6"),
            Some("3.31".into())
        );
        assert_eq!(parse_cmake_version("ninja 1.13"), None);
    }

    #[test]
    fn knows_the_gate_per_version() {
        assert_eq!(
            import_std_gate("4.4").unwrap(),
            "f35a9ac6-8463-4d38-8eec-5d6008153e7d"
        );
        let err = import_std_gate("3.28").unwrap_err().to_string();
        assert!(err.contains("--import-std-gate"), "{err}");
    }

    /// Generates a small project in each style and builds it with CMake + ninja. CI
    /// installs both on Linux and macOS, so there a failure fails the test; elsewhere
    /// missing tools skip it.
    #[test]
    fn generated_projects_build_with_cmake() {
        let have = |tool: &str| Command::new(tool).arg("--version").output().is_ok();
        let required = std::env::var_os("CI").is_some() && !cfg!(windows);
        if !(have("cmake") && have("ninja") && Toolchain::discover().is_ok()) {
            assert!(!required, "CI needs cmake, ninja and the toolchain");
            eprintln!("skipping: cmake, ninja or the toolchain is missing");
            return;
        }
        for style in [Style::Modules, Style::Legacy] {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path();
            project::generate(dir, &Meta::new(style, 10, 3, 2, 1)).unwrap();
            let build_dir = configure(dir, false, None).unwrap();
            let built = Command::new("cmake")
                .arg("--build")
                .arg(&build_dir)
                .status()
                .unwrap();
            assert!(built.success(), "{style:?} build failed");
            let exe = build_dir.join(format!("bench{}", std::env::consts::EXE_SUFFIX));
            let output = Command::new(exe).output().unwrap();
            assert!(output.status.success(), "{style:?} binary failed");
        }
    }
}
