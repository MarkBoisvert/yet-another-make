use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::CleanArgs;
use crate::manifest::MANIFEST_FILE_NAME;
use crate::style;

/// The build directory at the root of every project.
const TARGET_DIR: &str = "target";

/// The release profile's directory under `target/`.
const RELEASE_DIR: &str = "Release";

/// Remove a project's build output: all of `target/`, or only `target/Release` with
/// `--release`.
///
/// # Errors
///
/// Fails if `args.path` has no `Yam.toml`, or the build output can't be listed or
/// removed.
pub fn run(args: &CleanArgs) -> Result<()> {
    if !args.path.join(MANIFEST_FILE_NAME).is_file() {
        bail!(
            "could not find `{MANIFEST_FILE_NAME}` in `{}`",
            args.path.display()
        );
    }

    let mut dir = args.path.join(TARGET_DIR);
    if args.release {
        dir.push(RELEASE_DIR);
    }

    let files = list_files(&dir)?;
    if args.verbose {
        for file in &files {
            style::status("Removing", file.path.display());
        }
    }
    let bytes = files.iter().map(|file| file.len).sum();
    let summary = format!(
        "{} {}, {} total",
        files.len(),
        plural(files.len()),
        human_size(bytes)
    );

    if args.dry_run {
        style::status("Summary", summary);
        style::warning("no files deleted due to --dry-run");
        return Ok(());
    }

    remove(&dir)?;
    style::status("Removed", summary);
    Ok(())
}

struct File {
    path: PathBuf,
    len: u64,
}

/// Every non-directory entry under `dir`, without following symlinks. Empty if
/// `dir` doesn't exist.
fn list_files(dir: &Path) -> Result<Vec<File>> {
    if fs::symlink_metadata(dir).is_err() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.with_context(|| format!("failed to list `{}`", dir.display()))?;
        if entry.file_type().is_dir() {
            continue;
        }
        let len = entry
            .metadata()
            .with_context(|| format!("failed to read `{}`", entry.path().display()))?
            .len();
        files.push(File {
            path: entry.into_path(),
            len,
        });
    }
    Ok(files)
}

/// Remove `dir` and everything in it. A symlink is removed, not followed.
fn remove(dir: &Path) -> Result<()> {
    let result = match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(dir),
        Ok(_) => fs::remove_file(dir),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    };
    result.with_context(|| format!("failed to remove `{}`", dir.display()))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

/// `bytes` in binary units, e.g. `512B`, `1.5KiB`, `12.0MiB`.
#[allow(clippy::cast_precision_loss)] // Display only.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1}{}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn project(files: &[(&str, usize)]) -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILE_NAME),
            "[project]\nname = \"x\"\n",
        )
        .unwrap();
        for (file, len) in files {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, vec![0u8; *len]).unwrap();
        }
        dir
    }

    fn args(path: &Path) -> CleanArgs {
        CleanArgs {
            path: path.to_path_buf(),
            release: false,
            dry_run: false,
            verbose: false,
        }
    }

    const BUILD: [(&str, usize); 3] = [
        ("target/Debug/app", 10),
        ("target/Release/app", 20),
        ("target/Release/std/c++26/std.pcm", 30),
    ];

    #[test]
    fn removes_the_whole_target_dir() {
        let dir = project(&BUILD);
        run(&args(dir.path())).unwrap();
        assert!(!dir.path().join("target").exists());
        assert!(dir.path().join(MANIFEST_FILE_NAME).exists());
    }

    #[test]
    fn release_removes_only_release_artifacts() {
        let dir = project(&BUILD);
        let mut args = args(dir.path());
        args.release = true;
        run(&args).unwrap();
        assert!(dir.path().join("target/Debug/app").exists());
        assert!(!dir.path().join("target/Release").exists());
    }

    #[test]
    fn dry_run_removes_nothing() {
        let dir = project(&BUILD);
        let mut args = args(dir.path());
        args.dry_run = true;
        args.verbose = true;
        run(&args).unwrap();
        for (file, _) in BUILD {
            assert!(dir.path().join(file).exists(), "{file}");
        }
    }

    #[test]
    fn nothing_to_clean_is_fine() {
        let dir = project(&[]);
        run(&args(dir.path())).unwrap();
        let mut args = args(dir.path());
        args.release = true;
        run(&args).unwrap();
    }

    #[test]
    fn requires_a_project() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("target/Debug")).unwrap();
        let err = run(&args(dir.path())).unwrap_err();
        assert!(
            err.to_string().contains("could not find `Yam.toml`"),
            "{err}"
        );
        assert!(dir.path().join("target/Debug").exists());
    }

    #[test]
    fn lists_files_with_sizes() {
        let dir = project(&BUILD);
        let files = list_files(&dir.path().join("target")).unwrap();
        let listed: Vec<_> = files
            .iter()
            .map(|f| {
                (
                    f.path.strip_prefix(dir.path()).unwrap().to_path_buf(),
                    f.len,
                )
            })
            .collect();
        let expected: Vec<_> = BUILD
            .iter()
            .map(|(file, len)| (PathBuf::from(file), *len as u64))
            .collect();
        assert_eq!(listed, expected);
        assert_eq!(list_files(&dir.path().join("missing")).unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_removed_not_followed() {
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("keep"), "x").unwrap();
        let dir = project(&[]);
        std::os::unix::fs::symlink(outside.path(), dir.path().join("target")).unwrap();
        run(&args(dir.path())).unwrap();
        assert!(fs::symlink_metadata(dir.path().join("target")).is_err());
        assert!(outside.path().join("keep").exists());
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(1023), "1023B");
        assert_eq!(human_size(1024), "1.0KiB");
        assert_eq!(human_size(1536), "1.5KiB");
        assert_eq!(human_size(12 * 1024 * 1024), "12.0MiB");
        assert_eq!(human_size(3 << 30), "3.0GiB");
    }
}
