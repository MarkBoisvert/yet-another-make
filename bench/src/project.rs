//! Writing a benchmark project, and the edits each scenario applies to it.
//!
//! One source tree holds both a `Yam.toml` and a `CMakeLists.txt`, so yam and
//! CMake + ninja build exactly the same sources:
//!
//! ```text
//! <dir>/
//! ├── Yam.toml, CMakeLists.txt, bench.json
//! └── src/
//!     ├── main.cpp          # uses every top-layer unit
//!     └── units/uNNNN.*     # .cppm + .cpp (modules) or .hpp + .cpp (legacy)
//! ```
//!
//! Each interface has a `// bench:iface` line and each implementation a
//! `// bench:impl` line whose value an edit flips between 0 and 1, so repeated edits
//! always change the content.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use yet_another_make::manifest::{CppStd, Manifest, Project, TargetSpec};

use crate::graph::Graph;

pub const META_FILE: &str = "bench.json";
const IFACE_MARKER: &str = "// bench:iface";
const IMPL_MARKER: &str = "// bench:impl";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// C++26 named modules with `import std`
    Modules,
    /// C++17 headers and `#include`
    Legacy,
}

impl Style {
    #[must_use]
    pub fn std(self) -> CppStd {
        match self {
            Self::Modules => CppStd::Cpp26,
            Self::Legacy => CppStd::Cpp17,
        }
    }

    /// The interface file's extension.
    fn iface_ext(self) -> &'static str {
        match self {
            Self::Modules => "cppm",
            Self::Legacy => "hpp",
        }
    }
}

/// The edits `bench edit` can apply. `cold` and `noop` need no edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Scenario {
    /// Update the root interface's mtime without changing it
    Touch,
    /// Change one leaf unit's function body
    LeafImpl,
    /// Change one leaf unit's interface
    LeafIface,
    /// Change function bodies in 1% of units
    #[value(name = "impl-1pct")]
    Impl1pct,
    /// Change function bodies in 10% of units
    #[value(name = "impl-10pct")]
    Impl10pct,
    /// Change the interface with the median dependent count
    MidIface,
    /// Change the interface everything depends on
    RootIface,
    /// Add a new leaf unit and use it from `main`
    AddUnit,
}

/// `bench.json`: how the project was generated, which units the scenarios edit, and
/// how many units each scenario should rebuild.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub style: Style,
    pub seed: u64,
    pub depth: usize,
    pub fanout: usize,
    /// Units generated; `graph` also holds units added by `add-unit`.
    pub units: usize,
    pub leaf: usize,
    pub mid: usize,
    pub root: usize,
    /// For each scenario, the units it should rebuild (interface and implementation
    /// count as one unit). `main` is extra for interface edits and `add-unit`.
    pub expected_rebuilt_units: BTreeMap<String, usize>,
    pub graph: Graph,
}

impl Meta {
    #[must_use]
    pub fn new(style: Style, units: usize, depth: usize, fanout: usize, seed: u64) -> Self {
        let graph = Graph::generate(units, depth, fanout, seed);
        let counts = graph.dependent_counts();
        let leaf = units - 1;
        let mid = (0..units)
            .min_by_key(|&u| counts[u].abs_diff(units / 2))
            .unwrap_or(0);
        let rebuilt = |unit: usize| counts[unit] + 1;
        let expected_rebuilt_units = [
            ("touch", 0),
            ("leaf-impl", 1),
            ("leaf-iface", rebuilt(leaf)),
            ("impl-1pct", percent(units, 1)),
            ("impl-10pct", percent(units, 10)),
            ("mid-iface", rebuilt(mid)),
            ("root-iface", rebuilt(0)),
            ("add-unit", 1),
        ]
        .into_iter()
        .map(|(name, count)| (name.to_string(), count))
        .collect();
        Self {
            style,
            seed,
            depth,
            fanout,
            units,
            leaf,
            mid,
            root: 0,
            expected_rebuilt_units,
            graph,
        }
    }

    /// Units `main` uses: the original top layer plus any added units.
    fn main_units(&self) -> Vec<usize> {
        self.graph.top()
    }

    fn name(&self, unit: usize) -> String {
        let width = (self.units - 1).to_string().len().max(4);
        format!("u{unit:0width$}")
    }

