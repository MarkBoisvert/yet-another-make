//! A textual reader for the module declarations and imports at the top of a C++
//! source file, and for a `main()` definition.
//!
//! This is a stopgap until batched `clang-scan-deps -format=p1689` (#21) reports
//! provides and requires. It skips comments, string literals and preprocessor
//! directives, then reads the file's preamble: `module;`, `export module m;`,
//! `module m;`, `module m:part;` and `export import`/`import` declarations, stopping
//! at the first other declaration. It doesn't evaluate `#if`, so a module declaration
//! inside a conditional is read as if the condition held.

/// What a source file declares and imports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub module: Option<ModuleDecl>,
    /// Imports in order. Partition imports (`import :p;`) are already qualified with
    /// the file's own module name, as `m:p`.
    pub imports: Vec<Import>,
    /// Whether the file seems to define `main()`.
    pub defines_main: bool,
}

/// A file's module declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleDecl {
    /// `export module m;` or `export module m:part;`: an interface unit.
    Interface(String),
    /// `module m:part;`: an internal partition, which other units of `m` import.
    Partition(String),
    /// `module m;`: an implementation unit, which implicitly imports `m`.
    Implementation(String),
}

impl ModuleDecl {
    /// The module this file belongs to, without any partition: `m` for `m:part`.
    #[must_use]
    pub fn primary(&self) -> &str {
        let (Self::Interface(name) | Self::Partition(name) | Self::Implementation(name)) = self;
        name.split(':').next().unwrap_or_default()
    }

