//! Path classification used to rank files, and escaping of paths for display.

/// Make a path (or other attacker-controlled name) safe to print on one line of a terminal or an
/// agent transcript. Backslash is doubled first so a literal `\n` in a name stays distinguishable
/// from an escaped newline; CR and LF become `\r` and `\n`; every other control character (C0,
/// DEL, C1, U+2028, U+2029: the same set as `ctrl` in `adapters/claude-code/ctx-select.sh`) becomes
/// `\xNN` (U+2028 and U+2029 become `\u{2028}` and `\u{2029}`). Everything else, including non-ASCII text, is kept.
pub fn escape_control(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\u{00}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{{{:04x}}}", c as u32);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Replace raw terminal control characters with visible escapes, leaving backslashes alone.
/// For a whole line of text that may embed an error's `Display` output (a git, I/O or HTTP error
/// can carry a ref name or a server string). Unlike [`escape_control`] it does not double `\`,
/// because parts of the line are often already escaped (`{:?}` of a path, `escape_control`), and
/// escaping those twice would turn `\x1b` into `\\x1b`. Same character set and spelling as
/// `sanitize_for_terminal` in `src/main.rs` (the bin cannot call a `pub(crate)` item, so the two
/// are kept in step by hand; keep them identical).
pub(crate) fn sanitize_for_terminal(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\u{00}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{{{:04x}}}", c as u32);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Print one warning line to stderr with control characters escaped once. Pass the whole line,
/// prefix included (`warn:`, `warning:`, `semantic:`); nothing is added.
pub(crate) fn warn(line: &str) {
    eprintln!("{}", sanitize_for_terminal(line));
}

/// Score multiplier for test and fixture files (see [`path_score_factor`]).
pub const TEST_PATH_FACTOR: f32 = 0.5;

/// True for test, fixture and spec files: files under a `tests/`, `test/`, `__tests__/`,
/// `fixtures/`, `spec/`, `specs/` or `testdata/` directory, and files named `test_*`,
/// `*_test.*`, `*_spec.*`, `*.test.*` or `*.spec.*`.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let in_test_dir = lower
        .split('/')
        .rev()
        .skip(1) // the file name itself is not a directory
        .any(|d| {
            matches!(
                d,
                "tests" | "test" | "__tests__" | "fixtures" | "spec" | "specs" | "testdata"
            )
        });
    in_test_dir
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains("_spec.")
        || name.contains(".test.")
        || name.contains(".spec.")
}

/// Multiplier applied to a file's search score. Test files are halved, not dropped: they name
/// the same things as the code under test, so they otherwise crowd the real module out of the
/// top results (#160).
pub fn path_score_factor(path: &str) -> f32 {
    if is_test_path(path) {
        TEST_PATH_FACTOR
    } else {
        1.0
    }
}

/// The set `ctx-select.sh` replaces: `[\x00-\x1f\x7f-\x9f\u2028\u2029]`.
#[cfg(test)]
pub(crate) fn is_ctrl(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{2028}' | '\u{2029}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_for_terminal_escapes_every_control_and_keeps_backslashes() {
        let all: String = ('\u{0}'..='\u{9f}')
            .filter(|c| c.is_control())
            .chain(['\u{2028}', '\u{2029}'])
            .collect();
        let out = sanitize_for_terminal(&all);
        assert!(!out.chars().any(is_ctrl), "{out:?}");
        assert_eq!(
            sanitize_for_terminal("a\u{1b}[2Jb\u{9b}c\r\nd\u{2028}"),
            "a\\x1b[2Jb\\x9bc\\r\\nd\\u{2028}"
        );
        // Text that is already escaped passes through unchanged (no `\\x1b`).
        let hostile = "d\u{1b}[2J/a\u{9b}b";
        let already = escape_control(hostile);
        assert_eq!(sanitize_for_terminal(&already), already);
        let dbg = format!("{hostile:?}");
        assert_eq!(sanitize_for_terminal(&dbg), dbg);
        assert_eq!(
            sanitize_for_terminal("plain \u{e9} text"),
            "plain \u{e9} text"
        );
    }

    #[test]
    fn escape_control_leaves_plain_and_non_ascii_text() {
        assert_eq!(
            escape_control("src/a b/\u{e9}\u{4e2d}.rs"),
            "src/a b/\u{e9}\u{4e2d}.rs"
        );
        // U+00A0 (first char after C1) and U+2027 / U+202A are not in the escaped set.
        assert_eq!(
            escape_control("\u{a0}\u{2027}\u{202a}"),
            "\u{a0}\u{2027}\u{202a}"
        );
    }

    #[test]
    fn escape_control_keeps_newline_and_backslash_escapes_compatible() {
        assert_eq!(escape_control("a\nb\rc"), "a\\nb\\rc");
        assert_eq!(escape_control("a\\nb"), "a\\\\nb");
        assert_ne!(escape_control("a\\nb"), escape_control("a\nb"));
    }

    #[test]
    fn escape_control_escapes_every_c0_del_c1_and_line_separator() {
        let mut all: Vec<char> = ('\u{0}'..='\u{1f}').collect();
        all.extend('\u{7f}'..='\u{9f}');
        all.push('\u{2028}');
        all.push('\u{2029}');
        for c in all {
            let out = escape_control(&format!("a{c}b"));
            assert!(
                out.chars().all(|o| !is_ctrl(o)) && out.starts_with('a') && out.ends_with('b'),
                "U+{:04X} -> {out:?}",
                c as u32
            );
        }
        assert_eq!(escape_control("a\u{1b}]0;X\u{7}b"), "a\\x1b]0;X\\x07b");
        assert_eq!(escape_control("\u{7f}\u{85}\u{9f}"), "\\x7f\\x85\\x9f");
        assert_eq!(escape_control("\u{2028}\u{2029}"), "\\u{2028}\\u{2029}");
    }

    #[test]
    fn test_directories_are_tests() {
        for p in [
            "tests/test_command_parse.py",
            "test/foo.rs",
            "src/poetry/tests/helpers.py",
            "packages/x/__tests__/a.ts",
            "tests/fixtures/git/pkg/__init__.py",
            "Tests/Foo.rs",        // case-insensitive
            "spec/models/user.rb", // Ruby (RSpec)
            "pkg/specs/helper.rb",
            "pkg/parser/testdata/input.txt", // Go
        ] {
            assert!(is_test_path(p), "{p} should be a test path");
        }
    }

    #[test]
    fn test_file_names_are_tests() {
        for p in [
            "scrapy/utils/test_helpers.py",
            "src/foo_test.go",
            "src/foo_test.rs",
            "src/app.test.ts",
            "src/app.spec.tsx",
            "app/models/user_spec.rb", // Ruby (RSpec)
        ] {
            assert!(is_test_path(p), "{p} should be a test path");
        }
    }

    #[test]
    fn source_files_are_not_tests() {
        for p in [
            "src/poetry/console/commands/source/add.py",
            "src/main.rs",
            "scrapy/core/engine.py",
            "src/contest/mod.rs",     // "test" inside a word is not a test dir
            "src/latest.py",          // not test_*
            "src/protest_handler.ts", // not *.test.*
            "docs/testing-guide.md",  // not under tests/ and not test_*
            "src/attestation/mod.rs",
            "src/inspect_specimen.rs",  // not *_spec.*
            "src/specification/mod.rs", // "spec" inside a word is not a spec dir
        ] {
            assert!(!is_test_path(p), "{p} should not be a test path");
        }
    }

    #[test]
    fn factor_halves_tests_and_keeps_sources() {
        assert_eq!(path_score_factor("tests/test_a.py"), 0.5);
        assert_eq!(path_score_factor("src/a.py"), 1.0);
    }
}