    fn iface_path(&self, unit: usize) -> PathBuf {
        Path::new("src/units").join(format!("{}.{}", self.name(unit), self.style.iface_ext()))
    }

    fn impl_path(&self, unit: usize) -> PathBuf {
        Path::new("src/units").join(format!("{}.cpp", self.name(unit)))
    }
}

/// `ceil(units * pct / 100)`, at least 1.
fn percent(units: usize, pct: usize) -> usize {
    (units * pct).div_ceil(100).max(1)
}

/// Write a new project into `dir`, which must not already hold one.
///
/// # Errors
///
/// Fails if `dir` already has a `bench.json` or a file can't be written.
pub fn generate(dir: &Path, meta: &Meta) -> Result<()> {
    if dir.join(META_FILE).exists() {
        bail!("`{}` already holds a benchmark project", dir.display());
    }
    fs::create_dir_all(dir.join("src/units"))
        .with_context(|| format!("failed to create `{}`", dir.display()))?;
    for unit in 0..meta.graph.len() {
        write_unit(dir, meta, unit)?;
    }
    write_project_files(dir, meta)?;
    write(&dir.join("Yam.toml"), &yam_toml(meta))
}

/// Read `bench.json` from `dir`.
///
/// # Errors
///
/// Fails if it's missing or malformed.
pub fn load(dir: &Path) -> Result<Meta> {
    let path = dir.join(META_FILE);
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read `{}`; generate a project with `bench gen`",
            path.display()
        )
    })?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse `{}`", path.display()))
}

/// Apply `scenario`'s edit to the project in `dir`. Returns the files changed.
///
/// # Errors
///
/// Fails if the project can't be read or a file can't be changed.
pub fn edit(dir: &Path, scenario: Scenario) -> Result<Vec<PathBuf>> {
    let mut meta = load(dir)?;
    let units = meta.units;
    let changed = match scenario {
        Scenario::Touch => {
            let path = dir.join(meta.iface_path(meta.root));
            fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(std::time::SystemTime::now()))
                .with_context(|| format!("failed to touch `{}`", path.display()))?;
            vec![path]
        }
        Scenario::LeafImpl => vec![flip(&dir.join(meta.impl_path(meta.leaf)), IMPL_MARKER)?],
        Scenario::LeafIface => vec![flip(&dir.join(meta.iface_path(meta.leaf)), IFACE_MARKER)?],
        Scenario::MidIface => vec![flip(&dir.join(meta.iface_path(meta.mid)), IFACE_MARKER)?],
        Scenario::RootIface => vec![flip(&dir.join(meta.iface_path(meta.root)), IFACE_MARKER)?],
        Scenario::Impl1pct | Scenario::Impl10pct => {
            let pct = if scenario == Scenario::Impl1pct {
                1
            } else {
                10
            };
            let count = percent(units, pct);
            (0..count)
                .map(|i| flip(&dir.join(meta.impl_path(i * units / count)), IMPL_MARKER))
                .collect::<Result<_>>()?
        }
        Scenario::AddUnit => {
            let unit = meta.graph.len();
            meta.graph.deps.push(vec![meta.root]);
            write_unit(dir, &meta, unit)?;
            write_project_files(dir, &meta)?;
            vec![
                dir.join(meta.iface_path(unit)),
                dir.join(meta.impl_path(unit)),
            ]
        }
    };
    Ok(changed)
}

/// Flip the 0/1 value on the line ending with `marker`.
fn flip(path: &Path, marker: &str) -> Result<PathBuf> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read `{}`", path.display()))?;
    let mut found = false;
    let edited: String = text
        .lines()
        .map(|line| {
            if !found && line.ends_with(marker) {
                found = true;
                if line.contains("= 0;") {
                    return line.replacen("= 0;", "= 1;", 1);
                }
                return line.replacen("= 1;", "= 0;", 1);
            }
            line.to_string()
        })
        .flat_map(|line| [line, "\n".to_string()])
        .collect();
    if !found {
        bail!("`{}` has no `{marker}` line", path.display());
    }
    write(path, &edited)?;
    Ok(path.to_path_buf())
}

fn write(path: &Path, text: &str) -> Result<()> {
    fs::write(path, text).with_context(|| format!("failed to write `{}`", path.display()))
}

