use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::{InitArgs, Vcs};
use crate::style;

const MAIN_TEMPLATE: &str = r#"import std;

int main() {
    std::cout << "Hello, world!" << std::endl;
    return 0;
}
"#;

const MAIN_TEMPLATE_LEGACY: &str = r#"#include <iostream>

int main() {
    std::cout << "Hello, world!" << std::endl;
    return 0;
}
"#;

const LIB_TEMPLATE: &str = r#"int add(int left, int right) {
    return left + right;
}
"#;

pub fn run(args: InitArgs) -> Result<()> {
    if args.bin && args.lib {
        bail!("cannot specify both `--bin` and `--lib`");
    }

    let path = absolute_path(&args.path)?;
    fs::create_dir_all(&path)
        .with_context(|| format!("failed to create directory `{}`", path.display()))?;
    // Resolve `..`/`.` and symlinks now that the directory exists, so ancestor
    // checks (e.g. detecting an enclosing git repo) walk the real directory tree
    // instead of the literal, possibly relative, path components.
    let path = path
        .canonicalize()
        .with_context(|| format!("failed to resolve `{}`", path.display()))?;

    let manifest_path = path.join("Yam.toml");
    if manifest_path.exists() {
        bail!(
            "`yam init` cannot be run on existing yam packages\n\n`{}` already exists",
            manifest_path.display()
        );
    }

    let name = match &args.name {
        Some(name) => name.clone(),
        None => package_name_from_path(&path)?,
    };
    validate_package_name(&name)?;

    let is_lib = args.lib;
    write_manifest(&manifest_path, &name)?;

    let src_dir = path.join("src");
    fs::create_dir_all(&src_dir)
        .with_context(|| format!("failed to create directory `{}`", src_dir.display()))?;

    if is_lib {
        write_if_missing(&src_dir.join("lib.cpp"), LIB_TEMPLATE)?;
    } else {
        let template = if args.legacy {
            MAIN_TEMPLATE_LEGACY
        } else {
            MAIN_TEMPLATE
        };
        write_if_missing(&src_dir.join("main.cpp"), template)?;
    }

    // An explicit `--vcs git` forces initialization even inside an existing repo;
    // with no `--vcs` at all we auto-detect and skip if one already encloses `path`.
    let should_init_vcs = match args.vcs {
        Some(Vcs::None) => false,
        Some(Vcs::Git) => true,
        None => !is_inside_existing_vcs(&path),
    };
    if should_init_vcs {
        init_git(&path)?;
        write_gitignore(&path)?;
    }

    let kind = if is_lib {
        "library"
    } else {
        "binary (application)"
    };
    style::status("Created", format!("{kind} `{name}` package"));

    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()
            .context("failed to read current directory")?
            .join(path))
    }
}

fn package_name_from_path(path: &Path) -> Result<String> {
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        anyhow::anyhow!(
            "cannot infer package name from path `{}`; use --name",
            path.display()
        )
    })?;
    Ok(name.to_string())
}

fn validate_package_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let first = chars
        .next()
        .ok_or_else(|| anyhow::anyhow!("package name cannot be empty"))?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        bail!("invalid package name `{name}`: must start with a letter or underscore");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("invalid package name `{name}`: must be ASCII alphanumeric, `-`, or `_`");
    }
    Ok(())
}

fn write_manifest(manifest_path: &Path, name: &str) -> Result<()> {
    let contents = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nstd = \"c++20\"\n\n[dependencies]\n"
    );
    fs::write(manifest_path, contents)
        .with_context(|| format!("failed to write `{}`", manifest_path.display()))
}

fn write_if_missing(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, contents).with_context(|| format!("failed to write `{}`", path.display()))
}

fn is_inside_existing_vcs(path: &Path) -> bool {
    let mut current = Some(path.to_path_buf());
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return true;
        }
        current = dir.parent().map(Path::to_path_buf);
    }
    false
}

fn init_git(path: &Path) -> Result<()> {
    git2::Repository::init(path).with_context(|| {
        format!(
            "failed to initialize a git repository at `{}`",
            path.display()
        )
    })?;
    Ok(())
}

