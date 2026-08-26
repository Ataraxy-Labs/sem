//! Entity *headers* — the middle zoom between a bare name listing and a
//! full body: the signature lines up to where the body starts, preceded by
//! the first line of the doc comment sitting immediately above the entity.
//! Derived from the raw file text by line scanning, not the parse tree, so
//! it works uniformly for every language the registry knows and costs one
//! file read — no re-parse.
//!
//! Two styles cover the supported languages: brace languages end their
//! signature at the first `{` (Rust, TypeScript/JavaScript, Go, C-family,
//! Java, …), Python ends its `def`/`class` line at the trailing `:`.
//! Multi-line signatures are collected until the terminator; an entity with
//! no terminator inside its own bounds (e.g. a field or a one-line
//! declaration) contributes just its first line. The entity's own base
//! indentation is stripped so callers can re-indent for their own display.

use std::collections::HashMap;
use std::path::Path;

/// How a language marks the end of a signature / start of a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderStyle {
    /// Signature ends at the first `{` (kept, with anything after it cut).
    Brace,
    /// Signature ends at a line whose trimmed text ends with `:`.
    Python,
}

/// Pick the header style for a file by extension. Everything that isn't
/// Python is treated as brace-style — for extensions with no braces at all
/// the terminator simply never fires and the first-line fallback applies,
/// which is the right minimal header for those anyway.
pub fn header_style_for_path(file_path: &str) -> HeaderStyle {
    match file_path.rsplit('.').next() {
        Some("py") => HeaderStyle::Python,
        _ => HeaderStyle::Brace,
    }
}

/// Derive the header for one entity. `file_lines` is the whole file split
/// into lines (0-indexed); `start_line`/`end_line` are the entity's own
/// 1-indexed inclusive bounds. Returns the first doc-comment line above the
/// entity (if any) followed by the signature lines, all with the entity's
/// base indentation stripped and trailing whitespace trimmed. Out-of-range
/// bounds return an empty header rather than panicking.
pub fn entity_header(
    style: HeaderStyle,
    file_lines: &[&str],
    start_line: usize,
    end_line: usize,
) -> Vec<String> {
    if start_line == 0 || start_line > file_lines.len() {
        return Vec::new();
    }
    let first = &file_lines[start_line - 1];
    let base_indent = first.len() - first.trim_start().len();
    let dedent = |line: &str| -> String {
        let cut = line
            .char_indices()
            .take_while(|(i, c)| *i < base_indent && c.is_whitespace())
            .count();
        line[cut..].trim_end().to_string()
    };

    let mut out = Vec::new();
    let has_doc = if let Some(doc) = first_doc_line(style, file_lines, start_line) {
        out.push(doc);
        true
    } else {
        false
    };

    let last = end_line.clamp(start_line, file_lines.len());
    for line in &file_lines[start_line - 1..last] {
        match style {
            HeaderStyle::Brace => {
                if let Some(brace) = line.find('{') {
                    out.push(dedent(&line[..=brace]));
                    return out;
                }
                out.push(dedent(line));
            }
            HeaderStyle::Python => {
                out.push(dedent(line));
                if line.trim_end().ends_with(':') {
                    return out;
                }
            }
        }
    }

    // No terminator inside the entity's own bounds: a field, a one-line
    // declaration, or a language with no braces. The first line alone is
    // the honest minimal header.
    out.truncate(if has_doc { 2 } else { 1 });
    out
}

/// Headers for a batch of entities, keyed by entity id. `items` yields
/// `(id, file_path, start_line, end_line)`; each distinct file is read once
/// under `root`. An unreadable file (or an empty derived header) just leaves
/// its entities out of the map — the caller's listing never fails over the
/// zoom detail.
pub fn headers_by_id<'a>(
    root: &Path,
    items: impl Iterator<Item = (&'a str, &'a str, usize, usize)>,
) -> HashMap<String, Vec<String>> {
    let mut file_cache: HashMap<&'a str, Option<String>> = HashMap::new();
    let mut out = HashMap::new();
    for (id, file_path, start_line, end_line) in items {
        let content = file_cache
            .entry(file_path)
            .or_insert_with(|| std::fs::read_to_string(root.join(file_path)).ok());
        let Some(content) = content.as_deref() else {
            continue;
        };
        let file_lines: Vec<&str> = content.lines().collect();
        let header = entity_header(
            header_style_for_path(file_path),
            &file_lines,
            start_line,
            end_line,
        );
        if !header.is_empty() {
            out.insert(id.to_string(), header);
        }
    }
    out
}

