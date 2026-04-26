pub mod blame;
pub mod context;
pub mod diff;
pub mod entities;
pub mod graph;
pub mod impact;
pub mod log;
pub mod setup;
pub mod verify;

/// Truncate a string to `max_chars` Unicode scalar values (codepoints), appending "..." if
/// truncated. Safe for multibyte encodings (CJK, simple emoji). Note: does not split on grapheme
/// cluster boundaries — ZWJ emoji sequences may render incorrectly at the truncation point.
///
/// If `max_chars <= 3`, no ellipsis is appended (no room); the string is simply truncated.
pub fn truncate_str(s: &str, max_chars: usize) -> String {
    if max_chars <= 3 {
        return s.chars().take(max_chars).collect();
    }
    // Use char_indices to find the byte boundary in a single pass
    let mut last_boundary = 0;
    let mut truncate_boundary = 0;
    let mut count = 0;
    for (i, c) in s.char_indices() {
        count += 1;
        if count == max_chars - 3 {
            truncate_boundary = i + c.len_utf8();
        }
        if count == max_chars {
            last_boundary = i + c.len_utf8();
            break;
        }
    }
    if count < max_chars {
        // String fits within max_chars — return as-is
        s.to_string()
    } else if s[last_boundary..].is_empty() {
        // Exactly max_chars — return as-is
        s.to_string()
    } else {
        // String exceeds max_chars — truncate with ellipsis
        format!("{}...", &s[..truncate_boundary])
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_str;

    #[test]
    fn ascii_short_string_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn ascii_exact_length_unchanged() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn ascii_truncated_with_ellipsis() {
        // 6 chars > max 5, so take 2 chars + "..."
        assert_eq!(truncate_str("abcdef", 5), "ab...");
    }

    #[test]
    fn cjk_short_string_unchanged() {
        assert_eq!(truncate_str("日本語", 10), "日本語");
    }

    #[test]
    fn cjk_truncated_at_char_boundary() {
        // This was the original bug — byte-index slicing panicked on CJK chars.
        // "bff側でwebsocketエラーが頻発している問題を修正" is 28 chars
        let msg = "bff側でwebsocketエラーが頻発している問題を修正";
        let result = truncate_str(msg, 15);
        // 15 - 3 = 12 chars kept + "..."
        assert_eq!(result.chars().count(), 15);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn emoji_truncated_at_char_boundary() {
        let msg = "🎉🚀✨ feat: add new feature with celebration";
        let result = truncate_str(msg, 10);
        // 10 - 3 = 7 chars kept + "..."
        assert_eq!(result.chars().count(), 10);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn mixed_cjk_ascii_truncation() {
        // Reproduces the exact scenario that caused the original panic:
        // byte-index slicing at 37 landed inside '頻' (bytes 36..39)
        let msg = ":bug: bff側でwebsocketエラーが頻発している問題を修正";
        // 35 chars, truncate at 20 to force truncation
        let result = truncate_str(msg, 20);
        assert_eq!(result.chars().count(), 20);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn empty_string() {
        assert_eq!(truncate_str("", 10), "");
    }

    #[test]
    fn max_chars_zero() {
        assert_eq!(truncate_str("hello", 0), "");
    }

    #[test]
    fn max_chars_one() {
        assert_eq!(truncate_str("hello", 1), "h");
    }

    #[test]
    fn max_chars_three_with_longer_string() {
        // Boundary: max_chars == 3, string is longer → no room for "...", just take 3 chars
        assert_eq!(truncate_str("hello", 3), "hel");
    }

    #[test]
    fn max_chars_four_triggers_ellipsis() {
        // max_chars == 4, string is longer → take 1 char + "..."
        assert_eq!(truncate_str("hello", 4), "h...");
    }
}
