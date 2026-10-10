//! Semantic checks on a parsed manifest against the project's files.
//!
//! Parsing (`Manifest::from_toml_str`) rejects what can't be represented at all.
//! Validation reports everything else as [`Diagnostic`]s, so one run shows every
//! problem instead of stopping at the first.

use std::collections::HashSet;
use std::path::Path;

use super::sources::{GlobList, SourceTree, slash_path, stays_inside_root};
use super::{Manifest, ManifestError, Target, TargetKind, TargetSpec, resolve_targets};
use crate::style;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// One validation finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Where in the manifest the problem is, e.g. `project.name`, `bin[1].sources[0]`,
    /// or a file path for a conventional target with no manifest entry.
    pub key: String,
    pub message: String,
    pub help: Option<String>,
}

impl Diagnostic {
    fn error(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            key: key.into(),
            message: message.into(),
            help: None,
        }
    }

    fn warning(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(key, message)
        }
    }

    fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

/// Whether any diagnostic is an error.
#[must_use]
pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(|d| d.severity == Severity::Error)
}

/// Print diagnostics to stderr as `error:`/`warning:` lines, each followed by its
/// `help:` line if it has one.
pub fn report(diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        match diagnostic.severity {
            Severity::Error => style::error(&diagnostic.message),
            Severity::Warning => style::warning(&diagnostic.message),
        }
        if let Some(help) = &diagnostic.help {
            style::help(help);
        }
    }
}

/// The naming rule for projects and targets: an ASCII letter or `_`, then ASCII
/// letters, digits, `-` or `_`. Names become file names and identifiers, so this keeps
/// them portable.
///
/// # Errors
///
/// Returns why `name` is invalid, as a message that names it as a `kind` name.
pub fn check_name(kind: &str, name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(format!("{kind} name must not be empty"));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!(
            "invalid {kind} name '{name}': must start with a letter or '_'"
        ));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!(
            "invalid {kind} name '{name}': use only letters, digits, '-' or '_'"
        ));
    }
    Ok(())
}

/// Validate `manifest` against the files under `root`.
///
/// # Errors
///
/// Fails only if the project tree can't be read. Problems with the manifest itself are
/// returned as diagnostics.
pub fn validate(manifest: &Manifest, root: &Path) -> Result<Vec<Diagnostic>, ManifestError> {
    let mut out = Vec::new();

    check_project(manifest, &mut out);
    let specs = specs_with_keys(manifest);
    for (key, spec) in &specs {
        check_spec(key, spec, root, &mut out);
    }
    check_duplicate_bins(manifest, &mut out);
    check_dependencies(manifest, &mut out);

    match resolve_targets(manifest, root) {
        Ok(targets) => check_files(manifest, &targets, root, &mut out)?,
        Err(err @ ManifestError::NoTargets { .. }) => {
            out.push(Diagnostic::error("project", err.to_string()));
        }
        Err(err) => return Err(err),
    }

    Ok(out)
}

fn check_project(manifest: &Manifest, out: &mut Vec<Diagnostic>) {
    if let Err(message) = check_name("project", &manifest.project.name) {
        out.push(Diagnostic::error("project.name", message));
    }
    if manifest.project.version.trim().is_empty() {
        out.push(Diagnostic::error(
            "project.version",
            "'project.version' must not be empty",
        ));
    }
    for key in &manifest.unused_keys {
        out.push(
            Diagnostic::warning(key.clone(), format!("unused manifest key '{key}'"))
                .with_help("check the spelling; yam ignores keys it doesn't know"),
        );
    }
}

/// `[lib]` as `lib` and each `[[bin]]` as `bin[i]`.
fn specs_with_keys(manifest: &Manifest) -> Vec<(String, &TargetSpec)> {
    manifest
        .lib
        .iter()
        .map(|spec| ("lib".to_string(), spec))
        .chain(
            manifest
                .bins
                .iter()
                .enumerate()
                .map(|(i, spec)| (format!("bin[{i}]"), spec)),
        )
        .collect()
}

