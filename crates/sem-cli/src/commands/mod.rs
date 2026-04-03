pub mod blame;
pub mod context;
pub mod diff;
pub mod entities;
pub mod graph;
pub mod impact;
pub mod log;
pub mod setup;

/// Truncate a string to `max_chars` characters (not bytes), appending "..." if truncated.
/// This is safe for multibyte characters (e.g. CJK, emoji).
pub fn truncate_str(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count > max_chars {
        let truncated: String = s.chars().take(max_chars - 3).collect();
        format!("{truncated}...")
    } else {
        s.to_string()
    }
}
