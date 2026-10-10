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

const WARNING: Style = Style::new()
    .bold()
    .fg_color(Some(Color::Ansi(AnsiColor::Yellow)));

const NOTE: Style = Style::new()
    .bold()
    .fg_color(Some(Color::Ansi(AnsiColor::Cyan)));

const HELP: Style = Style::new()
    .bold()
    .fg_color(Some(Color::Ansi(AnsiColor::Cyan)));

/// Print a cargo-style status line, e.g. `     Created binary (application) ...`
pub fn status(verb: &str, message: impl fmt::Display) {
    let reset = STATUS.render_reset();
    anstream::println!("{STATUS}{verb:>12}{reset} {message}");
}

/// Print a cargo-style `error: ...` line to stderr.
pub fn error(message: impl fmt::Display) {
    anstream::eprintln!("{}", prefixed(ERROR, "error", message));
}

/// Print a cargo-style `warning: ...` line to stderr.
pub fn warning(message: impl fmt::Display) {
    anstream::eprintln!("{}", prefixed(WARNING, "warning", message));
}

/// Print a cargo-style `note: ...` line to stderr, for context that isn't a problem
/// (e.g. which dependency raised a project's C++ standard).
pub fn note(message: impl fmt::Display) {
    anstream::eprintln!("{}", prefixed(NOTE, "note", message));
}

/// Print a cargo-style `help: ...` line to stderr, suggesting how to fix the
/// preceding error or warning.
pub fn help(message: impl fmt::Display) {
    anstream::eprintln!("{}", prefixed(HELP, "help", message));
}

/// `label: message`, with the label styled. `anstream` strips the styling when color
/// is off.
fn prefixed(style: Style, label: &str, message: impl fmt::Display) -> String {
    let reset = style.render_reset();
    format!("{style}{label}{reset}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> String {
        anstream::adapter::strip_str(text).to_string()
    }

    #[test]
    fn prefixes_read_like_cargo_without_color() {
        assert_eq!(plain(&prefixed(ERROR, "error", "boom")), "error: boom");
        assert_eq!(
            plain(&prefixed(WARNING, "warning", "careful")),
            "warning: careful"
        );
        assert_eq!(plain(&prefixed(NOTE, "note", "fyi")), "note: fyi");
        assert_eq!(plain(&prefixed(HELP, "help", "try this")), "help: try this");
    }

    #[test]
    fn only_the_label_is_styled() {
        let styled = prefixed(WARNING, "warning", "careful");
        assert!(styled.starts_with(&WARNING.render().to_string()));
        assert!(styled.ends_with(": careful"));
    }
}