    /// The module or partition this file provides, if any.
    #[must_use]
    pub fn provides(&self) -> Option<&str> {
        match self {
            Self::Interface(name) | Self::Partition(name) => Some(name),
            Self::Implementation(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Import {
    /// `import m;`, `import m:p;` or (qualified) `import :p;`.
    Module(String),
    /// `import <header>;` or `import "header";`.
    HeaderUnit,
}

impl Scan {
    /// Read `source`, a file's text.
    #[must_use]
    pub fn of(source: &str) -> Self {
        let code = strip(source);
        let tokens = tokenize(&code);
        let mut scan = Self {
            defines_main: defines_main(&tokens),
            ..Self::default()
        };
        scan.read_preamble(&tokens);
        scan
    }

    fn read_preamble(&mut self, tokens: &[&str]) {
        let mut rest = tokens;
        loop {
            let (exported, statement) = match rest {
                ["export", tail @ ..] => (true, tail),
                tail => (false, tail),
            };
            match statement {
                ["module", ";", tail @ ..] if !exported => rest = tail,
                ["module", tail @ ..] => {
                    let Some((name, tail)) = module_name(tail) else {
                        return;
                    };
                    let Some(tail) = tail.strip_prefix(&[";"]) else {
                        return;
                    };
                    self.module = Some(match (exported, name.contains(':')) {
                        (true, _) => ModuleDecl::Interface(name),
                        (false, true) => ModuleDecl::Partition(name),
                        (false, false) => ModuleDecl::Implementation(name),
                    });
                    rest = tail;
                }
                ["import", tail @ ..] => {
                    let Some((import, tail)) = self.import(tail) else {
                        return;
                    };
                    self.imports.push(import);
                    rest = tail;
                }
                _ => return,
            }
        }
    }

    /// The import after an `import` keyword, and the tokens after its `;`.
    fn import<'t>(&self, tokens: &'t [&'t str]) -> Option<(Import, &'t [&'t str])> {
        let (import, tail) = match tokens {
            ["<" | "\"", ..] => {
                let end = tokens.iter().position(|t| *t == ";")?;
                return Some((Import::HeaderUnit, &tokens[end + 1..]));
            }
            [":", tail @ ..] => {
                let (partition, tail) = dotted(tail)?;
                let primary = self.module.as_ref().map_or("", ModuleDecl::primary);
                (Import::Module(format!("{primary}:{partition}")), tail)
            }
            tail => {
                let (name, tail) = module_name(tail)?;
                (Import::Module(name), tail)
            }
        };
        Some((import, tail.strip_prefix(&[";"])?))
    }
}

/// `a.b` or `a.b:c.d`, and the tokens after it.
fn module_name<'t>(tokens: &'t [&'t str]) -> Option<(String, &'t [&'t str])> {
    let (mut name, tail) = dotted(tokens)?;
    if let [":", after @ ..] = tail {
        let (partition, tail) = dotted(after)?;
        name.push(':');
        name.push_str(&partition);
        return Some((name, tail));
    }
    Some((name, tail))
}

/// Identifiers joined by `.`, and the tokens after them.
fn dotted<'t>(tokens: &'t [&'t str]) -> Option<(String, &'t [&'t str])> {
    let (first, mut tail) = tokens.split_first()?;
    if !is_identifier(first) {
        return None;
    }
    let mut name = (*first).to_string();
    while let [".", part, after @ ..] = tail
        && is_identifier(part)
    {
        name.push('.');
        name.push_str(part);
        tail = after;
    }
    Some((name, tail))
}

fn is_identifier(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
}

/// `int main(` or `auto main(`, not as a member (`x.main(`, `X::main(`).
fn defines_main(tokens: &[&str]) -> bool {
    tokens.windows(3).enumerate().any(|(i, window)| {
        matches!(window, ["int" | "auto", "main", "("])
            && !(i > 0 && matches!(tokens[i - 1], "." | "->" | ":"))
    })
}

/// Identifiers and numbers as tokens; every other non-space character is a token of
/// its own. `::` comes out as two `:` tokens, which is all the preamble needs.
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

    fn imports(scan: &Scan) -> Vec<&str> {
        scan.imports
            .iter()
            .map(|import| match import {
                Import::Module(name) => name.as_str(),
                Import::HeaderUnit => "<header>",
            })
            .collect()
    }

    #[test]
    fn reads_an_interface_and_its_imports() {
        let scan = Scan::of(
            "export module u0005;\n\nimport std;\nimport u0004;\nexport import a.b:c;\n\nexport namespace u0005 {}\nimport late;\n",
        );
        assert_eq!(scan.module, Some(ModuleDecl::Interface("u0005".into())));
        assert_eq!(imports(&scan), ["std", "u0004", "a.b:c"]);
        assert!(!scan.defines_main);
    }

    #[test]
    fn reads_the_global_module_fragment_and_implementation_units() {
        let scan = Scan::of(
            "module;\n#include <cstdio>\n#define X \\\n  import nope;\nmodule app.core;\nimport :util;\n",
        );
        assert_eq!(
            scan.module,
            Some(ModuleDecl::Implementation("app.core".into()))
        );
        assert_eq!(imports(&scan), ["app.core:util"]);
    }

    #[test]
    fn reads_partitions() {
        let scan = Scan::of("module m:detail;\nimport :other;\n");
        assert_eq!(scan.module, Some(ModuleDecl::Partition("m:detail".into())));
        assert_eq!(imports(&scan), ["m:other"]);
        let scan = Scan::of("export module m:api;\n");
        assert_eq!(scan.module, Some(ModuleDecl::Interface("m:api".into())));
    }

    #[test]
    fn ignores_comments_strings_and_directives() {
        let scan = Scan::of(
            "// export module fake;\n/* module\n other; */\n#include \"x.h\" // import y;\nimport std;\nauto s = \"import z;\";\n",
        );
        assert_eq!(scan.module, None);
        assert_eq!(imports(&scan), ["std"]);
    }

    #[test]
    fn legacy_files_have_no_module_declarations() {
        let scan = Scan::of(
            "#include <cstdio>\n#include \"lib.hpp\"\n\nint main(int argc, char** argv) {\n  std::printf(\"hi\");\n}\n",
        );
        assert_eq!(
            scan,
            Scan {
                module: None,
                imports: vec![],
                defines_main: true,
            }
        );
    }

    #[test]
    fn header_units_are_reported() {
        let scan = Scan::of("import <vector>;\nimport \"config.h\";\nimport std;\n");
        assert_eq!(imports(&scan), ["<header>", "<header>", "std"]);
    }

    #[test]
    fn import_as_an_ordinary_identifier_ends_the_preamble() {
        let scan = Scan::of("void import(int);\nimport std;\n");
        assert_eq!(imports(&scan), Vec::<&str>::new());
    }

    #[test]
    fn finds_main_but_not_members_or_strings() {
        assert!(Scan::of("int main() {}").defines_main);
        assert!(Scan::of("auto main() -> int { return 0; }").defines_main);
        assert!(!Scan::of("int x = app.main();").defines_main);
        assert!(!Scan::of("int App::main(int) { return 0; }").defines_main);
        assert!(!Scan::of("const char* s = \"int main(\";").defines_main);
        assert!(!Scan::of("// int main() {}\n/* int main() */").defines_main);
        assert!(!Scan::of("int domain(int);").defines_main);
    }

    #[test]
    fn raw_strings_are_skipped() {
        let scan = Scan::of("auto s = R\"x(int main() { \")\" }\n)x\";\n");
        assert!(!scan.defines_main);
    }
}
