//! The `Yam.toml` manifest: its data model, parsing, and target resolution.

mod cpp_std;
mod render;
mod sources;
mod targets;
mod validate;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub use cpp_std::{CppStd, ParseCppStdError};
pub use sources::{GlobError, GlobList, SourceTree};
pub use targets::{Target, TargetKind, resolve_targets};
pub use validate::{Diagnostic, Severity, check_name, has_errors, report, validate};

/// The manifest's file name, at the root of every project.
pub const MANIFEST_FILE_NAME: &str = "Yam.toml";

/// The `version` used when a manifest doesn't specify one.
pub const DEFAULT_VERSION: &str = "0.1.0";

/// A parsed `Yam.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub project: Project,
    /// The `[lib]` table, if present. A library can also exist purely by
    /// convention (see [`resolve_targets`]).
    pub lib: Option<TargetSpec>,
    /// The `[[bin]]` tables, in manifest order. More binaries can be discovered by
    /// convention (see [`resolve_targets`]).
    pub bins: Vec<TargetSpec>,
    /// Parsed but reserved: dependencies have no effect until package support lands.
    pub dependencies: BTreeMap<String, DependencySpec>,
    /// Keys in the file that yam doesn't recognize, as dotted paths (e.g.
    /// `lib.inlcude-dirs`). [`validate`] reports them as warnings.
    pub unused_keys: Vec<String>,
}

/// The `[project]` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub version: String,
    /// The minimum C++ standard the project's code and public interface need.
    pub std: CppStd,
}

/// A `[lib]` or `[[bin]]` table. Every field is optional except a binary's `name`;
/// [`resolve_targets`] fills in the conventional defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TargetSpec {
    pub name: Option<String>,
    /// The entry file: `main()` for a binary, the primary interface for a library.
    pub path: Option<PathBuf>,
    /// Globs relative to the project root. When set, they replace the conventional
    /// source set entirely.
    pub sources: Option<Vec<String>>,
    /// Globs relative to the project root, removed from the source set.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Header search directories, relative to the project root.
    #[serde(default)]
    pub include_dirs: Vec<PathBuf>,
}

/// One `[dependencies]` entry. `foo = "1.2"` is shorthand for `foo = { version = "1.2" }`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct DependencySpec {
    pub version: Option<String>,
    pub path: Option<PathBuf>,
    pub git: Option<String>,
    pub rev: Option<String>,
}

