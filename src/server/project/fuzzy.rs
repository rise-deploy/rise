/// Fuzzy string matching using Jaro-Winkler distance algorithm
///
/// This module provides fuzzy matching capabilities for project names, allowing
/// helpful suggestions when users make typos.
/// Calculate Jaro similarity between two strings
///
/// Returns a score between 0.0 (no similarity) and 1.0 (identical)
fn jaro_similarity(s1: &str, s2: &str) -> f64 {
    if s1 == s2 {
        return 1.0;
    }
    if s1.is_empty() || s2.is_empty() {
        return 0.0;
    }

    let s1_len = s1.len();
    let s2_len = s2.len();

    // Maximum allowed distance for matching characters
    let match_distance = (s1_len.max(s2_len) / 2).saturating_sub(1);

    let s1_chars: Vec<char> = s1.chars().collect();
    let s2_chars: Vec<char> = s2.chars().collect();

    let mut s1_matches = vec![false; s1_len];
    let mut s2_matches = vec![false; s2_len];

    let mut matches = 0;

    // Find matches
    for i in 0..s1_len {
        let start = i.saturating_sub(match_distance);
        let end = (i + match_distance + 1).min(s2_len);

        for j in start..end {
            if s2_matches[j] || s1_chars[i] != s2_chars[j] {
                continue;
            }
            s1_matches[i] = true;
            s2_matches[j] = true;
            matches += 1;
            break;
        }
    }

    if matches == 0 {
        return 0.0;
    }

    // Count transpositions
    let mut transpositions = 0;
    let mut k = 0;

    for i in 0..s1_len {
        if !s1_matches[i] {
            continue;
        }
        while !s2_matches[k] {
            k += 1;
        }
        if s1_chars[i] != s2_chars[k] {
            transpositions += 1;
        }
        k += 1;
    }

    let matches_f64 = matches as f64;

    (matches_f64 / s1_len as f64
        + matches_f64 / s2_len as f64
        + (matches_f64 - transpositions as f64 / 2.0) / matches_f64)
        / 3.0
}

/// Calculate Jaro-Winkler distance between two strings
///
/// This is an extension of Jaro similarity that gives more weight to strings
/// with common prefixes, which is useful for typos where the beginning is often correct.
///
/// Returns a score between 0.0 (no similarity) and 1.0 (identical)
pub fn jaro_winkler_distance(s1: &str, s2: &str) -> f64 {
    let jaro = jaro_similarity(s1, s2);

    // Find common prefix length (up to 4 characters)
    let prefix_len = s1
        .chars()
        .zip(s2.chars())
        .take(4)
        .take_while(|(c1, c2)| c1 == c2)
        .count();

    // Jaro-Winkler uses a prefix scale of 0.1
    let prefix_scale = 0.1;

    jaro + (prefix_len as f64 * prefix_scale * (1.0 - jaro))
}

/// Find project names similar to the input string
///
/// Returns a list of project names with similarity above the threshold,
/// sorted by similarity score (most similar first).
///
/// # Arguments
/// * `input` - The input string to match against
/// * `names` - The project names to search through
/// * `threshold` - Minimum similarity score (0.0-1.0). Recommended: 0.85
///
/// # Returns
/// Vector of project names sorted by similarity (descending)
pub fn find_similar_projects(input: &str, names: &[&str], threshold: f64) -> Vec<String> {
    let input_lower = input.to_lowercase();

    let mut matches: Vec<(f64, String)> = names
        .iter()
        .map(|name| {
            let score = jaro_winkler_distance(&input_lower, &name.to_lowercase());
            (score, name.to_string())
        })
        .filter(|(score, _)| *score >= threshold)
        .collect();

    // Sort by score descending
    matches.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Return just the names
    matches.into_iter().map(|(_, name)| name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jaro_winkler_identical() {
        assert_eq!(jaro_winkler_distance("my-app", "my-app"), 1.0);
    }

    #[test]
    fn test_jaro_winkler_completely_different() {
        let score = jaro_winkler_distance("abc", "xyz");
        assert!(score < 0.5);
    }

    #[test]
    fn test_jaro_winkler_similar() {
        // "my-ap" vs "my-app" should be very similar
        let score = jaro_winkler_distance("my-ap", "my-app");
        assert!(score > 0.9, "Expected score > 0.9, got {}", score);
    }

    #[test]
    fn test_jaro_winkler_prefix_bonus() {
        // Strings with common prefix get higher scores than those without
        // Using same edit distance (1 char different) but different positions
        let score1 = jaro_winkler_distance("abcdef", "abcdex"); // Different at end
        let score2 = jaro_winkler_distance("abcdef", "xbcdef"); // Different at start

        assert!(
            score1 > score2,
            "Common prefix should score higher: score1={}, score2={}",
            score1,
            score2
        );
    }

    #[test]
    fn test_find_similar_projects_empty() {
        let similar = find_similar_projects("my-ap", &[], 0.85);
        assert_eq!(similar.len(), 0);
    }
}
