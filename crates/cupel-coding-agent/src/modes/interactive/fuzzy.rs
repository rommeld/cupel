//! Fuzzy matching for the file autocomplete.
//!
//! The shape is the classic editor-completion matcher: a query matches when
//! all of its characters appear IN ORDER in the candidate (not necessarily
//! adjacent), and a score decides ranking. LOWER is better, because the
//! score is mostly penalties:
//!
//! - consecutive-match streaks earn `-5 * streak` (typing "main" should
//!   love `main.rs`),
//! - gaps between matches cost `+2 * gap`,
//! - matching right after a word boundary (`space - _ . / :`) earns `-10`
//!   ("mr" should hit `main.rs` via m..r-after-dot),
//! - later positions cost `+0.1 * index` (prefer early matches),
//! - an exact full match earns a decisive `-100`.
//!
//! One quirk ported as-is: if the query fails, retry with its trailing
//! letter/digit halves swapped (`"v2" <-> "2v"`) at a `+5` penalty -
//! version-ish queries match either spelling.

/// Score one candidate against one query token. `None` = no match.
/// Lower scores rank first.
#[must_use]
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<f64> {
    let query_lower: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let candidate_lower: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();

    if let Some(score) = score_subsequence(&query_lower, &candidate_lower) {
        return Some(score);
    }

    // Letter/digit swap fallback: "v2" also tries "2v" (and vice versa).
    let swapped = swap_letter_digit_halves(&query_lower)?;
    score_subsequence(&swapped, &candidate_lower).map(|score| score + 5.0)
}

fn score_subsequence(query: &[char], candidate: &[char]) -> Option<f64> {
    if query.is_empty() {
        return Some(0.0);
    }
    if query.len() > candidate.len() {
        return None;
    }

    let mut query_index = 0;
    let mut score = 0.0;
    let mut last_match: Option<usize> = None;
    let mut consecutive = 0_u32;

    for (i, c) in candidate.iter().enumerate() {
        if query_index >= query.len() {
            break;
        }
        if *c != query[query_index] {
            continue;
        }

        let is_word_boundary = i == 0
            || matches!(
                candidate[i - 1],
                ' ' | '\t' | '\n' | '-' | '_' | '.' | '/' | ':'
            );

        match last_match {
            Some(last) if last + 1 == i => {
                consecutive += 1;
                score -= 5.0 * f64::from(consecutive);
            }
            Some(last) => {
                consecutive = 0;
                score += 2.0 * (i - last - 1) as f64;
            }
            None => consecutive = 0,
        }
        if is_word_boundary {
            score -= 10.0;
        }
        score += 0.1 * i as f64;

        last_match = Some(i);
        query_index += 1;
    }

    if query_index < query.len() {
        return None;
    }
    if query == candidate {
        score -= 100.0;
    }
    Some(score)
}

/// `"abc123"` -> `"123abc"`, `"123abc"` -> `"abc123"`; anything else `None`.
fn swap_letter_digit_halves(query: &[char]) -> Option<Vec<char>> {
    let split = query.iter().position(char::is_ascii_digit);
    match split {
        Some(0) => {
            // digits first, then letters
            let letters_start = query.iter().position(char::is_ascii_alphabetic)?;
            let (digits, letters) = query.split_at(letters_start);
            (digits.iter().all(char::is_ascii_digit)
                && letters.iter().all(char::is_ascii_alphabetic))
            .then(|| letters.iter().chain(digits.iter()).copied().collect())
        }
        Some(letters_end) => {
            let (letters, digits) = query.split_at(letters_end);
            (letters.iter().all(char::is_ascii_alphabetic)
                && digits.iter().all(char::is_ascii_digit))
            .then(|| digits.iter().chain(letters.iter()).copied().collect())
        }
        None => None,
    }
}

/// Filter + rank: the query splits on whitespace AND `/` into tokens
/// ("src main" or "src/main" both mean two tokens); ALL tokens must match;
/// per-token scores sum; ascending stable sort (ties keep input order).
#[must_use]
pub fn fuzzy_filter<'a, T>(query: &str, items: &'a [T], key: impl Fn(&T) -> &str) -> Vec<&'a T> {
    let tokens: Vec<&str> = query
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return items.iter().collect();
    }

    let mut scored: Vec<(&T, f64)> = items
        .iter()
        .filter_map(|item| {
            let text = key(item);
            let mut total = 0.0;
            for token in &tokens {
                total += fuzzy_score(token, text)?;
            }
            Some((item, total))
        })
        .collect();
    // Stable sort keeps input order on ties; scores are finite by
    // construction, so the comparison never actually falls back.
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(core::cmp::Ordering::Equal));
    scored.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_in_order_matches_out_of_order_does_not() {
        assert!(fuzzy_score("fbr", "foo/bar").is_some());
        assert!(fuzzy_score("rbf", "foo/bar").is_none());
        assert!(fuzzy_score("longerthan", "short").is_none());
    }

    #[test]
    fn empty_query_matches_everything_at_zero() {
        assert_eq!(fuzzy_score("", "anything"), Some(0.0));
    }

    #[test]
    fn exact_match_outranks_everything() {
        let exact = fuzzy_score("main.rs", "main.rs").expect("matches");
        let prefix = fuzzy_score("main.rs", "main.rs.bak").expect("matches");
        assert!(exact < prefix);
    }

    #[test]
    fn consecutive_runs_beat_scattered_chars() {
        let consecutive = fuzzy_score("main", "src/main.rs").expect("matches");
        let scattered = fuzzy_score("main", "map_input.rs").expect("matches");
        assert!(consecutive < scattered, "{consecutive} vs {scattered}");
    }

    #[test]
    fn word_boundary_matches_score_better() {
        // "r" right after '.' (boundary) vs buried mid-word.
        let boundary = fuzzy_score("r", ".rs").expect("matches");
        let mid_word = fuzzy_score("r", "xxr").expect("matches");
        assert!(boundary < mid_word);
    }

    #[test]
    fn earlier_matches_win_position_ties() {
        let early = fuzzy_score("a", "abc").expect("matches");
        let late = fuzzy_score("a", "xxa").expect("matches");
        assert!(early < late);
    }

    #[test]
    fn letter_digit_swap_fallback_matches_at_a_penalty() {
        // "2v" is not a subsequence of "v2-schema", but the swap "v2" is.
        let swapped = fuzzy_score("2v", "v2-schema").expect("swap fallback matches");
        let direct = fuzzy_score("v2", "v2-schema").expect("matches");
        assert!(direct < swapped, "swap carries a +5 penalty");
    }

    #[test]
    fn filter_requires_all_tokens_and_sorts_ascending() {
        let items = vec![
            "src/main.rs".to_string(),
            "docs/main.md".to_string(),
            "src/lib.rs".to_string(),
        ];
        // Both "src main" and "src/main" mean the same two tokens.
        for query in ["src main", "src/main"] {
            let matched = fuzzy_filter(query, &items, |s| s.as_str());
            assert_eq!(matched, vec![&items[0]], "query: {query}");
        }
        // Empty query preserves input order.
        let all = fuzzy_filter("", &items, |s| s.as_str());
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], &items[0]);
    }
}
