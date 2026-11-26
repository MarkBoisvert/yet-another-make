//! Whether a source file defines `main()`, so a bin with two entry files is a clear
//! error instead of a duplicate-symbol link failure.
//!
//! `clang-scan-deps` doesn't report this, so it's read from the file's text: comments,
//! string literals and preprocessor directives are skipped, then `int main(` or
//! `auto main(` is looked for. `#if` isn't evaluated.

/// Whether `source`, a file's text, seems to define `main()`.
#[must_use]
pub fn defines_main(source: &str) -> bool {
    let code = strip(source);
    let tokens = tokenize(&code);
    tokens.windows(3).enumerate().any(|(i, window)| {
        matches!(window, ["int" | "auto", "main", "("])
            && !(i > 0 && matches!(tokens[i - 1], "." | "->" | ":"))
    })
}

/// Identifiers and numbers as tokens; every other non-space character is a token of
/// its own.
fn tokenize(code: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut chars = code.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        let mut end = start + c.len_utf8();
        if c.is_alphanumeric() || c == '_' {
            while let Some(&(i, next)) = chars.peek() {
                if !(next.is_alphanumeric() || next == '_') {
                    break;
                }
                end = i + next.len_utf8();
                chars.next();
            }
        }
        tokens.push(&code[start..end]);
    }
    tokens
}

/// `source` with comments and preprocessor directives blanked out, and each string
/// or character literal replaced by its opening quote. Newlines are kept.
fn strip(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut line_start = true;
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                out.push('\n');
                line_start = true;
                continue;
            }
            '/' if chars.peek() == Some(&'/') => {
                skip_line(&mut chars, &mut out, false);
                line_start = true;
                continue;
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                    }
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                out.push(' ');
                continue;
            }
            '#' if line_start => {
                skip_line(&mut chars, &mut out, true);
                line_start = true;
                continue;
            }
            '"' if out.ends_with('R') => {
                skip_raw_string(&mut chars);
                out.push('"');
            }
            '"' | '\'' => {
                let mut escaped = false;
                for next in chars.by_ref() {
                    if next == '\n' {
                        break;
                    }
                    if !escaped && next == c {
                        break;
                    }
                    escaped = !escaped && next == '\\';
                }
                out.push(c);
            }
            c => out.push(c),
        }
        if !c.is_whitespace() {
            line_start = false;
        }
    }
    out
}

/// Skip to the end of the line, following `\` continuations when `continued` is set
/// (preprocessor directives). The newline itself is written to `out`.
fn skip_line(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    out: &mut String,
    continued: bool,
) {
    let mut last = ' ';
    for c in chars.by_ref() {
        if c == '\n' {
            out.push('\n');
            if !(continued && last == '\\') {
                return;
            }
        }
        if c != '\r' {
            last = c;
        }
    }
}

/// Skip a raw string's body, `delim( … )delim"`, after its opening quote.
fn skip_raw_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let mut delimiter = String::new();
    for c in chars.by_ref() {
        if c == '(' {
            break;
        }
        delimiter.push(c);
    }
    let close: Vec<char> = format!("){delimiter}\"").chars().collect();
    let mut window: Vec<char> = Vec::with_capacity(close.len());
    for c in chars.by_ref() {
        if window.len() == close.len() {
            window.remove(0);
        }
        window.push(c);
        if window == close {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_main_but_not_members_or_strings() {
        assert!(defines_main("int main() {}"));
        assert!(defines_main("auto main() -> int { return 0; }"));
        assert!(defines_main(
            "#include <cstdio>\n\nint main(int argc, char** argv) {\n  std::printf(\"hi\");\n}\n"
        ));
        assert!(!defines_main("int x = app.main();"));
        assert!(!defines_main("int App::main(int) { return 0; }"));
        assert!(!defines_main("const char* s = \"int main(\";"));
        assert!(!defines_main("// int main() {}\n/* int main() */"));
        assert!(!defines_main("int domain(int);"));
    }

    #[test]
    fn directives_and_raw_strings_are_skipped() {
        assert!(!defines_main("#define ENTRY \\\n  int main() {}\n"));
        assert!(!defines_main("auto s = R\"x(int main() { \")\" }\n)x\";\n"));
    }
}
