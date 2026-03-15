use std::hash::Hasher;
use tree_sitter::Node;
use xxhash_rust::xxh3::Xxh3;

pub fn content_hash(content: &str) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(content.as_bytes()))
}

pub fn short_hash(content: &str, length: usize) -> String {
    let hash = content_hash(content);
    hash[..length.min(hash.len())].to_string()
}

/// Compute a structural hash from a tree-sitter AST node.
/// Strips comments and normalizes whitespace so formatting-only changes
/// produce the same hash. Uses streaming xxHash64 to avoid intermediate
/// string allocations.
pub fn structural_hash(node: Node, source: &[u8]) -> String {
    let mut hasher = Xxh3::new();
    hash_structural_tokens(node, source, &mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Compute a structural hash that excludes tokens within a given byte range.
/// Used to strip the entity name from the hash so that renames of otherwise
/// identical entities produce the same hash, enabling Phase 2 rename detection.
pub fn structural_hash_excluding_range(
    node: Node,
    source: &[u8],
    exclude_start: usize,
    exclude_end: usize,
) -> String {
    let mut hasher = Xxh3::new();
    hash_structural_tokens_excluding(node, source, &mut hasher, exclude_start, exclude_end);
    format!("{:016x}", hasher.finish())
}

/// Recursively hash tokens from the AST, skipping comments.
/// Hashes both node types (structure) and leaf text (content) so that
/// structurally different ASTs with identical leaf tokens produce different hashes.
/// Zero allocations: hashes directly from source byte slices.
fn hash_structural_tokens(node: Node, source: &[u8], hasher: &mut Xxh3) {
    let kind = node.kind();

    if is_comment_node(kind) {
        return;
    }

    if node.child_count() == 0 {
        // Leaf node: hash its text directly from the source buffer
        let start = node.start_byte();
        let end = node.end_byte();
        if start < end && end <= source.len() {
            let bytes = &source[start..end];
            // Trim whitespace manually to avoid allocation
            let trimmed = trim_bytes(bytes);
            if !trimmed.is_empty() {
                hasher.write(trimmed);
                hasher.write(b" ");
            }
        }
    } else {
        // Hash the node type to capture structure, not just leaf content.
        // e.g. `x = foo(bar)` vs `foo(bar) = x` have same leaves but different structure.
        hasher.write(kind.as_bytes());
        hasher.write(b":");
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            hash_structural_tokens(child, source, hasher);
        }
    }
}

/// Like `hash_structural_tokens` but skips any leaf node whose byte range
/// overlaps the excluded range (the entity name).
fn hash_structural_tokens_excluding(
    node: Node,
    source: &[u8],
    hasher: &mut Xxh3,
    exclude_start: usize,
    exclude_end: usize,
) {
    let kind = node.kind();

    if is_comment_node(kind) {
        return;
    }

    if node.child_count() == 0 {
        let start = node.start_byte();
        let end = node.end_byte();
        // Skip leaf nodes that overlap the excluded range
        if start < exclude_end && end > exclude_start {
            return;
        }
        if start < end && end <= source.len() {
            let bytes = &source[start..end];
            let trimmed = trim_bytes(bytes);
            if !trimmed.is_empty() {
                hasher.write(trimmed);
                hasher.write(b" ");
            }
        }
    } else {
        hasher.write(kind.as_bytes());
        hasher.write(b":");
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            hash_structural_tokens_excluding(child, source, hasher, exclude_start, exclude_end);
        }
    }
}

/// Trim leading/trailing ASCII whitespace from a byte slice without allocating.
#[inline]
fn trim_bytes(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(start, |p| p + 1);
    &bytes[start..end]
}

fn is_comment_node(kind: &str) -> bool {
    matches!(
        kind,
        "comment" | "line_comment" | "block_comment" | "doc_comment" | "tag_comment"
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
        assert_eq!(h.len(), 16); // xxHash64 = 8 bytes = 16 hex chars
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_short_hash() {
        let h = short_hash("test", 8);
        assert_eq!(h.len(), 8);
    }
}