fn check_spec(key: &str, spec: &TargetSpec, root: &Path, out: &mut Vec<Diagnostic>) {
    let kind = if key == "lib" { "lib" } else { "bin" };
    if let Some(name) = &spec.name
        && let Err(message) = check_name(kind, name)
    {
        out.push(Diagnostic::error(format!("{key}.name"), message));
    }
    if let Some(path) = &spec.path
        && !stays_inside_root(path)
    {
        out.push(Diagnostic::error(
            format!("{key}.path"),
            format!(
                "'{key}.path' = '{}' must be relative and inside the project root",
                path.display()
            ),
        ));
    }
    for (field, globs) in [
        ("sources", spec.sources.as_deref()),
        ("exclude", Some(&spec.exclude[..])),
    ] {
        if let Some(globs) = globs
            && let Err(err) = GlobList::new(globs)
        {
            out.push(Diagnostic::error(
                format!("{key}.{field}[{}]", err.index),
                format!("'{key}.{field}[{}]': {err}", err.index),
            ));
        }
    }
    for (i, dir) in spec.include_dirs.iter().enumerate() {
        let dir_key = format!("{key}.include-dirs[{i}]");
        if !stays_inside_root(dir) {
            out.push(Diagnostic::error(
                dir_key.clone(),
                format!(
                    "'{dir_key}' = '{}' must be relative and inside the project root",
                    dir.display()
                ),
            ));
        } else if !root.join(dir).is_dir() {
            out.push(Diagnostic::warning(
                dir_key.clone(),
                format!("'{dir_key}': directory '{}' does not exist", dir.display()),
            ));
        }
    }
}

fn check_duplicate_bins(manifest: &Manifest, out: &mut Vec<Diagnostic>) {
    let mut seen = HashSet::new();
    for (i, spec) in manifest.bins.iter().enumerate() {
        if let Some(name) = &spec.name
            && !seen.insert(name)
        {
            out.push(Diagnostic::error(
                format!("bin[{i}].name"),
                format!("duplicate bin name '{name}'"),
            ));
        }
    }
}

fn check_dependencies(manifest: &Manifest, out: &mut Vec<Diagnostic>) {
    for (name, dep) in &manifest.dependencies {
        let key = format!("dependencies.{name}");
        if dep.version.is_none() && dep.path.is_none() && dep.git.is_none() {
            out.push(
                Diagnostic::error(
                    key.clone(),
                    format!("dependency '{name}' must specify one of: version, path, git"),
                )
                .with_help(format!(
                    "e.g. {name} = \"1.0\" or {name} = {{ path = \"../{name}\" }}"
                )),
            );
        }
        if dep.rev.is_some() && dep.git.is_none() {
            out.push(Diagnostic::warning(
                key,
                format!("dependency '{name}': 'rev' has no effect without 'git'"),
            ));
        }
    }
}