fn write_gitignore(path: &Path) -> Result<()> {
    let gitignore_path = path.join(".gitignore");
    let entry = "/target";

    if !gitignore_path.exists() {
        return fs::write(&gitignore_path, format!("{entry}\n"))
            .with_context(|| format!("failed to write `{}`", gitignore_path.display()));
    }

    let existing = fs::read_to_string(&gitignore_path)
        .with_context(|| format!("failed to read `{}`", gitignore_path.display()))?;
    if existing.lines().any(|line| line.trim() == entry) {
        return Ok(());
    }

    let mut new_contents = existing;
    if !new_contents.is_empty() && !new_contents.ends_with('\n') {
        new_contents.push('\n');
    }
    new_contents.push_str(entry);
    new_contents.push('\n');
    fs::write(&gitignore_path, new_contents)
        .with_context(|| format!("failed to write `{}`", gitignore_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn args(path: PathBuf, vcs: Option<Vcs>) -> InitArgs {
        InitArgs {
            path,
            bin: false,
            lib: false,
            name: None,
            legacy: false,
            vcs,
        }
    }

    fn has_git(dir: &Path) -> bool {
        dir.join(".git").exists()
    }

    fn has_gitignore(dir: &Path) -> bool {
        dir.join(".gitignore").exists()
    }

    /// Creates a directory that already has its own git repo, to stand in for
    /// "the target path is nested inside an existing repository".
    fn existing_repo(temp: &Path, name: &str) -> PathBuf {
        let repo = temp.join(name);
        fs::create_dir_all(&repo).unwrap();
        git2::Repository::init(&repo).unwrap();
        repo
    }

    #[test]
    fn fresh_dir_auto_detect_initializes_git() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("app");
        run(args(target.clone(), None)).unwrap();
        assert!(has_git(&target));
        assert!(has_gitignore(&target));
    }

    #[test]
    fn fresh_dir_explicit_vcs_git_initializes_git() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("app");
        run(args(target.clone(), Some(Vcs::Git))).unwrap();
        assert!(has_git(&target));
        assert!(has_gitignore(&target));
    }

    #[test]
    fn fresh_dir_vcs_none_skips_git() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("app");
        run(args(target.clone(), Some(Vcs::None))).unwrap();
        assert!(!has_git(&target));
        assert!(!has_gitignore(&target));
    }

    #[test]
    fn nested_dir_auto_detect_skips_git() {
        let temp = tempdir().unwrap();
        let parent = existing_repo(temp.path(), "parent");
        let target = parent.join("sub");
        run(args(target.clone(), None)).unwrap();
        assert!(!has_git(&target));
        assert!(!has_gitignore(&target));
    }

    #[test]
    fn nested_dir_explicit_vcs_git_forces_init() {
        let temp = tempdir().unwrap();
        let parent = existing_repo(temp.path(), "parent");
        let target = parent.join("sub");
        run(args(target.clone(), Some(Vcs::Git))).unwrap();
        assert!(has_git(&target));
        assert!(has_gitignore(&target));
    }

    #[test]
    fn nested_dir_vcs_none_skips_git() {
        let temp = tempdir().unwrap();
        let parent = existing_repo(temp.path(), "parent");
        let target = parent.join("sub");
        run(args(target.clone(), Some(Vcs::None))).unwrap();
        assert!(!has_git(&target));
        assert!(!has_gitignore(&target));
    }

    /// Regression test: a target reached via a literal `..` component (as
    /// `absolute_path()` produces for a relative `PATH` argument) must be
    /// resolved before checking ancestry, or a sibling directory gets
    /// mistaken for a descendant of the enclosing repo.
    #[test]
    fn dotdot_path_outside_enclosing_repo_still_initializes_git() {
        let temp = tempdir().unwrap();
        let repo_root = existing_repo(temp.path(), "reporoot");

        let dotdot_path = repo_root.join("..").join("foo");
        run(args(dotdot_path, None)).unwrap();

        let foo = temp.path().join("foo");
        assert!(has_git(&foo));
        assert!(has_gitignore(&foo));
    }
}