/// `main.cpp`, `CMakeLists.txt` and `bench.json`: everything that lists units.
fn write_project_files(dir: &Path, meta: &Meta) -> Result<()> {
    write(&dir.join("src/main.cpp"), &main_cpp(meta))?;
    write(&dir.join("CMakeLists.txt"), &cmake_lists(meta))?;
    let json = serde_json::to_string_pretty(meta).context("failed to serialize bench.json")?;
    write(&dir.join(META_FILE), &json)
}

fn write_unit(dir: &Path, meta: &Meta, unit: usize) -> Result<()> {
    let (iface, implementation) = match meta.style {
        Style::Modules => module_unit(meta, unit),
        Style::Legacy => legacy_unit(meta, unit),
    };
    write(&dir.join(meta.iface_path(unit)), &iface)?;
    write(&dir.join(meta.impl_path(unit)), &implementation)
}

/// The value expression for a unit: its index plus each dependency's value, so every
/// interface really uses its dependencies' interfaces.
fn value_expr(meta: &Meta, unit: usize) -> String {
    let mut expr = unit.to_string();
    for &dep in &meta.graph.deps[unit] {
        let _ = write!(expr, " + {}::kValue % 1000", meta.name(dep));
    }
    expr
}

const NAMESPACE_BODY: &str = r"
inline constexpr std::int64_t kValue = {value};
inline constexpr int kRevision = 0; {iface_marker}

struct Record {
    std::string name;
    std::vector<std::int64_t> values;
};

Record make_record(std::int64_t seed);
";

const IMPL_BODY: &str = r#"
Record make_record(std::int64_t seed) {
    constexpr int revision = 0; {impl_marker}
    Record record{"{name}", {}};
    record.values.reserve(4);
    record.values.push_back(seed + kValue);
    record.values.push_back(seed * 3 + revision + kRevision);
    std::sort(record.values.begin(), record.values.end());
    return record;
}
"#;

fn fill(template: &str, meta: &Meta, unit: usize) -> String {
    template
        .replace("{value}", &value_expr(meta, unit))
        .replace("{name}", &meta.name(unit))
        .replace("{iface_marker}", IFACE_MARKER)
        .replace("{impl_marker}", IMPL_MARKER)
}

fn module_unit(meta: &Meta, unit: usize) -> (String, String) {
    let name = meta.name(unit);
    let mut iface = format!("export module {name};\n\nimport std;\n");
    for &dep in &meta.graph.deps[unit] {
        let _ = writeln!(iface, "import {};", meta.name(dep));
    }
    let _ = write!(
        iface,
        "\nexport namespace {name} {{\n{}\n}} // namespace {name}\n",
        fill(NAMESPACE_BODY, meta, unit)
    );
    let implementation = format!(
        "module {name};\n\nimport std;\n\nnamespace {name} {{\n{}\n}} // namespace {name}\n",
        fill(IMPL_BODY, meta, unit)
    );
    (iface, implementation)
}

fn legacy_unit(meta: &Meta, unit: usize) -> (String, String) {
    let name = meta.name(unit);
    let mut iface = String::from(
        "#pragma once\n\n#include <algorithm>\n#include <cstdint>\n#include <string>\n#include <vector>\n\n",
    );
    for &dep in &meta.graph.deps[unit] {
        let _ = writeln!(iface, "#include \"{}.hpp\"", meta.name(dep));
    }
    let _ = write!(
        iface,
        "\nnamespace {name} {{\n{}\n}} // namespace {name}\n",
        fill(NAMESPACE_BODY, meta, unit)
    );
    let implementation = format!(
        "#include \"{name}.hpp\"\n\nnamespace {name} {{\n{}\n}} // namespace {name}\n",
        fill(IMPL_BODY, meta, unit)
    );
    (iface, implementation)
}

