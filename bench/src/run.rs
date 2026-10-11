//! `bench run`: time every scenario × style × size with `hyperfine`, and `bench
//! report`: render the results as the tables in `docs/bench.md`.
//!
//! Results are saved after every measurement, and cells already in the results file
//! are skipped, so a multi-hour run can be stopped and resumed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cmake;
use crate::project::{self, Meta, Style};

/// Every scenario, in report order. `cold` and `noop` need no `bench edit`.
pub const SCENARIOS: [&str; 10] = [
    "cold",
    "noop",
    "touch",
    "leaf-impl",
    "leaf-iface",
    "impl-1pct",
    "impl-10pct",
    "mid-iface",
    "root-iface",
    "add-unit",
];

/// Scenarios that rebuild most of the project; at 1k units and up they get fewer runs.
const HEAVY: [&str; 4] = ["cold", "touch", "mid-iface", "root-iface"];

/// A build tool being timed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tool {
    /// `ninja` alone: the no-op/touch gates compare against it.
    Ninja,
    /// `cmake --build`: the edit gates compare against it.
    CmakeBuild,
    /// `yam build`, once it can build multi-file targets (#20).
    Yam,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Self::Ninja => "ninja",
            Self::CmakeBuild => "cmake --build",
            Self::Yam => "yam build",
        }
    }
}

/// The tools each scenario is timed with: `ninja` alone for the gates that compare
/// against it, `cmake --build` for the rest.
fn tools_for(scenario: &str, yam: bool) -> Vec<Tool> {
    let mut tools = match scenario {
        "noop" => vec![Tool::Ninja, Tool::CmakeBuild],
        "touch" => vec![Tool::Ninja],
        _ => vec![Tool::CmakeBuild],
    };
    if yam {
        tools.push(Tool::Yam);
    }
    tools
}

/// How many times to run a scenario at a size.
fn runs_for(scenario: &str, units: usize) -> usize {
    if units >= 1000 && HEAVY.contains(&scenario) {
        2
    } else {
        5
    }
}

/// One measured cell of the grid. Times are in seconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub style: Style,
    pub units: usize,
    pub scenario: String,
    pub tool: Tool,
    pub runs: usize,
    pub mean: f64,
    pub stddev: f64,
    pub min: f64,
    pub max: f64,
}

/// The results file: the machine plus every measured cell.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Results {
    pub machine: BTreeMap<String, String>,
    pub cells: Vec<Cell>,
}

impl Results {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read `{}`", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("failed to parse `{}`", path.display()))
    }

    fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json).with_context(|| format!("failed to write `{}`", path.display()))
    }

    fn has(&self, style: Style, units: usize, scenario: &str, tool: Tool) -> bool {
        self.cells.iter().any(|c| {
            c.style == style && c.units == units && c.scenario == scenario && c.tool == tool
        })
    }
}

/// What `bench run` measures.
pub struct Plan {
    pub work: PathBuf,
    pub results: PathBuf,
    pub styles: Vec<Style>,
    pub sizes: Vec<usize>,
    pub scenarios: Vec<String>,
    pub jobs: usize,
    /// `yam` to time as well, once it can build these projects.
    pub yam: Option<PathBuf>,
}

/// Run every cell of `plan` not already in its results file.
///
/// # Errors
///
/// Fails if a project can't be generated or configured, or `hyperfine` fails.
pub fn run(plan: &Plan) -> Result<()> {
    for scenario in &plan.scenarios {
        if !SCENARIOS.contains(&scenario.as_str()) {
            bail!(
                "unknown scenario `{scenario}`; expected one of {}",
                SCENARIOS.join(", ")
            );
        }
    }
    fs::create_dir_all(&plan.work)
        .with_context(|| format!("failed to create `{}`", plan.work.display()))?;
    let bench = std::env::current_exe().context("failed to locate the bench binary")?;
    let mut results = Results::load(&plan.results)?;
    results.machine = machine_info(plan.jobs);
    results.save(&plan.results)?;

    for &style in &plan.styles {
        for &units in &plan.sizes {
            let dir = plan.work.join(format!("{}-{units}", style_name(style)));
            prepare_project(&dir, style, units, plan.jobs)?;
            let build = dir.join(cmake::BUILD_DIR);
            for scenario in &plan.scenarios {
                for tool in tools_for(scenario, plan.yam.is_some()) {
                    if results.has(style, units, scenario, tool) {
                        continue;
                    }
                    println!(
                        "\n== {} {units} {scenario} ({})",
                        style_name(style),
                        tool.label()
                    );
                    let command = build_command(tool, &dir, &build, plan);
                    let prepare = prepare_command(scenario, tool, &bench, &dir, &build, plan);
                    let runs = runs_for(scenario, units);
                    let (mean, stddev, min, max) = hyperfine(&command, prepare.as_deref(), runs)?;
                    results.cells.push(Cell {
                        style,
                        units,
                        scenario: scenario.clone(),
                        tool,
                        runs,
                        mean,
                        stddev,
                        min,
                        max,
                    });
                    results.save(&plan.results)?;
                }
            }
        }
    }
    Ok(())
}

