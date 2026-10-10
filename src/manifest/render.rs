//! Write a [`Manifest`] back out as `Yam.toml` text.
//!
//! The layout is fixed so generated manifests read the same everywhere: `[project]`
//! first, then `[lib]`, each `[[bin]]`, and `[dependencies]`. Strings are always
//! single-line basic strings. Unused keys aren't written back.

use std::fmt::Write as _;
use std::path::Path;

use super::{DependencySpec, Manifest, ManifestError, TargetSpec};

impl Manifest {
    /// Render the manifest as `Yam.toml` text. Parsing the result gives back an equal
    /// manifest, apart from [`Manifest::unused_keys`].
    #[must_use]
    pub fn to_toml_string(&self) -> String {
        let mut out = String::new();

        out.push_str("[project]\n");
        field(&mut out, "name", &self.project.name);
        field(&mut out, "version", &self.project.version);
        field(&mut out, "std", self.project.std.as_str());

        if let Some(lib) = &self.lib {
            out.push_str("\n[lib]\n");
            target(&mut out, lib);
        }
        for bin in &self.bins {
            out.push_str("\n[[bin]]\n");
            target(&mut out, bin);
        }

        out.push_str("\n[dependencies]\n");
        for (name, dep) in &self.dependencies {
            let _ = writeln!(out, "{} = {}", key(name), dependency(dep));
        }
        out
    }

    /// Write the manifest to `path`.
    ///
    /// # Errors
    ///
    /// Fails if the file can't be written.
    pub fn write(&self, path: &Path) -> Result<(), ManifestError> {
        std::fs::write(path, self.to_toml_string()).map_err(|source| ManifestError::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

fn target(out: &mut String, spec: &TargetSpec) {
    if let Some(name) = &spec.name {
        field(out, "name", name);
    }
    if let Some(path) = &spec.path {
        field(out, "path", &path.to_string_lossy());
    }
    if let Some(sources) = &spec.sources {
        list(out, "sources", sources.iter().map(String::as_str));
    }
    if !spec.exclude.is_empty() {
        list(out, "exclude", spec.exclude.iter().map(String::as_str));
    }
    if !spec.include_dirs.is_empty() {
        let dirs: Vec<_> = spec
            .include_dirs
            .iter()
            .map(|d| d.to_string_lossy())
            .collect();
        list(out, "include-dirs", dirs.iter().map(AsRef::as_ref));
    }
}

/// `name = "1.0"` when only a version is given, otherwise an inline table.
fn dependency(dep: &DependencySpec) -> String {
    if let (Some(version), None, None, None) = (&dep.version, &dep.path, &dep.git, &dep.rev) {
        return string(version);
    }
    let path = dep.path.as_ref().map(|p| p.to_string_lossy());
    let fields: Vec<String> = [
        ("version", dep.version.as_deref()),
        ("path", path.as_deref()),
        ("git", dep.git.as_deref()),
        ("rev", dep.rev.as_deref()),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.map(|v| format!("{k} = {}", string(v))))
    .collect();
    if fields.is_empty() {
        "{}".to_string()
    } else {
        format!("{{ {} }}", fields.join(", "))
    }
}

fn field(out: &mut String, name: &str, value: &str) {
    let _ = writeln!(out, "{name} = {}", string(value));
}

fn list<'a>(out: &mut String, name: &str, values: impl Iterator<Item = &'a str>) {
    let values: Vec<String> = values.map(string).collect();
    let _ = writeln!(out, "{name} = [{}]", values.join(", "));
}

/// A single-line TOML basic string. Single-line so it's also valid as a quoted key
/// (TOML doesn't allow multi-line strings there).
fn string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Other control characters (and DEL) must be escaped in basic strings.
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A bare key when possible, otherwise a quoted one.
fn key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare { name.to_string() } else { string(name) }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use proptest::prelude::*;

    use super::*;
    use crate::manifest::{CppStd, Project};

    fn round_trip(manifest: &Manifest) -> Manifest {
        let text = manifest.to_toml_string();
        Manifest::from_toml_str(&text).unwrap_or_else(|err| panic!("{err}\n---\n{text}"))
    }

    #[test]
    fn renders_a_minimal_manifest() {
        let manifest = Manifest::from_toml_str("[project]\nname = \"app\"\n").unwrap();
        assert_eq!(
            manifest.to_toml_string(),
            "[project]\nname = \"app\"\nversion = \"0.1.0\"\nstd = \"c++26\"\n\n[dependencies]\n"
        );
    }

    #[test]
    fn renders_every_section_in_order() {
        let text = r#"
            [dependencies]
            fmt = "11.0"
            local = { path = "../local" }

            [[bin]]
            name = "tool"
            path = "tools/tool.cpp"

            [project]
            name = "port"
            version = "1.2.3"
            std = "c++17"

            [lib]
            sources = ["lib/**/*.cpp"]
            exclude = ["lib/test/**"]
            include-dirs = ["include"]
        "#;
        let manifest = Manifest::from_toml_str(text).unwrap();
        assert_eq!(
            manifest.to_toml_string(),
            "[project]\nname = \"port\"\nversion = \"1.2.3\"\nstd = \"c++17\"\n\
             \n[lib]\nsources = [\"lib/**/*.cpp\"]\nexclude = [\"lib/test/**\"]\ninclude-dirs = [\"include\"]\n\
             \n[[bin]]\nname = \"tool\"\npath = \"tools/tool.cpp\"\n\
             \n[dependencies]\nfmt = \"11.0\"\nlocal = { path = \"../local\" }\n"
        );
        assert_eq!(round_trip(&manifest), manifest);
    }

    #[test]
    fn empty_lib_table_is_kept() {
        let manifest = Manifest::from_toml_str("[project]\nname = \"x\"\n[lib]\n").unwrap();
        assert!(manifest.to_toml_string().contains("\n[lib]\n"));
        assert_eq!(round_trip(&manifest), manifest);
    }

    #[test]
    fn unused_keys_are_not_written_back() {
        let manifest =
            Manifest::from_toml_str("[project]\nname = \"x\"\nauthor = \"me\"\n").unwrap();
        assert_eq!(manifest.unused_keys, ["project.author"]);
        assert!(!manifest.to_toml_string().contains("author"));
        assert_eq!(round_trip(&manifest).unused_keys, Vec::<String>::new());
    }

    #[test]
    fn write_saves_the_rendered_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Yam.toml");
        let manifest = Manifest::from_toml_str("[project]\nname = \"x\"\n").unwrap();
        manifest.write(&path).unwrap();
        assert_eq!(Manifest::from_path(&path).unwrap(), manifest);
    }

