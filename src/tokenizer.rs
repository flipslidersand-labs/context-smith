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
}
