//! Benchmark harness for `yam`: generates equivalent `yam` and CMake + ninja projects
//! and applies the edits each benchmark scenario needs. The `hyperfine` runner lands
//! in #19.

mod cmake;
mod graph;
mod project;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use project::{Meta, Scenario, Style};

#[derive(Parser)]
#[command(
    name = "bench",
    about = "Generate and edit yam vs CMake + ninja benchmark projects"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a project with both a Yam.toml and a CMakeLists.txt
    Gen {
        /// Directory to create the project in
        dir: PathBuf,
        /// Modules (`import std`, C++26) or legacy headers (C++17)
        #[arg(long, value_enum)]
        style: Style,
        /// Number of units (100, 1000 and 10000 are the benchmark sizes)
        #[arg(long, default_value_t = 100)]
        units: usize,
        /// Layers above the root unit
        #[arg(long, default_value_t = 8)]
        depth: usize,
        /// Dependencies per unit, on the layer below
        #[arg(long, default_value_t = 3)]
        fanout: usize,
        /// Seed for the dependency graph
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Apply a scenario's change to a generated project
    Edit {
        #[arg(value_enum)]
        scenario: Scenario,
        dir: PathBuf,
    },
    /// Configure the CMake + ninja build in <DIR>/build-cmake
    Configure {
        dir: PathBuf,
        /// Configure a Release build instead of Debug
        #[arg(long)]
        release: bool,
        /// The `CMAKE_EXPERIMENTAL_CXX_IMPORT_STD` value, for CMake versions bench
        /// doesn't know
        #[arg(long, value_name = "UUID")]
        import_std_gate: Option<String>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Gen {
            dir,
            style,
            units,
            depth,
            fanout,
            seed,
        } => {
            anyhow::ensure!(units >= 2, "--units must be at least 2");
            let meta = Meta::new(style, units, depth, fanout, seed);
            project::generate(&dir, &meta)?;
            println!(
                "generated {units} {style:?} units in `{}`: leaf u{}, mid u{} ({} rebuilt), root u0",
                dir.display(),
                meta.leaf,
                meta.mid,
                meta.expected_rebuilt_units["mid-iface"],
            );
        }
        Command::Edit { scenario, dir } => {
            for path in project::edit(&dir, scenario)? {
                println!("changed `{}`", path.display());
            }
        }
        Command::Configure {
            dir,
            release,
            import_std_gate,
        } => {
            let build = cmake::configure(&dir, release, import_std_gate.as_deref())?;
            println!("configured `{}`", build.display());
        }
    }
    Ok(())
}
