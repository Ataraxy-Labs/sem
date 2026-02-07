use sha2::{Digest, Sha256};
use tree_sitter::Node;

pub fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn short_hash(content: &str, length: usize) -> String {
    let hash = content_hash(content);
    hash[..length.min(hash.len())].to_string()
}

/// Compute a structural hash from a tree-sitter AST node.
/// Strips comments and normalizes whitespace so formatting-only changes
/// produce the same hash. This enables detecting "reformatted but not changed"
/// entities — inspired by Unison's content-addressed code model.
pub fn structural_hash(node: Node, source: &[u8]) -> String {
    let mut tokens = Vec::new();
    collect_structural_tokens(node, source, &mut tokens);
    let normalized = tokens.join(" ");
    content_hash(&normalized)
}

/// Recursively collect leaf tokens from the AST, skipping comments.
fn collect_structural_tokens(node: Node, source: &[u8], tokens: &mut Vec<String>) {
    let kind = node.kind();

    // Skip all comment types across languages
    if is_comment_node(kind) {
        return;
    }

    if node.child_count() == 0 {
        // Leaf node — collect its text
        let text = node.utf8_text(source).unwrap_or("").trim();
        if !text.is_empty() {
            tokens.push(text.to_string());
        }
    } else {
        // Internal node — recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_structural_tokens(child, source, tokens);
        }
    }
}

fn is_comment_node(kind: &str) -> bool {
    matches!(
        kind,
        "comment" | "line_comment" | "block_comment" | "doc_comment"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_hash_deterministic() {
        let h1 = content_hash("hello world");
        let h2 = content_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_content_hash_hex_format() {
        let h = content_hash("test");
        assert_eq!(h.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_short_hash() {
        let h = short_hash("test", 8);
        assert_eq!(h.len(), 8);
    }
}