/// Checks that need the project's files: entry files exist and are part of their
/// source sets, user globs match something, and no file is in both the library and a
/// binary.
fn check_files(
    manifest: &Manifest,
    targets: &[Target],
    root: &Path,
    out: &mut Vec<Diagnostic>,
) -> Result<(), ManifestError> {
    let tree = SourceTree::scan(root)?;
    let mut lib_files: Option<HashSet<String>> = None;
    let mut bin_files = Vec::new();

    for target in targets {
        let spec_key = spec_key_for(manifest, target);
        let entry = slash_path(&target.entry);
        let key = spec_key.clone().unwrap_or_else(|| entry.clone());

        if !root.join(&target.entry).is_file() {
            let mut diagnostic = Diagnostic::error(
                format!("{key}.path"),
                format!("{} entry file '{entry}' does not exist", describe(target)),
            );
            if target.kind == TargetKind::Lib
                && manifest.lib.as_ref().is_some_and(|l| l.path.is_none())
            {
                diagnostic = diagnostic.with_help("create it, or set 'path' in [lib]");
            }
            out.push(diagnostic);
        }

        // Invalid globs were already reported by `check_spec`.
        let (Ok(include), Ok(exclude)) = (
            GlobList::new(&target.sources),
            GlobList::new(&target.exclude),
        ) else {
            continue;
        };

        // Only user-written globs are expected to match: the conventional library
        // globs cover both `.cppm` and `.cpp`, and a project may use just one.
        if let Some(spec_key) = &spec_key {
            let spec = spec_for(manifest, spec_key);
            if spec.and_then(|s| s.sources.as_ref()).is_some() {
                warn_unmatched(
                    &tree,
                    &include,
                    &target.sources,
                    &format!("{spec_key}.sources"),
                    out,
                );
            }
            if let Some(spec) = spec {
                let user_exclude: Vec<String> = spec.exclude.clone();
                if let Ok(user_exclude_globs) = GlobList::new(&user_exclude) {
                    warn_unmatched(
                        &tree,
                        &user_exclude_globs,
                        &user_exclude,
                        &format!("{spec_key}.exclude"),
                        out,
                    );
                }
            }
        }

        let selected: HashSet<String> = tree
            .select(&include, &exclude)
            .into_iter()
            .map(String::from)
            .collect();
        if root.join(&target.entry).is_file() && !selected.contains(&entry) {
            out.push(
                Diagnostic::error(
                    format!("{key}.sources"),
                    format!(
                        "{} entry file '{entry}' is not part of its sources",
                        describe(target)
                    ),
                )
                .with_help("add it to 'sources', or remove it from 'exclude'"),
            );
        }

        match target.kind {
            TargetKind::Lib => lib_files = Some(selected),
            TargetKind::Bin => bin_files.push((target, key, selected)),
        }
    }

    if let Some(lib_files) = &lib_files {
        check_lib_overlap(lib_files, &bin_files, out);
    }
    Ok(())
}

/// A file in the library's source set must not also be in a bin's: the bin links the
/// library, so the file would be compiled and linked twice. Bins may share files.
fn check_lib_overlap(
    lib_files: &HashSet<String>,
    bin_files: &[(&Target, String, HashSet<String>)],
    out: &mut Vec<Diagnostic>,
) {
    for (target, key, files) in bin_files {
        let mut shared: Vec<&String> = files.intersection(lib_files).collect();
        if shared.is_empty() {
            continue;
        }
        shared.sort();
        let examples = shared
            .iter()
            .take(3)
            .map(|file| format!("'{file}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let more = if shared.len() > 3 {
            format!(" and {} more", shared.len() - 3)
        } else {
            String::new()
        };
        out.push(
            Diagnostic::error(
                format!("{key}.sources"),
                format!(
                    "{} and the library both include {examples}{more}",
                    describe(target)
                ),
            )
            .with_help(
                "the bin already links the library; remove these files from the bin's \
                 sources or add them to the library's exclude",
            ),
        );
    }
}

fn warn_unmatched(
    tree: &SourceTree,
    globs: &GlobList,
    patterns: &[String],
    key: &str,
    out: &mut Vec<Diagnostic>,
) {
    for index in tree.unmatched(globs) {
        out.push(Diagnostic::warning(
            format!("{key}[{index}]"),
            format!("'{key}[{index}]' = '{}' matches no files", patterns[index]),
        ));
    }
}

/// The manifest key (`lib` or `bin[i]`) of the table that declared `target`, if any.
fn spec_key_for(manifest: &Manifest, target: &Target) -> Option<String> {
    match target.kind {
        TargetKind::Lib => manifest.lib.as_ref().map(|_| "lib".to_string()),
        TargetKind::Bin => manifest
            .bins
            .iter()
            .position(|spec| spec.name.as_deref() == Some(target.name.as_str()))
            .map(|i| format!("bin[{i}]")),
    }
}

fn spec_for<'m>(manifest: &'m Manifest, key: &str) -> Option<&'m TargetSpec> {
    if key == "lib" {
        return manifest.lib.as_ref();
    }
    let index: usize = key.strip_prefix("bin[")?.strip_suffix(']')?.parse().ok()?;
    manifest.bins.get(index)
}

fn describe(target: &Target) -> String {
    match target.kind {
        TargetKind::Lib => format!("lib '{}'", target.name),
        TargetKind::Bin => format!("bin '{}'", target.name),
    }
}

