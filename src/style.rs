use std::fmt;

use anstyle::{AnsiColor, Color, Style};

use crate::cli::ColorWhen;

/// Apply `--color <WHEN>` to this process's output streams.
pub fn set_color_choice(choice: ColorWhen) {
    let choice = match choice {
        ColorWhen::Auto => anstream::ColorChoice::Auto,
        ColorWhen::Always => anstream::ColorChoice::Always,
        ColorWhen::Never => anstream::ColorChoice::Never,
    };
    choice.write_global();
}

const STATUS: Style = Style::new()
    .bold()
    .fg_color(Some(Color::Ansi(AnsiColor::Green)));

const ERROR: Style = Style::new()
    .bold()
    .fg_color(Some(Color::Ansi(AnsiColor::Red)));

/// Print a cargo-style status line, e.g. `     Created binary (application) ...`
pub fn status(verb: &str, message: impl fmt::Display) {
    let reset = STATUS.render_reset();
    anstream::println!("{STATUS}{verb:>12}{reset} {message}");
}

/// Print a cargo-style `error: ...` line to stderr.
pub fn error(message: impl fmt::Display) {
    let reset = ERROR.render_reset();
    anstream::eprintln!("{ERROR}error{reset}: {message}");
}
