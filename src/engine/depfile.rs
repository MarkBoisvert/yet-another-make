//! Reading the Makefile-syntax depfiles Clang writes with `-MMD -MF`.
//!
//! A depfile is one or more `target: dep dep \` rules. Lines continue with a trailing
//! `\`, a space inside a path is written `\ `, `#` as `\#` and `$` as `$$`. Any other
//! backslash is part of the path, as in a Windows path like `C:\src\a.h`.

/// Every dependency in the depfile `text`, in order, without duplicates. The targets
/// (the words ending in `:`) aren't included.
#[must_use]
pub fn parse(text: &str) -> Vec<String> {
    let mut deps: Vec<String> = Vec::new();
    let mut in_rule = false;
    for (word, line_end) in words(text) {
        if !in_rule {
            in_rule = word.ends_with(':');
        } else if !deps.contains(&word) {
            deps.push(word);
        }
        if line_end {
            in_rule = false;
        }
    }
    deps
}

/// The depfile's words, each with whether it ends a logical line.
fn words(text: &str) -> Vec<(String, bool)> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some('\n') => {
                    chars.next();
                    finish(&mut word, &mut words, false);
                }
                Some('\r') => {
                    chars.next();
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    finish(&mut word, &mut words, false);
                }
                Some(&next @ (' ' | '#' | '\\')) => {
                    chars.next();
                    word.push(next);
                }
                _ => word.push('\\'),
            },
            '$' if chars.peek() == Some(&'$') => {
                chars.next();
                word.push('$');
            }
            '\n' => finish(&mut word, &mut words, true),
            ' ' | '\t' | '\r' => finish(&mut word, &mut words, false),
            c => word.push(c),
        }
    }
    finish(&mut word, &mut words, true);
    words
}

/// End the current word, if any. At a line end, mark the line's last word.
fn finish(word: &mut String, words: &mut Vec<(String, bool)>, line_end: bool) {
    if !word.is_empty() {
        words.push((std::mem::take(word), line_end));
    } else if line_end && let Some(last) = words.last_mut() {
        last.1 = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_clang_output() {
        let text = "/p/obj/a.cpp.o: src/a.cpp src/a.hpp \\\n  src/b.hpp \\\n  include/c.hpp\n";
        assert_eq!(
            parse(text),
            ["src/a.cpp", "src/a.hpp", "src/b.hpp", "include/c.hpp"]
        );
    }

    #[test]
    fn unescapes_spaces_hashes_and_dollars() {
        let text = "a.o: my\\ dir/a.h odd\\#name.h price$$.h\n";
        assert_eq!(parse(text), ["my dir/a.h", "odd#name.h", "price$.h"]);
    }

    #[test]
    fn keeps_windows_backslashes_and_crlf() {
        let text = "C:\\p\\a.o: C:\\p\\a.cpp \\\r\n  C:\\p\\inc\\a.h\r\n";
        assert_eq!(parse(text), ["C:\\p\\a.cpp", "C:\\p\\inc\\a.h"]);
    }

    #[test]
    fn merges_several_rules_without_duplicates() {
        let text = "a.o: a.cpp x.h\nb.o: b.cpp x.h\n\nx.h:\n";
        assert_eq!(parse(text), ["a.cpp", "x.h", "b.cpp"]);
    }

    #[test]
    fn empty_or_target_only_depfiles_have_no_deps() {
        assert_eq!(parse(""), Vec::<String>::new());
        assert_eq!(parse("a.o:\n"), Vec::<String>::new());
    }
}