fn style_name(style: Style) -> &'static str {
    match style {
        Style::Modules => "modules",
        Style::Legacy => "legacy",
    }
}

/// Generate, configure and fully build the project, unless that's already done.
fn prepare_project(dir: &Path, style: Style, units: usize, jobs: usize) -> Result<()> {
    if !dir.join(project::META_FILE).exists() {
        println!(
            "generating {} {units} in `{}`",
            style_name(style),
            dir.display()
        );
        project::generate(dir, &Meta::new(style, units, 8, 3, 1))?;
    }
    let build = dir.join(cmake::BUILD_DIR);
    if !build.join("build.ninja").exists() {
        cmake::configure(dir, false, None)?;
    }
    println!(
        "building {} {units} so every scenario starts up to date",
        style_name(style)
    );
    let status = Command::new("cmake")
        .arg("--build")
        .arg(&build)
        .arg("--parallel")
        .arg(jobs.to_string())
        .status()
        .context("failed to run `cmake --build`")?;
    if !status.success() {
        bail!("the initial build of `{}` failed", dir.display());
    }
    Ok(())
}

/// The timed command, as hyperfine parses it (`--shell=none`).
fn build_command(tool: Tool, dir: &Path, build: &Path, plan: &Plan) -> String {
    let jobs = plan.jobs.to_string();
    match tool {
        Tool::Ninja => join(&["ninja", "-C", &path(build), "-j", &jobs]),
        Tool::CmakeBuild => join(&["cmake", "--build", &path(build), "--parallel", &jobs]),
        Tool::Yam => {
            let yam = plan.yam.as_deref().map(path).unwrap_or_default();
            join(&[&yam, "build", &path(dir)])
        }
    }
}

/// The untimed command run before each timed one.
fn prepare_command(
    scenario: &str,
    tool: Tool,
    bench: &Path,
    dir: &Path,
    build: &Path,
    plan: &Plan,
) -> Option<String> {
    match scenario {
        "noop" => None,
        "cold" => Some(match tool {
            Tool::Yam => {
                let yam = plan.yam.as_deref().map(path).unwrap_or_default();
                join(&[&yam, "clean", &path(dir)])
            }
            _ => join(&["ninja", "-C", &path(build), "-t", "clean"]),
        }),
        edit => Some(join(&[&path(bench), "edit", edit, &path(dir)])),
    }
}

/// Run hyperfine and return (mean, stddev, min, max) in seconds.
fn hyperfine(command: &str, prepare: Option<&str>, runs: usize) -> Result<(f64, f64, f64, f64)> {
    let export = std::env::temp_dir().join(format!("bench-hyperfine-{}.json", std::process::id()));
    let mut hyperfine = Command::new("hyperfine");
    hyperfine
        .args(["--shell=none", "--style", "basic", "--runs"])
        .arg(runs.to_string())
        .arg("--export-json")
        .arg(&export);
    if let Some(prepare) = prepare {
        hyperfine.arg("--prepare").arg(prepare);
    }
    hyperfine.arg(command);
    let status = hyperfine
        .status()
        .context("failed to run `hyperfine`; install it with `cargo install hyperfine`")?;
    if !status.success() {
        bail!("hyperfine failed for `{command}`");
    }
    let text = fs::read_to_string(&export)?;
    let _ = fs::remove_file(&export);
    parse_hyperfine(&text)
}

fn parse_hyperfine(json: &str) -> Result<(f64, f64, f64, f64)> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let result = &value["results"][0];
    let get = |key: &str| {
        result[key]
            .as_f64()
            .with_context(|| format!("hyperfine output has no `{key}`"))
    };
    // A single run has no standard deviation.
    let stddev = result["stddev"].as_f64().unwrap_or(0.0);
    Ok((get("mean")?, stddev, get("min")?, get("max")?))
}