/// The first line of the comment block sitting immediately above
/// `start_line`, skipping attribute/decorator lines (`#[...]`, `@...`)
/// between the block and the entity. Returned trimmed, marker and all —
/// e.g. `/// Frobnicates the baz.` or `# Talks to the flux capacitor.`
fn first_doc_line(style: HeaderStyle, file_lines: &[&str], start_line: usize) -> Option<String> {
    let mut i = start_line - 1; // 0-indexed line of the entity itself
    let mut first_comment: Option<&str> = None;
    while i > 0 {
        i -= 1;
        let line = file_lines[i].trim();
        let skippable = match style {
            HeaderStyle::Brace => line.starts_with("#[") || line.starts_with("#!["),
            HeaderStyle::Python => line.starts_with('@'),
        };
        if skippable && first_comment.is_none() {
            continue;
        }
        let is_comment = match style {
            HeaderStyle::Brace => {
                line.starts_with("///")
                    || line.starts_with("//!")
                    || line.starts_with("//")
                    || line.starts_with("/*")
                    || line.starts_with('*')
            }
            HeaderStyle::Python => line.starts_with('#'),
        };
        if is_comment {
            first_comment = Some(line);
        } else {
            break;
        }
    }
    first_comment.map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(src: &str) -> Vec<&str> {
        src.lines().collect()
    }

    #[test]
    fn python_files_get_python_style() {
        assert_eq!(header_style_for_path("pkg/util.py"), HeaderStyle::Python);
    }

    #[test]
    fn everything_else_gets_brace_style() {
        for path in ["a.rs", "b.ts", "c.go", "d.java", "e.c", "noext"] {
            assert_eq!(header_style_for_path(path), HeaderStyle::Brace, "{path}");
        }
    }

    #[test]
    fn brace_signature_stops_at_the_opening_brace() {
        let src = "pub fn alpha() -> usize {\n    41\n}\n";
        let header = entity_header(HeaderStyle::Brace, &lines(src), 1, 3);
        assert_eq!(header, vec!["pub fn alpha() -> usize {"]);
    }

    #[test]
    fn brace_single_line_entity_is_cut_at_the_brace() {
        let src = "fn tiny() -> usize { 1 }\n";
        let header = entity_header(HeaderStyle::Brace, &lines(src), 1, 1);
        assert_eq!(header, vec!["fn tiny() -> usize {"]);
    }

    #[test]
    fn brace_multi_line_signature_is_collected_up_to_the_brace() {
        let src = "pub fn wide(\n    a: usize,\n    b: usize,\n) -> usize {\n    a + b\n}\n";
        let header = entity_header(HeaderStyle::Brace, &lines(src), 1, 6);
        assert_eq!(
            header,
            vec![
                "pub fn wide(",
                "    a: usize,",
                "    b: usize,",
                ") -> usize {"
            ]
        );
    }

    #[test]
    fn python_signature_stops_at_the_colon() {
        let src = "def gamma():\n    return 1\n";
        let header = entity_header(HeaderStyle::Python, &lines(src), 1, 2);
        assert_eq!(header, vec!["def gamma():"]);
    }

    #[test]
    fn python_multi_line_signature_is_collected_up_to_the_colon() {
        let src = "def wide(\n    a,\n    b,\n):\n    return a + b\n";
        let header = entity_header(HeaderStyle::Python, &lines(src), 1, 5);
        assert_eq!(header, vec!["def wide(", "    a,", "    b,", "):"]);
    }

    #[test]
    fn first_doc_comment_line_is_included_above_the_signature() {
        let src = "/// Frobnicates the baz.\n/// More detail nobody needs here.\nfn frob() {\n}\n";
        let header = entity_header(HeaderStyle::Brace, &lines(src), 3, 4);
        assert_eq!(header, vec!["/// Frobnicates the baz.", "fn frob() {"]);
    }

    #[test]
    fn attributes_between_doc_and_entity_are_skipped() {
        let src = "/// Documented.\n#[inline]\nfn fast() {\n}\n";
        let header = entity_header(HeaderStyle::Brace, &lines(src), 3, 4);
        assert_eq!(header, vec!["/// Documented.", "fn fast() {"]);
    }

    #[test]
    fn python_comment_above_def_is_included() {
        let src = "# Talks to the flux capacitor.\ndef zap():\n    pass\n";
        let header = entity_header(HeaderStyle::Python, &lines(src), 2, 3);
        assert_eq!(header, vec!["# Talks to the flux capacitor.", "def zap():"]);
    }

    #[test]
    fn python_decorators_between_comment_and_def_are_skipped() {
        let src = "# Cached.\n@lru_cache\ndef memo():\n    pass\n";
        let header = entity_header(HeaderStyle::Python, &lines(src), 3, 4);
        assert_eq!(header, vec!["# Cached.", "def memo():"]);
    }

    #[test]
    fn no_doc_comment_yields_just_the_signature() {
        let src = "x = 1\n\ndef plain():\n    pass\n";
        let header = entity_header(HeaderStyle::Python, &lines(src), 3, 4);
        assert_eq!(header, vec!["def plain():"]);
    }

    #[test]
    fn no_terminator_falls_back_to_the_first_line() {
        let src = "count: usize,\nother: usize,\n";
        let header = entity_header(HeaderStyle::Brace, &lines(src), 1, 1);
        assert_eq!(header, vec!["count: usize,"]);
    }

    #[test]
    fn nested_entity_base_indentation_is_stripped() {
        let src = "class Box:\n    def get(self):\n        return self.v\n";
        let header = entity_header(HeaderStyle::Python, &lines(src), 2, 3);
        assert_eq!(header, vec!["def get(self):"]);
    }

    #[test]
    fn out_of_range_bounds_return_an_empty_header() {
        let src = "fn a() {}\n";
        assert!(entity_header(HeaderStyle::Brace, &lines(src), 0, 1).is_empty());
        assert!(entity_header(HeaderStyle::Brace, &lines(src), 9, 9).is_empty());
    }
}