/// Why a manifest couldn't be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("could not find '{MANIFEST_FILE_NAME}' in '{}'", dir.display())]
    NotFound { dir: PathBuf },

    #[error("failed to read '{}'", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to write '{}'", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(transparent)]
    Syntax(#[from] toml::de::Error),

    #[error("missing required field '{0}'")]
    MissingField(String),

    #[error("invalid 'project.std': {0}")]
    InvalidStd(#[from] ParseCppStdError),

    #[error(
        "no targets found in '{}': add src/main.cpp, src/lib.cppm or src/lib.cpp, \
         or declare [lib] or [[bin]] in {MANIFEST_FILE_NAME}",
        root.display()
    )]
    NoTargets { root: PathBuf },

    #[error("failed to list '{}'", path.display())]
    ListDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl Manifest {
    /// Parse manifest text.
    ///
    /// # Errors
    ///
    /// Fails on invalid TOML, a missing `project.name` or `[[bin]]` `name`, or an
    /// unknown `project.std`.
    pub fn from_toml_str(text: &str) -> Result<Self, ManifestError> {
        let mut unused_keys = Vec::new();
        let raw: RawManifest =
            serde_ignored::deserialize(toml::Deserializer::parse(text)?, |path| {
                unused_keys.push(key_path(&path));
            })?;
        unused_keys.sort();

        let project = raw
            .project
            .ok_or_else(|| ManifestError::MissingField("project.name".into()))?;
        let name = project
            .name
            .ok_or_else(|| ManifestError::MissingField("project.name".into()))?;
        let version = project
            .version
            .unwrap_or_else(|| DEFAULT_VERSION.to_string());
        let std = match project.std {
            Some(value) => value.parse()?,
            None => CppStd::DEFAULT,
        };

        if let Some(index) = raw.bin.iter().position(|bin| bin.name.is_none()) {
            return Err(ManifestError::MissingField(format!("bin[{index}].name")));
        }

        let dependencies = raw
            .dependencies
            .into_iter()
            .map(|(name, dep)| {
                let spec = match dep {
                    RawDependency::Version(version) => DependencySpec {
                        version: Some(version),
                        ..DependencySpec::default()
                    },
                    RawDependency::Detailed(spec) => spec,
                };
                (name, spec)
            })
            .collect();

        Ok(Self {
            project: Project { name, version, std },
            lib: raw.lib,
            bins: raw.bin,
            dependencies,
            unused_keys,
        })
    }

    /// Read and parse the manifest file at `path`.
    ///
    /// # Errors
    ///
    /// Fails if the file can't be read, or for any reason [`Self::from_toml_str`] does.
    pub fn from_path(path: &Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// Load `Yam.toml` from a project root directory.
    ///
    /// # Errors
    ///
    /// Fails with [`ManifestError::NotFound`] if the directory has no `Yam.toml`, or
    /// for any reason [`Self::from_path`] does.
    pub fn load(project_root: &Path) -> Result<Self, ManifestError> {
        let path = project_root.join(MANIFEST_FILE_NAME);
        if !path.is_file() {
            return Err(ManifestError::NotFound {
                dir: project_root.to_path_buf(),
            });
        }
        Self::from_path(&path)
    }
}

/// A `serde_ignored` path as a manifest key: `lib.include-dirs`, `bin[1].name`.
/// `Option` and newtype layers don't appear in the key.
fn key_path(path: &serde_ignored::Path<'_>) -> String {
    use serde_ignored::Path;
    match path {
        Path::Root => String::new(),
        Path::Seq { parent, index } => format!("{}[{index}]", key_path(parent)),
        Path::Map { parent, key } => match key_path(parent) {
            prefix if prefix.is_empty() => key.clone(),
            prefix => format!("{prefix}.{key}"),
        },
        Path::Some { parent }
        | Path::NewtypeStruct { parent }
        | Path::NewtypeVariant { parent } => key_path(parent),
    }
}

#[derive(Deserialize)]
struct RawManifest {
    project: Option<RawProject>,
    lib: Option<TargetSpec>,
    #[serde(default)]
    bin: Vec<TargetSpec>,
    #[serde(default)]
    dependencies: BTreeMap<String, RawDependency>,
}

#[derive(Deserialize)]
struct RawProject {
    name: Option<String>,
    version: Option<String>,
    std: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawDependency {
    Version(String),
    Detailed(DependencySpec),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Manifest {
        Manifest::from_toml_str(text).unwrap()
    }

    fn parse_err(text: &str) -> String {
        Manifest::from_toml_str(text).unwrap_err().to_string()
    }

    #[test]
    fn minimal_manifest_gets_defaults() {
        let manifest = parse("[project]\nname = \"hello\"\n");
        assert_eq!(
            manifest.project,
            Project {
                name: "hello".into(),
                version: "0.1.0".into(),
                std: CppStd::Cpp26,
            }
        );
        assert_eq!(manifest.lib, None);
        assert_eq!(manifest.bins, Vec::<TargetSpec>::new());
        assert_eq!(manifest.dependencies, BTreeMap::new());
    }

    #[test]
    fn reads_project_fields() {
        let manifest = parse("[project]\nname = \"mylib\"\nversion = \"1.2.3\"\nstd = \"c++23\"\n");
        assert_eq!(manifest.project.version, "1.2.3");
        assert_eq!(manifest.project.std, CppStd::Cpp23);
    }

    #[test]
    fn missing_project_name_is_reported() {
        assert_eq!(
            parse_err("[project]\nversion = \"1.0.0\"\n"),
            "missing required field 'project.name'"
        );
        assert_eq!(parse_err(""), "missing required field 'project.name'");
    }

    #[test]
    fn invalid_std_is_reported() {
        assert_eq!(
            parse_err("[project]\nname = \"x\"\nstd = \"c++98\"\n"),
            "invalid 'project.std': invalid std 'c++98'; expected one of: c++11, c++14, \
             c++17, c++20, c++23, c++26"
        );
    }

    #[test]
    fn invalid_toml_is_a_syntax_error() {
        let err = Manifest::from_toml_str("[project\nname = 1").unwrap_err();
        assert!(matches!(err, ManifestError::Syntax(_)), "{err:?}");
    }

    #[test]
    fn wrong_field_type_is_a_syntax_error() {
        let err = Manifest::from_toml_str("[project]\nname = 42\n").unwrap_err();
        assert!(matches!(err, ManifestError::Syntax(_)), "{err:?}");
    }

    #[test]
    fn reads_lib_and_bin_tables() {
        let manifest = parse(
            r#"
            [project]
            name = "port"

            [lib]
            sources = ["lib/**/*.cpp"]
            exclude = ["lib/**/test_*.cpp"]
            include-dirs = ["include"]

            [[bin]]
            name = "port-cli"
            path = "tools/cli.cpp"

            [[bin]]
            name = "other"
            "#,
        );
        let lib = manifest.lib.unwrap();
        assert_eq!(lib.sources, Some(vec!["lib/**/*.cpp".to_string()]));
        assert_eq!(lib.exclude, vec!["lib/**/test_*.cpp".to_string()]);
        assert_eq!(lib.include_dirs, vec![PathBuf::from("include")]);
        assert_eq!(manifest.bins.len(), 2);
        assert_eq!(manifest.bins[0].name.as_deref(), Some("port-cli"));
        assert_eq!(manifest.bins[0].path, Some(PathBuf::from("tools/cli.cpp")));
        assert_eq!(manifest.bins[1].path, None);
    }

    #[test]
    fn bin_without_name_is_reported_with_its_index() {
        assert_eq!(
            parse_err(
                "[project]\nname = \"x\"\n[[bin]]\nname = \"a\"\n[[bin]]\npath = \"b.cpp\"\n"
            ),
            "missing required field 'bin[1].name'"
        );
    }

    #[test]
    fn reads_dependencies_in_both_forms() {
        let manifest = parse(
            r#"
            [project]
            name = "app"

            [dependencies]
            fmt = "11.0"
            local = { path = "../local" }
            pinned = { git = "https://example.com/pinned.git", rev = "abc123" }
            "#,
        );
        let deps = &manifest.dependencies;
        assert_eq!(deps["fmt"].version.as_deref(), Some("11.0"));
        assert_eq!(deps["local"].path, Some(PathBuf::from("../local")));
        assert_eq!(deps["pinned"].rev.as_deref(), Some("abc123"));
    }

    #[test]
    fn load_reports_a_missing_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(matches!(err, ManifestError::NotFound { .. }), "{err:?}");
        assert!(err.to_string().starts_with("could not find 'Yam.toml' in "));
    }

    #[test]
    fn load_reads_the_manifest_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MANIFEST_FILE_NAME),
            "[project]\nname = \"x\"\n",
        )
        .unwrap();
        assert_eq!(Manifest::load(dir.path()).unwrap().project.name, "x");
    }
}