/// Paths the tests build projects from.
#[cfg(test)]
fn write_files(root: &Path, files: &[&str]) {
    for file in files {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(manifest: &str, files: &[&str]) -> Vec<Diagnostic> {
        let dir = tempfile::tempdir().unwrap();
        write_files(dir.path(), files);
        let manifest = Manifest::from_toml_str(manifest).unwrap();
        validate(&manifest, dir.path()).unwrap()
    }

    fn keys(diagnostics: &[Diagnostic]) -> Vec<(Severity, &str)> {
        diagnostics
            .iter()
            .map(|d| (d.severity, d.key.as_str()))
            .collect()
    }

    const BASIC: &str = "[project]\nname = \"app\"\n";

    #[test]
    fn a_conventional_project_is_clean() {
        let diagnostics = run(
            BASIC,
            &[
                "src/main.cpp",
                "src/lib.cppm",
                "src/util.cpp",
                "src/bin/tool.cpp",
            ],
        );
        assert_eq!(diagnostics, vec![]);
    }

    #[test]
    fn name_rule() {
        assert_eq!(check_name("project", "my-app_2"), Ok(()));
        assert_eq!(check_name("project", "_x"), Ok(()));
        assert!(
            check_name("project", "")
                .unwrap_err()
                .contains("must not be empty")
        );
        assert!(
            check_name("project", "2app")
                .unwrap_err()
                .contains("must start with")
        );
        assert!(
            check_name("bin", "a.b")
                .unwrap_err()
                .contains("invalid bin name 'a.b'")
        );
    }

    #[test]
    fn invalid_project_name_and_empty_version() {
        let diagnostics = run(
            "[project]\nname = \"9lives\"\nversion = \"\"\n",
            &["src/main.cpp"],
        );
        assert_eq!(
            keys(&diagnostics),
            [
                (Severity::Error, "project.name"),
                (Severity::Error, "project.version")
            ]
        );
    }

    #[test]
    fn unused_keys_are_warnings() {
        let diagnostics = run(
            "[project]\nname = \"app\"\nauthor = \"me\"\n[lib]\ninlcude-dirs = [\"include\"]\n[tools]\nx = 1\n",
            &["src/main.cpp", "src/lib.cppm"],
        );
        assert_eq!(
            keys(&diagnostics),
            [
                (Severity::Warning, "lib.inlcude-dirs"),
                (Severity::Warning, "project.author"),
                (Severity::Warning, "tools"),
            ]
        );
        assert_eq!(
            diagnostics[0].message,
            "unused manifest key 'lib.inlcude-dirs'"
        );
    }

    #[test]
    fn paths_and_globs_must_stay_inside_the_root() {
        let diagnostics = run(
            "[project]\nname = \"app\"\n[[bin]]\nname = \"app\"\npath = \"../main.cpp\"\nsources = [\"../shared/*.cpp\"]\ninclude-dirs = [\"/usr/include\"]\n",
            &[],
        );
        let keys = keys(&diagnostics);
        assert!(keys.contains(&(Severity::Error, "bin[0].path")), "{keys:?}");
        assert!(
            keys.contains(&(Severity::Error, "bin[0].sources[0]")),
            "{keys:?}"
        );
        assert!(
            keys.contains(&(Severity::Error, "bin[0].include-dirs[0]")),
            "{keys:?}"
        );
    }

    #[test]
    fn missing_include_dir_is_a_warning() {
        let diagnostics = run(
            "[project]\nname = \"app\"\n[lib]\ninclude-dirs = [\"include\", \"src\"]\n",
            &["src/lib.cppm"],
        );
        assert_eq!(
            keys(&diagnostics),
            [(Severity::Warning, "lib.include-dirs[0]")]
        );
    }

    #[test]
    fn malformed_glob_is_an_error() {
        let diagnostics = run(
            "[project]\nname = \"app\"\n[lib]\nsources = [\"src/*.cpp\", \"src/[x.cpp\"]\n",
            &["src/lib.cppm"],
        );
        assert_eq!(keys(&diagnostics)[0], (Severity::Error, "lib.sources[1]"));
    }

    #[test]
    fn duplicate_bin_names() {
        let diagnostics = run(
            "[project]\nname = \"app\"\n[[bin]]\nname = \"tool\"\n[[bin]]\nname = \"tool\"\n",
            &["src/bin/tool.cpp"],
        );
        assert!(keys(&diagnostics).contains(&(Severity::Error, "bin[1].name")));
    }

    #[test]
    fn missing_entry_file() {
        let diagnostics = run("[project]\nname = \"app\"\n[lib]\n", &["src/main.cpp"]);
        assert_eq!(keys(&diagnostics), [(Severity::Error, "lib.path")]);
        assert_eq!(
            diagnostics[0].help.as_deref(),
            Some("create it, or set 'path' in [lib]")
        );
    }

    #[test]
    fn entry_must_be_in_its_sources() {
        let diagnostics = run(
            "[project]\nname = \"app\"\n[[bin]]\nname = \"app\"\nsources = [\"app/**/*.cpp\"]\n",
            &["src/main.cpp", "app/util.cpp"],
        );
        assert_eq!(keys(&diagnostics), [(Severity::Error, "bin[0].sources")]);
    }

    #[test]
    fn user_globs_that_match_nothing_warn() {
        let diagnostics = run(
            "[project]\nname = \"app\"\n[lib]\npath = \"lib/a.cpp\"\nsources = [\"lib/*.cpp\", \"lib/*.cppm\"]\nexclude = [\"lib/gone/**\"]\n",
            &["lib/a.cpp"],
        );
        assert_eq!(
            keys(&diagnostics),
            [
                (Severity::Warning, "lib.sources[1]"),
                (Severity::Warning, "lib.exclude[0]")
            ]
        );
    }

    #[test]
    fn conventional_lib_globs_may_match_nothing() {
        // `src/**/*.cppm` matches nothing in a header-based project; that's fine.
        assert_eq!(run(BASIC, &["src/lib.cpp"]), vec![]);
    }

    #[test]
    fn a_file_in_both_lib_and_bin_is_an_error() {
        let diagnostics = run(
            "[project]\nname = \"app\"\n[[bin]]\nname = \"app\"\nsources = [\"src/**/*.cpp\"]\n",
            &["src/main.cpp", "src/lib.cpp", "src/util.cpp"],
        );
        assert_eq!(keys(&diagnostics), [(Severity::Error, "bin[0].sources")]);
        assert_eq!(
            diagnostics[0].message,
            "bin 'app' and the library both include 'src/lib.cpp', 'src/util.cpp'"
        );
    }

    #[test]
    fn two_bins_may_share_a_file() {
        let diagnostics = run(
            "[project]\nname = \"app\"\n[[bin]]\nname = \"a\"\npath = \"tools/a.cpp\"\nsources = [\"tools/a.cpp\", \"tools/common.cpp\"]\n[[bin]]\nname = \"b\"\npath = \"tools/b.cpp\"\nsources = [\"tools/b.cpp\", \"tools/common.cpp\"]\n",
            &["tools/a.cpp", "tools/b.cpp", "tools/common.cpp"],
        );
        assert_eq!(diagnostics, vec![]);
    }

    #[test]
    fn dependency_rules() {
        let diagnostics = run(
            "[project]\nname = \"app\"\n[dependencies]\nempty = {}\npinned = { version = \"1.0\", rev = \"abc\" }\nok = \"1.0\"\n",
            &["src/main.cpp"],
        );
        assert_eq!(
            keys(&diagnostics),
            [
                (Severity::Error, "dependencies.empty"),
                (Severity::Warning, "dependencies.pinned"),
            ]
        );
    }

    #[test]
    fn no_targets_is_an_error() {
        let diagnostics = run(BASIC, &["README.md"]);
        assert_eq!(keys(&diagnostics), [(Severity::Error, "project")]);
    }

    #[test]
    fn has_errors_ignores_warnings() {
        assert!(!has_errors(&[Diagnostic::warning("k", "m")]));
        assert!(has_errors(&[
            Diagnostic::warning("k", "m"),
            Diagnostic::error("k", "m")
        ]));
    }
}
