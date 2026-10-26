//! Heuristic level classification and free-text search over raw log lines.
//!
//! Backends whose runtime returns raw bytes with no level metadata (the
//! Kubernetes API, the Docker daemon, CloudWatch) classify each line here.
//! Loki is the exception: it carries its own `detected_level`.

use chrono::Duration;

/// Cap an upstream response body before including it in an error message. Loki
/// can return very large HTML/JSON error pages and we don't want those flooding
/// the structured log line that surfaces the failure.
pub fn truncate_for_error(s: String) -> String {
    const MAX: usize = 1024;
    if s.len() <= MAX {
        return s;
    }
    // Truncate at a UTF-8 boundary at or before MAX so the resulting String is
    // still valid. `floor_char_boundary` is unstable, so do it by hand.
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut t = s;
    t.truncate(end);
    t.push_str("... (truncated)");
    t
}

// Patterns for per-line classification. Loki classifies via its built-in
// `detected_level` metadata (passed through verbatim) and does not use these.
const LEVEL_REGEX_ERROR: &str = r"(?i)\b(error|err|fatal|panic|exception|failed)\b";
const LEVEL_REGEX_WARN: &str = r"(?i)\b(warn|warning)\b";

/// Classify a raw log line into one of the three [`HEURISTIC_LEVELS`].
///
/// For runtimes that return raw bytes with no level metadata of their own, each
/// line is scanned for error/warn keywords with an info catch-all.
pub fn classify_log_line(line: &str) -> &'static str {
    use std::sync::OnceLock;
    static ERROR_RE: OnceLock<regex::Regex> = OnceLock::new();
    static WARN_RE: OnceLock<regex::Regex> = OnceLock::new();
    let err = ERROR_RE.get_or_init(|| regex::Regex::new(LEVEL_REGEX_ERROR).unwrap());
    let warn = WARN_RE.get_or_init(|| regex::Regex::new(LEVEL_REGEX_WARN).unwrap());
    if err.is_match(line) {
        "error"
    } else if warn.is_match(line) {
        "warn"
    } else {
        "info"
    }
}

/// Trim a free-text search term, treating blank input as "no filter".
pub fn normalize_search(value: Option<&str>) -> Option<String> {
    value
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Case-insensitive substring match of a raw line against a search term.
/// A blank or absent term matches everything.
pub fn line_matches_search(line: &str, search: Option<&str>) -> bool {
    let Some(text) = normalize_search(search) else {
        return true;
    };
    let lower_line = line.to_lowercase();
    let lower_text = text.to_lowercase();
    lower_line.contains(&lower_text)
}

/// Parse a short retention hint like `"7d"` or `"2w"`. Supported units:
/// `s` (seconds), `m` (minutes), `h` (hours), `d` (days), `w` (weeks; 7 days).
pub fn parse_duration_hint(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    if trimmed.len() < 2 {
        return None;
    }
    let (number, unit) = trimmed.split_at(trimmed.len() - 1);
    let number: i64 = number.parse().ok()?;
    match unit {
        "s" => Some(Duration::seconds(number)),
        "m" => Some(Duration::minutes(number)),
        "h" => Some(Duration::hours(number)),
        "d" => Some(Duration::days(number)),
        "w" => Some(Duration::weeks(number)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_for_error_keeps_short_strings_intact() {
        let short = "boom".to_string();
        assert_eq!(truncate_for_error(short.clone()), short);
    }

    #[test]
    fn truncate_for_error_truncates_long_strings_at_utf8_boundary() {
        let long: String = "a".repeat(2048);
        let out = truncate_for_error(long);
        assert!(out.ends_with("... (truncated)"));
        // Total length: 1024 + len("... (truncated)").
        assert_eq!(out.len(), 1024 + "... (truncated)".len());
    }

    #[test]
    fn parse_duration_hint_supports_all_units_including_weeks() {
        // Cover every supported unit; `w` is the new one and must equal 7d.
        assert_eq!(parse_duration_hint("30s"), Some(Duration::seconds(30)));
        assert_eq!(parse_duration_hint("5m"), Some(Duration::minutes(5)));
        assert_eq!(parse_duration_hint("2h"), Some(Duration::hours(2)));
        assert_eq!(parse_duration_hint("7d"), Some(Duration::days(7)));
        assert_eq!(parse_duration_hint("2w"), Some(Duration::weeks(2)));
        assert_eq!(parse_duration_hint("2w"), Some(Duration::days(14)));
        // Negative weeks pass through (chrono::Duration accepts negatives) — a
        // retention hint of "-1w" is nonsensical but the parser shouldn't
        // crash on it. We only assert the unit math here.
        assert_eq!(parse_duration_hint("0w"), Some(Duration::zero()));

        // Unsupported unit / malformed input.
        assert_eq!(parse_duration_hint("5y"), None);
        assert_eq!(parse_duration_hint("xw"), None);
        // Whitespace is trimmed.
        assert_eq!(parse_duration_hint("  3w  "), Some(Duration::weeks(3)));
    }

    #[test]
    fn classify_log_line_returns_one_of_three_levels() {
        // The K8s backend has no upstream classifier; its regex catch-all
        // promises every line lands in exactly one of `HEURISTIC_LEVELS`.
        assert_eq!(classify_log_line("plain hello world"), "info");
        assert_eq!(classify_log_line("WARN connection retry"), "warn");
        assert_eq!(classify_log_line("ERROR: failed to connect"), "error");
        // Error wins over warn when both match.
        assert_eq!(classify_log_line("WARN: fatal error in handler"), "error");
    }
}
