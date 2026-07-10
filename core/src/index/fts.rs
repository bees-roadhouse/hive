// FTS5 query construction + ranking helpers (PR 1.5). `fts5_query` is the
// successor to store/semantic.rs `to_match_query` (the Postgres tsquery
// builder): user text in, a query string safe to hand to `MATCH` out.
//
// The rules, pinned by the adversarial tests below:
//
//   1. Split the input on Unicode whitespace.
//   2. Per token, keep only alphanumeric characters (Unicode-aware) and
//      lowercase them. This deletes every FTS5 operator and special
//      character (`"`, `-`, `.`, `:`, `(`, `)`, `*`, `^`, `+`) and, with
//      the lowercasing, also neutralizes the word-form operators — FTS5
//      only treats bare UPPERCASE `AND`/`OR`/`NOT`/`NEAR` as operators.
//   3. Drop tokens with nothing left (pure punctuation).
//   4. Double-quote every surviving token (a quoted string is always a
//      plain phrase term to FTS5) and append `*` to EVERY token — prefix
//      matching per term, matching `to_match_query`'s `:*`-per-term
//      semantics (documented choice: every token, not just the last; this
//      builder serves stored-corpus search, not an as-you-type box where
//      only the tail is mid-word).
//   5. Join with a single space — implicit AND in FTS5, matching the
//      ` & ` join on the Postgres side.
//
// Empty output means "no usable terms"; callers return zero hits rather
// than passing an empty MATCH through (which would be a syntax error).

/// Build a safe FTS5 MATCH query from raw user input. See module header for
/// the exact rules. Returns an empty string when no token survives.
pub fn fts5_query(user_input: &str) -> String {
    user_input
        .split_whitespace()
        .filter_map(|token| {
            let stem: String = token
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(|c| c.to_lowercase())
                .collect();
            if stem.is_empty() {
                None
            } else {
                Some(format!("\"{stem}\"*"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize a raw `bm25()` rank into a descending 0..1 relevance score.
///
/// FTS5's `bm25()` is ascending-is-better and negative for matches (it
/// returns the negated BM25 weight so that plain `ORDER BY bm25(t)` ranks
/// best-first). This helper flips it into the shape the Postgres path's
/// clamped `ts_rank` gave callers: higher is better, bounded 0..1, rounded
/// to 3 decimals. The mapping `x / (1 + x)` over the positive weight is
/// monotone, so ordering by this score DESC equals ordering by bm25 ASC.
pub fn bm25_score(raw_bm25: f64) -> f64 {
    let positive = (-raw_bm25).max(0.0);
    let normalized = positive / (1.0 + positive);
    (normalized * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the pinned adversarial matrix ───────────────────────────────────────

    #[test]
    fn plain_words_quote_and_star() {
        assert_eq!(fts5_query("hello world"), "\"hello\"* \"world\"*");
    }

    #[test]
    fn embedded_quotes_are_stripped() {
        assert_eq!(fts5_query(r#""hello" wor"ld"#), "\"hello\"* \"world\"*");
        assert_eq!(fts5_query(r#"""" """#), "");
    }

    #[test]
    fn minus_cannot_negate() {
        assert_eq!(fts5_query("-secret"), "\"secret\"*");
        assert_eq!(fts5_query("a -b"), "\"a\"* \"b\"*");
    }

    #[test]
    fn dots_and_colons_are_stripped() {
        assert_eq!(fts5_query("store.ts"), "\"storets\"*");
        assert_eq!(fts5_query("kind:mail"), "\"kindmail\"*");
    }

    #[test]
    fn word_operators_are_neutralized_by_lowercasing() {
        assert_eq!(
            fts5_query("cats AND dogs OR birds NOT fish NEAR bees"),
            "\"cats\"* \"and\"* \"dogs\"* \"or\"* \"birds\"* \"not\"* \"fish\"* \"near\"* \"bees\"*"
        );
    }

    #[test]
    fn parens_and_carets_cannot_group_or_anchor() {
        assert_eq!(fts5_query("(a OR b)"), "\"a\"* \"or\"* \"b\"*");
        assert_eq!(fts5_query("^first"), "\"first\"*");
    }

    #[test]
    fn user_stars_are_stripped_ours_are_appended() {
        assert_eq!(fts5_query("pre*"), "\"pre\"*");
        assert_eq!(fts5_query("*"), "");
    }

    #[test]
    fn unicode_words_survive() {
        assert_eq!(fts5_query("Café Zibaldone"), "\"café\"* \"zibaldone\"*");
        assert_eq!(fts5_query("蜂蜜 レシピ"), "\"蜂蜜\"* \"レシピ\"*");
    }

    #[test]
    fn empty_and_punctuation_only_inputs_yield_empty() {
        assert_eq!(fts5_query(""), "");
        assert_eq!(fts5_query("   "), "");
        assert_eq!(fts5_query("!!! ... ---"), "");
    }

    #[test]
    fn single_char_stems_survive() {
        // Parity with to_match_query, which keeps 1-char stems (`a:*`).
        assert_eq!(fts5_query("a bee"), "\"a\"* \"bee\"*");
    }

    #[test]
    fn digits_and_mixed_tokens() {
        assert_eq!(fts5_query("c++ 2026 v0.6.0"), "\"c\"* \"2026\"* \"v060\"*");
    }

    // ── bm25 normalization ──────────────────────────────────────────────────

    #[test]
    fn bm25_score_flips_and_bounds() {
        // More negative bm25 (better match) → higher score.
        assert!(bm25_score(-5.0) > bm25_score(-1.0));
        assert_eq!(bm25_score(0.0), 0.0);
        // Wrong-signed input clamps instead of going negative.
        assert_eq!(bm25_score(3.0), 0.0);
        let s = bm25_score(-1000.0);
        assert!(s > 0.99 && s <= 1.0);
        // 3-decimal rounding like the Postgres path.
        assert_eq!(bm25_score(-1.0), 0.5);
    }
}
