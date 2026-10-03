//! Code-shape checks: a tree-sitter query run over files. Every capture is a
//! hit (file, line, column, captured text); a promise holds when there are none.
//! Any language sem has a grammar for works; the grammar comes from the path.

use std::path::Path;

use rayon::prelude::*;
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::parser::plugins::code::languages::get_language_config;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Hit {
    pub line: usize,
    pub col: usize,
    pub capture: String,
    pub text: String,
}

/// Grammar for a path, from sem's language registry (by extension).
pub fn language_for(path: &str) -> Option<Language> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    (get_language_config(&format!(".{ext}"))?.get_language)()
}

fn parse(lang: &Language, source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(lang).ok()?;
    parser.parse(source, None)
}

/// A query compiled once per grammar.
pub struct Pattern {
    src: String,
    compiled: Vec<(Language, Query)>,
    /// Keep a hit only if it lies inside a node of one of these kinds (any depth).
    within: Vec<String>,
}

impl Pattern {
    pub fn new(src: &str) -> Pattern {
        Pattern { src: src.to_string(), compiled: Vec::new(), within: Vec::new() }
    }

    pub fn within(mut self, kinds: Vec<String>) -> Pattern {
        self.within = kinds;
        self
    }

    fn prepare(&mut self, lang: &Language) -> Result<(), String> {
        if !self.compiled.iter().any(|(l, _)| l == lang) {
            let q = Query::new(lang, &self.src).map_err(|e| format!("invalid query: {e}"))?;
            self.compiled.push((lang.clone(), q));
        }
        Ok(())
    }

    /// Captures in an already-parsed tree (`prepare`d for its grammar). When
    /// captures nest, only the outermost is a hit (`y - a / 2` is one).
    fn hits(&self, lang: &Language, tree: &Tree, source: &str) -> Vec<Hit> {
        let Some((_, query)) = self.compiled.iter().find(|(l, _)| l == lang) else { return Vec::new() };
        let inside = |mut n: tree_sitter::Node| {
            while let Some(parent) = n.parent() {
                if self.within.iter().any(|k| k == parent.kind()) {
                    return true;
                }
                n = parent;
            }
            false
        };
        let names = query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut found = Vec::new();
        let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
        while let Some(m) = matches.next() {
            for c in m.captures {
                let name = names[c.index as usize];
                if name.starts_with('_') || (!self.within.is_empty() && !inside(c.node)) {
                    continue; // helper captures (used by predicates) are not hits
                }
                found.push((c.node.start_byte(), c.node.end_byte(), name, c.node));
            }
        }
        found.sort_by_key(|&(s, e, _, _)| (s, std::cmp::Reverse(e)));
        let mut end = 0;
        let mut hits = Vec::new();
        for (s, e, name, node) in found {
            if !hits.is_empty() && e <= end && s < end {
                continue; // nested in (or equal to) the previous hit
            }
            end = e;
            let p = node.start_position();
            let text = node.utf8_text(source.as_bytes()).unwrap_or("");
            let first = text.lines().next().unwrap_or("").trim();
            hits.push(Hit { line: p.row + 1, col: p.column + 1, capture: name.to_string(), text: first.chars().take(120).collect() });
        }
        hits
    }

    /// All (outermost) captures of the query in `source`.
    pub fn find(&mut self, path: &str, source: &str) -> Result<Vec<Hit>, String> {
        let Some(lang) = language_for(path) else { return Ok(Vec::new()) };
        self.prepare(&lang)?;
        Ok(parse(&lang, source).map(|t| self.hits(&lang, &t, source)).unwrap_or_default())
    }
}