/// Quote arguments containing spaces for hyperfine's shell-words parsing.
fn join(args: &[&str]) -> String {
    args.iter()
        .map(|arg| {
            if arg.contains(char::is_whitespace) || arg.is_empty() {
                format!("'{arg}'")
            } else {
                (*arg).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The machine and tool versions, for the report.
fn machine_info(jobs: usize) -> BTreeMap<String, String> {
    let first_line = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .output()
            .ok()
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .next()
                    .map(|line| line.trim().to_string())
            })
            .unwrap_or_else(|| "unknown".into())
    };
    let cpu = fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find_map(|line| line.strip_prefix("model name"))
                .and_then(|rest| rest.split(':').nth(1))
                .map(|name| name.trim().to_string())
        })
        .unwrap_or_else(|| first_line("sysctl", &["-n", "machdep.cpu.brand_string"]));
    let memory = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|info| {
            let kb: u64 = info
                .lines()
                .find_map(|line| line.strip_prefix("MemTotal:"))?
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .ok()?;
            Some(format!("{} GiB", kb / (1024 * 1024)))
        })
        .unwrap_or_else(|| "unknown".into());
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let cxx = yet_another_make::toolchain::Toolchain::discover().map_or_else(
        |_| "unknown".into(),
        |t| format!("{} ({})", t.cxx.path.display(), t.cxx.version),
    );
    [
        ("cpu", cpu),
        ("cores", cores.to_string()),
        ("jobs", jobs.to_string()),
        ("memory", memory),
        ("os", first_line("uname", &["-srm"])),
        ("clang++", cxx),
        ("cmake", first_line("cmake", &["--version"])),
        ("ninja", first_line("ninja", &["--version"])),
        ("hyperfine", first_line("hyperfine", &["--version"])),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

/// Render `results` as Markdown: the machine, then one table per style and size.
#[must_use]
pub fn report(results: &Results) -> String {
    let mut out = String::from("| | |\n|---|---|\n");
    for (key, value) in &results.machine {
        let _ = writeln!(out, "| {key} | {value} |");
    }
    let mut groups: Vec<(Style, usize)> =
        results.cells.iter().map(|c| (c.style, c.units)).collect();
    groups.sort_by_key(|&(style, units)| (style_name(style), units));
    groups.dedup();
    let tools = [Tool::Ninja, Tool::CmakeBuild, Tool::Yam];
    for (style, units) in groups {
        let _ = write!(
            out,
            "\n### {} — {units} units\n\n| Scenario | ninja | cmake --build | yam build |\n|---|---|---|---|\n",
            style_name(style)
        );
        for scenario in SCENARIOS {
            let row: Vec<String> = tools
                .iter()
                .map(|&tool| {
                    results
                        .cells
                        .iter()
                        .find(|c| {
                            c.style == style
                                && c.units == units
                                && c.scenario == scenario
                                && c.tool == tool
                        })
                        .map_or_else(|| "—".to_string(), format_cell)
                })
                .collect();
            if row.iter().all(|cell| cell == "—") {
                continue;
            }
            let _ = writeln!(out, "| `{scenario}` | {} |", row.join(" | "));
        }
    }
    out
}

fn format_cell(cell: &Cell) -> String {
    format!(
        "{} ± {} (n={})",
        seconds(cell.mean),
        seconds(cell.stddev),
        cell.runs
    )
}

fn seconds(value: f64) -> String {
    if value < 0.01 {
        format!("{:.1} ms", value * 1000.0)
    } else if value < 1.0 {
        format!("{:.0} ms", value * 1000.0)
    } else {
        format!("{value:.2} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heavy_scenarios_run_fewer_times_at_scale() {
        assert_eq!(runs_for("root-iface", 100), 5);
        assert_eq!(runs_for("root-iface", 1000), 2);
        assert_eq!(runs_for("cold", 10_000), 2);
        assert_eq!(runs_for("leaf-impl", 10_000), 5);
    }

    #[test]
    fn picks_tools_per_scenario() {
        assert_eq!(tools_for("noop", false), [Tool::Ninja, Tool::CmakeBuild]);
        assert_eq!(tools_for("touch", false), [Tool::Ninja]);
        assert_eq!(tools_for("root-iface", true), [Tool::CmakeBuild, Tool::Yam]);
    }

    #[test]
    fn parses_hyperfine_json() {
        let json = r#"{"results":[{"command":"x","mean":1.5,"stddev":0.25,"min":1.2,"max":1.9}]}"#;
        assert_eq!(parse_hyperfine(json).unwrap(), (1.5, 0.25, 1.2, 1.9));
        let single =
            r#"{"results":[{"command":"x","mean":2.0,"stddev":null,"min":2.0,"max":2.0}]}"#;
        assert_eq!(parse_hyperfine(single).unwrap(), (2.0, 0.0, 2.0, 2.0));
    }

    #[test]
    fn quotes_arguments_with_spaces() {
        assert_eq!(
            join(&["ninja", "-C", "/a b/build"]),
            "ninja -C '/a b/build'"
        );
    }

    #[test]
    fn reports_tables_per_style_and_size() {
        let cell = |scenario: &str, tool, mean| Cell {
            style: Style::Legacy,
            units: 100,
            scenario: scenario.into(),
            tool,
            runs: 5,
            mean,
            stddev: 0.01,
            min: mean,
            max: mean,
        };
        let results = Results {
            machine: [("cores".to_string(), "12".to_string())]
                .into_iter()
                .collect(),
            cells: vec![
                cell("noop", Tool::Ninja, 0.004),
                cell("noop", Tool::CmakeBuild, 0.02),
                cell("cold", Tool::CmakeBuild, 14.33),
            ],
        };
        let text = report(&results);
        assert!(text.contains("| cores | 12 |"), "{text}");
        assert!(text.contains("### legacy — 100 units"), "{text}");
        assert!(
            text.contains("| `cold` | — | 14.33 s ± 10 ms (n=5) | — |"),
            "{text}"
        );
        assert!(
            text.contains("| `noop` | 4.0 ms ± 10 ms (n=5) | 20 ms ± 10 ms (n=5) | — |"),
            "{text}"
        );
        assert!(!text.contains("`touch`"), "{text}");
        let cold = text.find("`cold`").unwrap();
        assert!(cold < text.find("`noop`").unwrap(), "{text}");
    }
}
