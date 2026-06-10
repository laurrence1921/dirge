//! Shared FTS5 query handling (dirge-fmuu).
//!
//! Call sites used to carry independent sanitizers with different
//! threat models — a syntax hazard fixed in one stayed open in the
//! others (the bare-apostrophe FTS5 error already happened once). All
//! FTS5-syntax knowledge now lives here. Pick by input shape:
//!
//! - [`sanitize_query`] — free-form USER queries where FTS5 operators
//!   and quoted phrases should keep working (session search). Most
//!   permissive; 6-step Hermes pipeline.
//! - [`quote_terms`] — arbitrary model/program text that should match
//!   as plain words, ANDed. Every token is quoted, so no input can be
//!   a syntax error (memory search).

use regex::Regex;
use std::sync::LazyLock;

/// Sanitize a free-form user query for FTS5 MATCH while preserving
/// intentional syntax (quoted phrases, prefix `*`).
/// Port of Hermes's `_sanitize_fts5_query` (hermes_state.py:2036-2086).
///
/// FTS5 MATCH treats many characters as syntax: `+`, `(`, `)`, `{`,
/// `}`, `^`, and bare boolean operators (AND, OR, NOT). Passing raw
/// user input directly to MATCH can be a query-syntax error.
///
/// Strategy (6-step pipeline from Hermes):
/// 1. Extract balanced double-quoted phrases, protect with placeholders
/// 2. Strip remaining FTS5-special chars: `+{}()"^`
/// 3. Collapse repeated `*` into single `*`, remove leading `*`
/// 4. Remove dangling boolean operators at start/end
/// 5. Wrap hyphenated and dotted terms in quotes (FTS5 splits on `-` and `.`)
/// 6. Restore preserved quoted phrases
pub(crate) fn sanitize_query(query: &str) -> String {
    if query.trim().is_empty() {
        return String::new();
    }

    // Step 1: Extract balanced double-quoted phrases and protect them.
    static QUOTED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""[^"]*""#).unwrap());
    let mut quoted_parts: Vec<String> = Vec::new();
    let mut sanitized = QUOTED_RE
        .replace_all(query, |caps: &regex::Captures| {
            let s = caps[0].to_string();
            let idx = quoted_parts.len();
            quoted_parts.push(s);
            format!("\x00Q{idx}\x00")
        })
        .to_string();

    // Step 2: Strip remaining FTS5-special characters: + { } ( ) " ^
    static SPECIAL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[+{}()"^]"#).unwrap());
    sanitized = SPECIAL_RE.replace_all(&sanitized, " ").to_string();

    // Step 3: Collapse repeated * into single *, remove leading *
    static STAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*+").unwrap());
    sanitized = STAR_RE.replace_all(&sanitized, "*").to_string();
    static LEADING_STAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|\s)\*").unwrap());
    sanitized = LEADING_STAR_RE.replace_all(&sanitized, "$1").to_string();

    // Step 4: Remove dangling boolean operators at start/end.
    // SESS-7: loop until stable so chained operators like
    // `AND OR foo` or `foo AND OR` are fully stripped. The single
    // `replace` (not `replace_all`) only consumed one match per
    // side and left FTS5-invalid residue that the engine then
    // rejected.
    static DANGLING_START_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(AND|OR|NOT)\b\s*").unwrap());
    static DANGLING_END_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\s+(AND|OR|NOT)\s*$").unwrap());
    loop {
        let before = sanitized.clone();
        sanitized = DANGLING_START_RE.replace(sanitized.trim(), "").to_string();
        sanitized = DANGLING_END_RE.replace(sanitized.trim(), "").to_string();
        if sanitized == before {
            break;
        }
    }

    // Step 5: Wrap hyphenated and dotted terms in quotes.
    // FTS5 tokenizer splits on `-` and `.`, so `chat-send` becomes
    // `chat AND send`. Quoting preserves phrase semantics.
    static DOT_DASH_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b(\w+(?:[._-]\w+)+\w*)\b").unwrap());
    sanitized = DOT_DASH_RE.replace_all(&sanitized, r#""$1""#).to_string();

    // Step 6: Restore preserved quoted phrases
    for (i, quoted) in quoted_parts.iter().enumerate() {
        sanitized = sanitized.replace(&format!("\x00Q{i}\x00"), quoted);
    }

    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed.to_string()
}

/// Quote each whitespace token so arbitrary phrasing can never be an
/// FTS5 syntax error (quotes inside tokens are stripped). Tokens are
/// implicitly ANDed by FTS5. No operator or phrase support — by
/// design: the input is model/program text, not query syntax.
pub(crate) fn quote_terms(query: &str) -> String {
    query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .filter(|t| t.len() > 2)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── sanitize_query (moved from session_search) ───────────────

    #[test]
    fn sanitize_passes_plain_words() {
        assert_eq!(sanitize_query("database migrations"), "database migrations");
    }

    #[test]
    fn sanitize_preserves_quoted_phrases() {
        assert_eq!(sanitize_query("\"exact phrase\""), "\"exact phrase\"");
    }

    #[test]
    fn sanitize_strips_special_chars() {
        assert_eq!(sanitize_query("+hello"), "hello");
    }

    #[test]
    fn sanitize_collapses_stars() {
        assert_eq!(sanitize_query("a***test"), "a*test");
    }

    #[test]
    fn sanitize_strips_dangling_operators() {
        assert_eq!(sanitize_query("hello AND"), "hello");
        assert_eq!(sanitize_query("AND OR foo"), "foo");
        assert_eq!(sanitize_query("foo AND OR"), "foo");
    }

    #[test]
    fn sanitize_quotes_dotted_and_hyphenated_terms() {
        assert_eq!(sanitize_query("my-app.config.ts"), "\"my-app.config.ts\"");
    }

    #[test]
    fn sanitize_empty_is_empty() {
        assert_eq!(sanitize_query("   "), "");
        assert_eq!(sanitize_query("AND"), "");
        // Nothing but special chars cleans to empty.
        assert_eq!(sanitize_query("*\"()"), "");
    }

    #[test]
    fn sanitize_trims_whitespace() {
        assert_eq!(sanitize_query("  hello world  "), "hello world");
    }

    // ── quote_terms (moved from memory_db) ───────────────────────

    #[test]
    fn quote_terms_neutralizes_syntax() {
        assert_eq!(quote_terms("cargo build"), "\"cargo\" \"build\"");
        assert_eq!(quote_terms("don't"), "\"don't\"");
        assert_eq!(quote_terms("a \"b\" c"), "\"a\" \"b\" \"c\"");
        assert_eq!(quote_terms("   "), "");
    }
}