/// Run patterns over files under `root`, parsing each file once (files in
/// parallel). `files[i] = (repo-relative path, indices of the patterns that
/// apply to it)`; returns, per pattern, its `(path, hit)`s in file order.
pub fn scan(root: &Path, files: &[(String, Vec<usize>)], patterns: &mut [Pattern]) -> Result<Vec<Vec<(String, Hit)>>, String> {
    for (path, which) in files {
        if let Some(lang) = language_for(path) {
            for &i in which {
                patterns[i].prepare(&lang)?;
            }
        }
    }
    let patterns = &*patterns;
    let per_file: Vec<Vec<(usize, Hit)>> = files
        .par_iter()
        .map(|(path, which)| {
            let Some(lang) = language_for(path) else { return Vec::new() };
            let Ok(src) = std::fs::read_to_string(root.join(path)) else { return Vec::new() };
            let Some(tree) = parse(&lang, &src) else { return Vec::new() };
            which.iter().flat_map(|&i| patterns[i].hits(&lang, &tree, &src).into_iter().map(move |h| (i, h))).collect()
        })
        .collect();
    let mut out: Vec<Vec<(String, Hit)>> = (0..patterns.len()).map(|_| Vec::new()).collect();
    for ((path, _), hits) in files.iter().zip(per_file) {
        for (i, h) in hits {
            out[i].push((path.clone(), h));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{scan, Pattern};

    #[test]
    fn finds_ternaries_inside_jsx_only() {
        let src = r#"const a = x ? 1 : 2;
export const V = () => <div>{open ? <A/> : null}{name}</div>;
"#;
        let mut p = Pattern::new("(jsx_expression (ternary_expression) @logic)");
        let hits = p.find("v.tsx", src).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].line, hits[0].capture.as_str()), (2, "logic"));
    }

    #[test]
    fn within_matches_at_any_depth() {
        let src = "const t = a ? b : c;\nconst v = <div>{f(open ? 1 : 2)}</div>;";
        let mut p = Pattern::new("(ternary_expression) @logic").within(vec!["jsx_expression".into()]);
        let hits = p.find("v.tsx", src).unwrap();
        assert_eq!(hits.iter().map(|h| h.line).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn helper_captures_are_not_hits() {
        let src = "const v = <div>{items.map(i => <b/>)}</div>;";
        let mut p = Pattern::new(
            r#"(jsx_expression (call_expression function: (member_expression property: (property_identifier) @_m)) @logic (#match? @_m "^(map|filter|reduce)$"))"#,
        );
        let hits = p.find("v.tsx", src).unwrap();
        assert_eq!(hits.iter().map(|h| h.capture.as_str()).collect::<Vec<_>>(), vec!["logic"]);
    }

    #[test]
    fn nested_captures_report_the_outermost_once() {
        let src = "const v = <div style={{ top: y - A / 2 }}>{a + b}</div>;";
        let mut p = Pattern::new(r#"(binary_expression operator: ["+" "-" "/"]) @arithmetic"#).within(vec!["jsx_expression".into()]);
        let hits = p.find("v.tsx", src).unwrap();
        assert_eq!(hits.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(), vec!["y - A / 2", "a + b"]);
    }

    #[test]
    fn empty_handler_bodies_are_not_logic() {
        let src = "const v = <b onClick={() => {}} onKey={() => { go(); }} />;";
        let mut p = Pattern::new("(arrow_function body: (statement_block (_))) @handler_body").within(vec!["jsx_attribute".into()]);
        let hits = p.find("v.jsx", src).unwrap();
        assert_eq!(hits.iter().map(|h| h.col).collect::<Vec<_>>(), vec![40]);
    }

    #[test]
    fn any_registered_language_works() {
        let rust = "fn f(x: Option<u8>) -> u8 { x.unwrap() }";
        let q = r#"((call_expression function: (field_expression field: (field_identifier) @_m)) @unwrap (#eq? @_m "unwrap"))"#;
        assert_eq!(Pattern::new(q).find("src/lib.rs", rust).unwrap().len(), 1);
        let py = "def f():\n    print('x')\n    return 1\n";
        let q = r#"((call function: (identifier) @_f) @print (#eq? @_f "print"))"#;
        assert_eq!(Pattern::new(q).find("app/main.py", py).unwrap()[0].line, 2);
        let go = "package m\nfunc f() { panic(\"no\") }\n";
        let q = r#"((call_expression function: (identifier) @_f) @panic (#eq? @_f "panic"))"#;
        assert_eq!(Pattern::new(q).find("m.go", go).unwrap()[0].text, "panic(\"no\")");
        assert!(Pattern::new(q).find("notes.unknown-ext", go).unwrap().is_empty());
    }

    #[test]
    fn scan_parses_once_and_splits_hits_per_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.tsx"), "const v = <div>{x ? 1 : 2}{n + 1}</div>;").unwrap();
        std::fs::write(dir.path().join("b.py"), "x = 1 if y else 2\n").unwrap();
        let mut ps = vec![
            Pattern::new("(ternary_expression) @t").within(vec!["jsx_expression".into()]),
            Pattern::new("(binary_expression) @b").within(vec!["jsx_expression".into()]),
            Pattern::new("(conditional_expression) @c"),
        ];
        let files = vec![("a.tsx".to_string(), vec![0, 1]), ("b.py".to_string(), vec![2])];
        let out = scan(dir.path(), &files, &mut ps).unwrap();
        let got: Vec<Vec<(&str, &str)>> = out.iter().map(|v| v.iter().map(|(f, h)| (f.as_str(), h.capture.as_str())).collect()).collect();
        assert_eq!(got, vec![vec![("a.tsx", "t")], vec![("a.tsx", "b")], vec![("b.py", "c")]]);
    }
}
