use std::path::PathBuf;

/// A file candidate for bundle inclusion, scored and sized.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub file_id: i64,
    pub path: PathBuf,
    pub score: f32,
    pub content: String,
}

impl Candidate {
    /// Estimated token count of the file content (see [`estimate_tokens`]).
    pub fn tokens(&self) -> usize {
        estimate_tokens(&self.content)
    }
}

/// Estimate the token count of a string with a conservative, model-agnostic heuristic.
///
/// English and source code (ASCII) average roughly 4 characters per token, so ASCII
/// characters are counted at 1/4 each — matching the previous `chars / 4` behaviour.
/// Multi-byte characters (CJK, emoji, …) tokenize far more densely — frequently around
/// one token per character — so each is counted as a full token. Biasing the non-ASCII
/// case upward keeps the estimate on the safe side of the budget, so bundles containing
/// Japanese comments or identifiers never silently overflow the real token limit.
///
/// This intentionally avoids a model-specific BPE tokenizer (e.g. tiktoken) to preserve
/// context-smith's offline, zero-heavy-dependency design; the goal here is a safe upper
/// bound for budget allocation, not exact per-model counts.
pub fn estimate_tokens(content: &str) -> usize {
    let mut ascii = 0usize;
    let mut wide = 0usize;
    for ch in content.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    let tokens = ascii / 4 + wide;
    if content.is_empty() {
        0
    } else {
        tokens.max(1)
    }
}

/// Take the longest prefix of `content` whose estimated token count does not exceed
/// `max_tokens`. Cuts on a UTF-8 char boundary and uses the same weighting as
/// [`estimate_tokens`], so ASCII and multi-byte content are truncated consistently.
fn truncate_to_tokens(content: &str, max_tokens: usize) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let mut ascii = 0usize;
    let mut wide = 0usize;
    let mut end = 0usize;
    for (i, ch) in content.char_indices() {
        let (na, nw) = if ch.is_ascii() {
            (ascii + 1, wide)
        } else {
            (ascii, wide + 1)
        };
        if na / 4 + nw > max_tokens {
            break;
        }
        ascii = na;
        wide = nw;
        end = i + ch.len_utf8();
    }
    content[..end].to_string()
}

/// Greedy budget allocation: sort by score descending, include until budget exhausted.
/// Returns selected candidates in score order.
pub fn allocate(mut candidates: Vec<Candidate>, budget: usize) -> Vec<Candidate> {
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut used = 0usize;
    let mut selected = Vec::new();

    for c in candidates {
        if c.content.is_empty() {
            continue;
        }
        // Ensure files shorter than 4 chars still count as 1 token so they aren't silently excluded
        let t = c.tokens().max(1);
        if used + t > budget {
            // Try fitting a smaller remaining chunk
            if used >= budget {
                break;
            }
            let truncated = truncate_to_tokens(&c.content, budget - used);
            let tok = estimate_tokens(&truncated);
            if tok == 0 {
                break;
            }
            selected.push(Candidate {
                content: truncated,
                ..c
            });
            break;
        }
        used += t;
        selected.push(c);
    }

    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(score: f32, chars: usize) -> Candidate {
        Candidate {
            file_id: 0,
            path: PathBuf::from("x.rs"),
            score,
            content: "a".repeat(chars),
        }
    }

    #[test]
    fn selects_by_score_descending() {
        let cs = vec![make(0.5, 400), make(0.9, 400), make(0.1, 400)];
        let out = allocate(cs, 200);
        assert_eq!(out.len(), 2);
        assert!((out[0].score - 0.9).abs() < f32::EPSILON);
        assert!((out[1].score - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn truncates_last_file() {
        let cs = vec![make(1.0, 400), make(0.5, 800)];
        let out = allocate(cs, 150);
        assert_eq!(out.len(), 2);
        assert!(out[1].tokens() <= 50);
    }

    #[test]
    fn empty_when_budget_zero() {
        let cs = vec![make(1.0, 400)];
        let out = allocate(cs, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn ascii_estimate_matches_chars_over_four() {
        // Regression guard: ASCII content must keep the historic chars/4 behaviour.
        assert_eq!(estimate_tokens(&"a".repeat(400)), 100);
        // 1-3 ASCII chars are fewer than 4, but estimate_tokens now returns at least 1
        // for any non-empty content so that short files are never invisible to the budget.
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn short_ascii_never_zero_tokens() {
        // Regression for #60: 1-3 ASCII chars must not estimate as 0 tokens.
        for n in 1..=3 {
            let s = "a".repeat(n);
            assert_eq!(
                estimate_tokens(&s),
                1,
                "{n}-char ASCII string must estimate as 1 token, not 0"
            );
        }
    }

    #[test]
    fn cjk_estimate_exceeds_chars_over_four() {
        // 100 Japanese chars: old chars/4 said 25 tokens; the real count is ~100+.
        let jp = "あ".repeat(100);
        let est = estimate_tokens(&jp);
        assert_eq!(est, 100, "each multi-byte char counts as one token");
        assert!(
            est > jp.chars().count() / 4,
            "must exceed the old chars/4 estimate"
        );
    }

    #[test]
    fn cjk_allocation_never_overflows_budget() {
        // A large Japanese file against a small budget must be truncated so the
        // selected token total stays within budget (the pre-fix bug over-selected).
        let jp = Candidate {
            file_id: 1,
            path: PathBuf::from("notes.md"),
            score: 1.0,
            content: "日本語のコメント。".repeat(500),
        };
        let budget = 200;
        let out = allocate(vec![jp], budget);
        assert_eq!(out.len(), 1);
        let total: usize = out.iter().map(|c| c.tokens()).sum();
        assert!(
            total <= budget,
            "selected tokens {total} must not exceed budget {budget}"
        );
    }
}
