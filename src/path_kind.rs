//! Path classification used to rank files.

/// Score multiplier for test and fixture files (see [`path_score_factor`]).
pub const TEST_PATH_FACTOR: f32 = 0.5;

/// True for test, fixture and spec files: files under a `tests/`, `test/`, `__tests__/` or
/// `fixtures/` directory, and files named `test_*`, `*_test.*`, `*.test.*` or `*.spec.*`.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let in_test_dir = lower
        .split('/')
        .rev()
        .skip(1) // the file name itself is not a directory
        .any(|d| matches!(d, "tests" | "test" | "__tests__" | "fixtures"));
    in_test_dir
        || name.starts_with("test_")
        || name.contains("_test.")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_directories_are_tests() {
        for p in [
            "tests/test_command_parse.py",
            "test/foo.rs",
            "src/poetry/tests/helpers.py",
            "packages/x/__tests__/a.ts",
            "tests/fixtures/git/pkg/__init__.py",
            "Tests/Foo.rs", // case-insensitive
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