    #[test]
    fn strings_and_keys_are_single_line_and_escaped() {
        assert_eq!(string("plain"), "\"plain\"");
        assert_eq!(string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(string("line\nnext\ttab"), "\"line\\nnext\\ttab\"");
        assert_eq!(string("bell\u{7}del\u{7f}"), "\"bell\\u0007del\\u007F\"");
        assert_eq!(key("fmt"), "fmt");
        assert_eq!(key("my-dep_2"), "my-dep_2");
        assert_eq!(key("'\""), "\"'\\\"\"");
        assert_eq!(key(""), "\"\"");
    }

    // --- property test -----------------------------------------------------------

    /// Any string, including quotes, backslashes, newlines and non-ASCII.
    fn text() -> impl Strategy<Value = String> {
        prop_oneof![
            "[a-zA-Z0-9_./*-]{0,12}",
            any::<String>().prop_map(|s| s.chars().take(16).collect()),
        ]
    }

    fn opt_text() -> impl Strategy<Value = Option<String>> {
        prop::option::of(text())
    }

    fn texts() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(text(), 0..4)
    }

    fn target_spec() -> impl Strategy<Value = TargetSpec> {
        (
            opt_text(),
            opt_text(),
            prop::option::of(texts()),
            texts(),
            texts(),
        )
            .prop_map(|(name, path, sources, exclude, include_dirs)| TargetSpec {
                name,
                path: path.map(PathBuf::from),
                sources,
                exclude,
                include_dirs: include_dirs.into_iter().map(PathBuf::from).collect(),
            })
    }

    fn dependency_spec() -> impl Strategy<Value = DependencySpec> {
        (opt_text(), opt_text(), opt_text(), opt_text()).prop_map(|(version, path, git, rev)| {
            DependencySpec {
                version,
                path: path.map(PathBuf::from),
                git,
                rev,
            }
        })
    }

    fn manifest() -> impl Strategy<Value = Manifest> {
        (
            text(),
            text(),
            prop::sample::select(CppStd::ALL.to_vec()),
            prop::option::of(target_spec()),
            prop::collection::vec(
                target_spec().prop_map(|mut spec| {
                    spec.name.get_or_insert_with(|| "bin".to_string());
                    spec
                }),
                0..3,
            ),
            prop::collection::btree_map(text(), dependency_spec(), 0..4),
        )
            .prop_map(|(name, version, std, lib, bins, dependencies)| Manifest {
                project: Project { name, version, std },
                lib,
                bins,
                dependencies: dependencies.into_iter().collect::<BTreeMap<_, _>>(),
                unused_keys: Vec::new(),
            })
    }

    proptest! {
        #[test]
        fn render_then_parse_round_trips(manifest in manifest()) {
            prop_assert_eq!(round_trip(&manifest), manifest);
        }
    }
}
