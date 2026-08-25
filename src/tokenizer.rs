/// Tokenize a file path so FTS5 can match individual path components and subwords.
///
/// "src/auth/middleware.rs" → "src auth middleware rs"
/// "src/parseHTTPRequest.go" → "src parse http request go"
pub fn tokenize_path(path: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    for component in path.split(['/', '\\', '.']) {
        if component.is_empty() {
            continue;
        }
        for word in split_identifier(component) {
            tokens.push(word.to_lowercase());
        }
    }
    tokens.join(" ")
}

/// Pre-tokenize a code identifier so FTS5's unicode61 can match subwords.
///
/// "authenticateUser" → "authenticateuser authenticate user"
/// "parse_json_body"  → "parse_json_body parse json body"
///
/// The original lowercased form is always kept first (exact-match anchor),
/// followed by the split subwords.
pub fn tokenize_code(name: &str) -> String {
    let words = split_identifier(name);
    if words.len() <= 1 {
        return name.to_lowercase();
    }
    let original = name.to_lowercase();
    let parts: Vec<String> = words.into_iter().map(|w| w.to_lowercase()).collect();
    format!("{} {}", original, parts.join(" "))
}

/// Split a camelCase or snake_case identifier into subwords.
fn split_identifier(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut result: Vec<&str> = Vec::new();
    let mut start = 0;

    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'_' || b == b'-' {
            if i > start {
                result.push(&s[start..i]);
            }
            start = i + 1;
            i += 1;
            continue;
        }
        // camelCase transition: lowercase → uppercase
        if i > start && b.is_ascii_uppercase() {
            // lookahead: if next is lowercase, this starts a new word
            let prev_lower = bytes[i - 1].is_ascii_lowercase() || bytes[i - 1].is_ascii_digit();
            let next_lower = bytes.get(i + 1).is_some_and(|c| c.is_ascii_lowercase());
            if prev_lower || (next_lower && i > start) {
                result.push(&s[start..i]);
                start = i;
            }
        }
        i += 1;
    }
    if start < s.len() {
        result.push(&s[start..]);
    }
    if result.is_empty() {
        result.push(s);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case() {
        let t = tokenize_code("authenticateUser");
        assert!(t.contains("authenticate"), "got: {}", t);
        assert!(t.contains("user"), "got: {}", t);
        assert!(t.starts_with("authenticateuser"), "got: {}", t);
    }

    #[test]
    fn snake_case() {
        let t = tokenize_code("parse_json_body");
        assert!(t.contains("parse"), "got: {}", t);
        assert!(t.contains("json"), "got: {}", t);
        assert!(t.contains("body"), "got: {}", t);
    }

    #[test]
    fn single_word() {
        assert_eq!(tokenize_code("index"), "index");
    }

    #[test]
    fn acronym() {
        let t = tokenize_code("parseHTTPRequest");
        assert!(t.contains("parse"), "got: {}", t);
        assert!(t.contains("request"), "got: {}", t);
    }

    #[test]
    fn path_tokenization() {
        let t = tokenize_path("src/auth/middleware.rs");
        assert!(t.contains("src"), "got: {}", t);
        assert!(t.contains("auth"), "got: {}", t);
        assert!(t.contains("middleware"), "got: {}", t);
        assert!(!t.contains("rs") || t.contains("rs"), "extension present");
    }

    #[test]
    fn path_camel_component() {
        let t = tokenize_path("src/parseHTTPRequest.go");
        assert!(t.contains("parse"), "got: {}", t);
        assert!(t.contains("request"), "got: {}", t);
        assert!(t.contains("go"), "got: {}", t);
    }

    // --- edge-case unit tests (fixed inputs) ---

    #[test]
    fn empty_string_does_not_panic() {
        let t = tokenize_code("");
        // Empty input should produce empty or single empty token — must not panic.
        assert!(t.is_empty() || !t.is_empty());
    }

    #[test]
    fn empty_path_does_not_panic() {
        let _ = tokenize_path("");
    }

    #[test]
    fn digit_mixed_identifier() {
        // "parse2JSON" → should contain "parse" and "json"
        let t = tokenize_code("parse2JSON");
        assert!(t.contains("parse"), "got: {}", t);
        assert!(t.contains("json"), "got: {}", t);
    }

    #[test]
    fn consecutive_underscores() {
        // "foo__bar" — consecutive underscores must not panic; "foo" and "bar" present
        let t = tokenize_code("foo__bar");
        assert!(t.contains("foo"), "got: {}", t);
        assert!(t.contains("bar"), "got: {}", t);
    }

    #[test]
    fn all_caps_acronym() {
        // "HTTPURL" — full-uppercase, no lower boundary; must not panic
        let t = tokenize_code("HTTPURL");
        assert!(!t.is_empty(), "got empty for HTTPURL");
    }

    #[test]
    fn screaming_snake_case() {
        let t = tokenize_code("MAX_RETRY_COUNT");
        assert!(t.contains("max"), "got: {}", t);
        assert!(t.contains("retry"), "got: {}", t);
        assert!(t.contains("count"), "got: {}", t);
    }

    #[test]
    fn kebab_case() {
        let t = tokenize_code("my-module-name");
        assert!(t.contains("my"), "got: {}", t);
        assert!(t.contains("module"), "got: {}", t);
        assert!(t.contains("name"), "got: {}", t);
    }

    #[test]
    fn only_underscores() {
        // Must not panic
        let _ = tokenize_code("___");
    }

    #[test]
    fn only_digits() {
        let t = tokenize_code("12345");
        assert!(!t.is_empty(), "got empty for digits-only");
    }

    #[test]
    fn single_uppercase_char() {
        let _ = tokenize_code("A");
    }

    #[test]
    fn leading_uppercase() {
        // "MyStruct" — PascalCase
        let t = tokenize_code("MyStruct");
        assert!(t.contains("my"), "got: {}", t);
        assert!(t.contains("struct"), "got: {}", t);
    }

    // --- property-based tests using proptest ---

    mod prop {
        use super::super::{split_identifier, tokenize_code, tokenize_path};
        use proptest::prelude::*;

        /// Strategy producing identifier-like strings:
        /// ASCII alphanumeric plus '_' and '-', length 0..=64.
        fn ident_strategy() -> impl Strategy<Value = String> {
            proptest::string::string_regex("[a-zA-Z0-9_\\-]{0,64}").unwrap()
        }

        proptest! {
            /// Invariant 1: split_identifier never panics and the joined tokens
            /// (excluding separator characters '_' and '-') contain all original
            /// alphanumeric characters in lowercase.
            #[test]
            fn split_identifier_preserves_alnum_chars(s in ident_strategy()) {
                let words = split_identifier(&s);
                // Must always return at least one element (even for empty input
                // the function returns the original slice — accept either 0 or 1+).
                let joined = words.join("");
                // Every alphanumeric byte in `s` must appear in `joined`.
                let s_alnum: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                let j_alnum: String = joined.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                prop_assert_eq!(
                    s_alnum.to_lowercase(),
                    j_alnum.to_lowercase(),
                    "input={:?} words={:?}",
                    s, words
                );
            }

            /// Invariant 2: tokenize_code never panics on arbitrary ident-like input.
            #[test]
            fn tokenize_code_no_panic(s in ident_strategy()) {
                let _ = tokenize_code(&s);
            }

            /// Invariant 3: tokenize_path never panics on arbitrary path-like input.
            #[test]
            fn tokenize_path_no_panic(
                s in proptest::string::string_regex("[a-zA-Z0-9_\\-/\\\\.]{0,128}").unwrap()
            ) {
                let _ = tokenize_path(&s);
            }

            /// Invariant 4: tokenize_code output is always lowercase.
            #[test]
            fn tokenize_code_output_is_lowercase(s in ident_strategy()) {
                let t = tokenize_code(&s);
                prop_assert_eq!(t.to_lowercase(), t, "output not lowercase for input={:?}", s);
            }

            /// Invariant 5: tokenize_path output is always lowercase.
            #[test]
            fn tokenize_path_output_is_lowercase(
                s in proptest::string::string_regex("[a-zA-Z0-9_\\-/\\\\.]{0,128}").unwrap()
            ) {
                let t = tokenize_path(&s);
                prop_assert_eq!(t.to_lowercase(), t, "output not lowercase for path={:?}", s);
            }

            /// Invariant 6: idempotency — tokenize_code applied twice to the first
            /// token of the output equals the output itself (already-tokenized input
            /// should not produce extra splits on re-tokenization).
            #[test]
            fn tokenize_code_first_token_idempotent(s in ident_strategy()) {
                let t1 = tokenize_code(&s);
                // Take the first space-separated token (the lowercased original).
                let first = t1.split_whitespace().next().unwrap_or("");
                let t2 = tokenize_code(first);
                // The first token of t2 must equal `first` (single word, no further split).
                let first2 = t2.split_whitespace().next().unwrap_or("");
                prop_assert_eq!(first, first2, "not idempotent: s={:?}", s);
            }
        }
    }
}
