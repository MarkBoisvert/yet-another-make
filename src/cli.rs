use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Color, Style, Styles};
use clap::{Args, Parser, Subcommand, ValueEnum};

/// Mirrors the color palette cargo uses for its own `--help` output.
const STYLES: Styles = Styles::styled()
    .header(
        Style::new()
            .bold()
            .fg_color(Some(Color::Ansi(AnsiColor::Green))),
    )
    .usage(
        Style::new()
            .bold()
            .fg_color(Some(Color::Ansi(AnsiColor::Green))),
    )
    .literal(
        Style::new()
            .bold()
            .fg_color(Some(Color::Ansi(AnsiColor::Cyan))),
    )
    .placeholder(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan))))
    .error(
        Style::new()
            .bold()
            .fg_color(Some(Color::Ansi(AnsiColor::Red))),
    )
    .valid(
        Style::new()
            .bold()
            .fg_color(Some(Color::Ansi(AnsiColor::Cyan))),
    )
    .invalid(
        Style::new()
            .bold()
            .fg_color(Some(Color::Ansi(AnsiColor::Yellow))),
    );

#[derive(Parser)]
#[command(
    name = "yam",
    version,
    about = "Yet Another Make",
    styles = STYLES
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Coloring
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "auto",
        hide_default_value = true,
        value_name = "WHEN"
    )]
    pub color: ColorWhen,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ColorWhen {
    Auto,
    Always,
    Never,
}

impl ColorWhen {
    pub fn to_clap(self) -> clap::ColorChoice {
        match self {
            Self::Auto => clap::ColorChoice::Auto,
            Self::Always => clap::ColorChoice::Always,
            Self::Never => clap::ColorChoice::Never,
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Initialize a new project
    Init(InitArgs),
}

#[derive(Args)]
pub struct InitArgs {
    /// Path to initialize the package in
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Use a binary (application) template [default]
    #[arg(long, conflicts_with = "lib")]
    pub bin: bool,

    /// Use a library template
    #[arg(long)]
    pub lib: bool,

    /// Set the package name (defaults to the directory name)
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Use traditional header includes instead of C++ modules
    #[arg(long)]
    pub legacy: bool,

    /// Initialize a version control repository for the given version control system, overriding auto-detection of an existing repository
    #[arg(long, value_enum, value_name = "VCS")]
    pub vcs: Option<Vcs>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Vcs {
    Git,
    None,
}