fn main_cpp(meta: &Meta) -> String {
    let units = meta.main_units();
    let mut out = String::new();
    match meta.style {
        Style::Modules => {
            out.push_str("import std;\n");
            for &unit in &units {
                let _ = writeln!(out, "import {};", meta.name(unit));
            }
        }
        Style::Legacy => {
            out.push_str("#include <cstdint>\n#include <cstdio>\n");
            for &unit in &units {
                let _ = writeln!(out, "#include \"units/{}.hpp\"", meta.name(unit));
            }
        }
    }
    out.push_str("\nint main() {\n    std::int64_t total = 0;\n");
    for &unit in &units {
        let _ = writeln!(
            out,
            "    total += {}::make_record(1).values.front();",
            meta.name(unit)
        );
    }
    match meta.style {
        Style::Modules => out.push_str("    std::println(\"{}\", total);\n}\n"),
        Style::Legacy => {
            out.push_str("    std::printf(\"%lld\\n\", static_cast<long long>(total));\n}\n");
        }
    }
    out
}

fn yam_toml(meta: &Meta) -> String {
    let (entry, sources) = match meta.style {
        Style::Modules => (
            meta.iface_path(0),
            vec!["src/units/*.cppm", "src/units/*.cpp"],
        ),
        Style::Legacy => (meta.impl_path(0), vec!["src/units/*.cpp"]),
    };
    Manifest {
        project: Project {
            name: "bench".into(),
            version: "0.1.0".into(),
            std: meta.style.std(),
        },
        lib: Some(TargetSpec {
            name: Some("units".into()),
            path: Some(entry),
            sources: Some(sources.into_iter().map(String::from).collect()),
            ..TargetSpec::default()
        }),
        bins: Vec::new(),
        dependencies: BTreeMap::new(),
        unused_keys: Vec::new(),
    }
    .to_toml_string()
}

