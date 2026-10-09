//! `yam` (Yet Another Make): a Cargo-style build tool for modern C++.
//!
//! The `yam` binary is a thin wrapper around [`run`].

pub mod cli;
pub mod commands;
pub mod style;

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, ValueEnum};
use cli::{Cli, ColorWhen, Command};

/// Parse the process arguments, run the selected subcommand, and report its outcome.
///
/// Argument errors, `--help` and `--version` are handled by clap, which exits the
/// process directly.
#[must_use]
pub fn run() -> ExitCode {
    // `--color` needs to be known before the rest of argument parsing runs, so that
    // parse errors and `--help`/`--version` themselves come out correctly colored.
    let color = prescan_color(&std::env::args().collect::<Vec<_>>()).to_clap();
    let matches = build_command(color).get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => err.exit(),
    };

    style::set_color_choice(cli.color);

    let Some(command) = cli.command else {
        let _ = build_command(color).print_help();
        return ExitCode::SUCCESS;
    };

    let result = match command {
        Command::Init(args) => commands::init::run(&args),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            style::error(err);
            ExitCode::FAILURE
        }
    }
}

fn build_command(color: clap::ColorChoice) -> clap::Command {
    let mut command = Cli::command().color(color);
    // Force clap to materialize its auto-generated `help` subcommand so it can be
    // mutated below; it's otherwise only built lazily right before matching.
    command.build();
    command.mut_subcommand("help", |cmd| cmd.about("Displays help for a yam command"))
}

fn prescan_color(args: &[String]) -> ColorWhen {
    let mut iter = args.iter().skip(1);
    let mut raw = None;
    while let Some(arg) = iter.next() {
        if let Some(val) = arg.strip_prefix("--color=") {
            raw = Some(val.to_string());
        } else if arg == "--color" {
            raw = iter.next().cloned();
        }
    }
    raw.and_then(|val| ColorWhen::from_str(&val, true).ok())
        .unwrap_or(ColorWhen::Auto)
}
