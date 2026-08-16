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
    /// Rough token count: chars / 4.
    pub fn tokens(&self) -> usize {
        self.content.chars().count() / 4
    }
}

/// Greedy budget allocation: sort by score descending, include until budget exhausted.
/// Returns selected candidates in score order.
pub fn allocate(mut candidates: Vec<Candidate>, budget: usize) -> Vec<Candidate> {
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    let mut used = 0usize;
    let mut selected = Vec::new();

    for c in candidates {
        let t = c.tokens();
        if t == 0 {
            continue;
        }
        if used + t > budget {
            // Try fitting a smaller remaining chunk
            if used >= budget {
                break;
            }
            let remaining_chars = (budget - used) * 4;
            let truncated: String = c.content.chars().take(remaining_chars).collect();
            let tok = truncated.chars().count() / 4;
            if tok == 0 {
                break;
            }
            selected.push(Candidate { content: truncated, ..c });
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
}