fn cmake_lists(meta: &Meta) -> String {
    let std_number = &meta.style.std().as_str()["c++".len()..];
    let mut out = format!(
        "# Generated by `bench gen`; configure with `bench configure`.\n\
         cmake_minimum_required(VERSION 3.30)\n\
         project(bench LANGUAGES CXX)\n\n\
         set(CMAKE_CXX_STANDARD {std_number})\n\
         set(CMAKE_CXX_STANDARD_REQUIRED ON)\n\
         set(CMAKE_CXX_EXTENSIONS OFF)\n"
    );
    let path = |p: PathBuf| p.to_string_lossy().replace('\\', "/");
    match meta.style {
        Style::Modules => {
            out.push_str("set(CMAKE_CXX_MODULE_STD ON)\n\nadd_library(units STATIC)\ntarget_sources(units\n  PUBLIC FILE_SET CXX_MODULES FILES\n");
            for unit in 0..meta.graph.len() {
                let _ = writeln!(out, "    {}", path(meta.iface_path(unit)));
            }
            out.push_str("  PRIVATE\n");
        }
        Style::Legacy => {
            out.push_str("set(CMAKE_CXX_SCAN_FOR_MODULES OFF)\n\nadd_library(units STATIC)\ntarget_sources(units PRIVATE\n");
        }
    }
    for unit in 0..meta.graph.len() {
        let _ = writeln!(out, "    {}", path(meta.impl_path(unit)));
    }
    out.push_str(
        ")\n\nadd_executable(bench src/main.cpp)\ntarget_link_libraries(bench PRIVATE units)\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use yet_another_make::manifest::{self, TargetKind};

    fn project(style: Style, units: usize) -> (tempfile::TempDir, Meta) {
        let temp = tempdir().unwrap();
        let meta = Meta::new(style, units, 4, 3, 1);
        generate(temp.path(), &meta).unwrap();
        (temp, meta)
    }

    #[test]
    fn writes_both_build_systems_over_the_same_sources() {
        for style in [Style::Modules, Style::Legacy] {
            let (temp, meta) = project(style, 20);
            let dir = temp.path();
            for unit in 0..20 {
                assert!(dir.join(meta.iface_path(unit)).is_file());
                assert!(dir.join(meta.impl_path(unit)).is_file());
            }
            let cmake = fs::read_to_string(dir.join("CMakeLists.txt")).unwrap();
            assert!(cmake.contains("src/units/u0019.cpp"), "{cmake}");
            assert_eq!(load(dir).unwrap(), meta);

            // yam reads the same tree: a lib of every unit plus the `main` bin.
            let manifest = Manifest::load(dir).unwrap();
            assert_eq!(manifest.project.std, style.std());
            let diagnostics = manifest::validate(&manifest, dir).unwrap();
            assert_eq!(diagnostics, Vec::new());
            let targets = manifest::resolve_targets(&manifest, dir).unwrap();
            let kinds: Vec<_> = targets.iter().map(|t| (t.name.as_str(), t.kind)).collect();
            assert_eq!(
                kinds,
                [("units", TargetKind::Lib), ("bench", TargetKind::Bin)]
            );
        }
    }

    #[test]
    fn module_units_import_their_dependencies() {
        let (temp, meta) = project(Style::Modules, 20);
        let unit = 19;
        let text = fs::read_to_string(temp.path().join(meta.iface_path(unit))).unwrap();
        assert!(
            text.starts_with("export module u0019;\n\nimport std;\n"),
            "{text}"
        );
        for &dep in &meta.graph.deps[unit] {
            assert!(
                text.contains(&format!("import {};", meta.name(dep))),
                "{text}"
            );
        }
        let main = fs::read_to_string(temp.path().join("src/main.cpp")).unwrap();
        assert!(main.contains("import u0019;"), "{main}");
    }

    #[test]
    fn refuses_to_overwrite_a_project() {
        let (temp, meta) = project(Style::Legacy, 5);
        assert!(generate(temp.path(), &meta).is_err());
    }

    #[test]
    fn edits_flip_markers_back_and_forth() {
        let (temp, meta) = project(Style::Legacy, 30);
        let leaf_impl = temp.path().join(meta.impl_path(meta.leaf));
        let original = fs::read_to_string(&leaf_impl).unwrap();
        edit(temp.path(), Scenario::LeafImpl).unwrap();
        let edited = fs::read_to_string(&leaf_impl).unwrap();
        assert_ne!(edited, original);
        assert!(
            edited.contains("constexpr int revision = 1; // bench:impl"),
            "{edited}"
        );
        edit(temp.path(), Scenario::LeafImpl).unwrap();
        assert_eq!(fs::read_to_string(&leaf_impl).unwrap(), original);

        let root = temp.path().join(meta.iface_path(0));
        edit(temp.path(), Scenario::RootIface).unwrap();
        assert!(
            fs::read_to_string(&root)
                .unwrap()
                .contains("kRevision = 1; // bench:iface")
        );
    }

    #[test]
    fn percentage_edits_touch_that_many_units() {
        let (temp, _) = project(Style::Modules, 100);
        assert_eq!(edit(temp.path(), Scenario::Impl1pct).unwrap().len(), 1);
        let changed = edit(temp.path(), Scenario::Impl10pct).unwrap();
        assert_eq!(changed.len(), 10);
        let mut unique = changed.clone();
        unique.dedup();
        assert_eq!(unique.len(), 10);
    }

    #[test]
    fn touch_changes_only_the_mtime() {
        let (temp, meta) = project(Style::Modules, 10);
        let root = temp.path().join(meta.iface_path(0));
        let before = fs::read_to_string(&root).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        fs::File::options()
            .write(true)
            .open(&root)
            .unwrap()
            .set_modified(old)
            .unwrap();
        edit(temp.path(), Scenario::Touch).unwrap();
        assert_eq!(fs::read_to_string(&root).unwrap(), before);
        assert!(fs::metadata(&root).unwrap().modified().unwrap() > old);
    }

    #[test]
    fn add_unit_extends_main_and_cmake() {
        let (temp, meta) = project(Style::Modules, 10);
        edit(temp.path(), Scenario::AddUnit).unwrap();
        let after = load(temp.path()).unwrap();
        assert_eq!(after.graph.len(), 11);
        assert_eq!(after.units, meta.units);
        let main = fs::read_to_string(temp.path().join("src/main.cpp")).unwrap();
        assert!(main.contains("import u0010;"), "{main}");
        let cmake = fs::read_to_string(temp.path().join("CMakeLists.txt")).unwrap();
        assert!(cmake.contains("src/units/u0010.cppm"), "{cmake}");
    }

    #[test]
    fn records_expected_rebuilds() {
        let meta = Meta::new(Style::Modules, 1000, 8, 3, 1);
        let expected = &meta.expected_rebuilt_units;
        assert_eq!(expected["root-iface"], 1000);
        assert_eq!(expected["leaf-impl"], 1);
        assert_eq!(expected["impl-10pct"], 100);
        let mid = expected["mid-iface"];
        assert!(mid > 1 && mid < 1000, "{mid}");
    }
}
