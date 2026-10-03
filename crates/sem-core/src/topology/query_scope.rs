//! Branch-scoped predicates for tree-sitter queries.
//!
//! Tree-sitter attaches every predicate to the whole *pattern*, even one
//! written inside a branch of an alternation `[ .. ]`. When two branches
//! reuse a capture name, each with its own predicate:
//!
//! ```text
//! [((call function: (identifier) @_f) (#eq? @_f "print"))
//!  ((call function: (identifier) @_f) (#eq? @_f "exit"))]
//! ```
//!
//! the pattern demands `@_f == "print"` AND `@_f == "exit"` and never
//! matches — a law written this way is silently vacuous. The intended
//! meaning is "each branch's predicates constrain that branch". Since a
//! predicate over a capture that is absent from a match holds, renaming a
//! branch's predicate-referenced captures to names unique to that branch
//! gives exactly that meaning (quantifiers on the alternation keep working).
//! Renamed captures carry a `__alt<n>` suffix; [`display_name`] strips it.
//!
//! A branch predicate over a capture defined *outside* its branch cannot be
//! scoped this way (it would constrain every branch); that is an error.

const SUFFIX: &str = "__alt";

/// The capture name as written in the law (without a branch suffix).
pub fn display_name(name: &str) -> &str {
    match name.rfind(SUFFIX) {
        Some(i) if !name[i + SUFFIX.len()..].is_empty() && name[i + SUFFIX.len()..].bytes().all(|b| b.is_ascii_digit()) => &name[..i],
        _ => name,
    }
}

#[derive(Debug)]
enum Tok {
    Open(u8, usize),
    Close(usize),
    Atom(usize, usize),
}

#[derive(Debug)]
enum Node {
    /// `(` or `[` list: open byte, children.
    List(u8, Vec<Node>),
    /// Byte range of an atom (identifier, string, capture, field, quantifier).
    Atom(usize, usize),
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b';' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            c if c.is_ascii_whitespace() => i += 1,
            b'(' | b'[' => {
                out.push(Tok::Open(b[i], i));
                i += 1;
            }
            b')' | b']' => {
                out.push(Tok::Close(i));
                i += 1;
            }
            b'"' => {
                let s = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                if i >= b.len() {
                    return Err("unterminated string in query".into());
                }
                i += 1;
                out.push(Tok::Atom(s, i));
            }
            _ => {
                let s = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'(' | b')' | b'[' | b']' | b'"' | b';') {
                    i += 1;
                }
                out.push(Tok::Atom(s, i));
            }
        }
    }
    Ok(out)
}

fn parse(toks: &[Tok], pos: &mut usize, until_close: bool) -> Result<Vec<Node>, String> {
    let mut out = Vec::new();
    while *pos < toks.len() {
        match toks[*pos] {
            Tok::Open(c, _) => {
                *pos += 1;
                let kids = parse(toks, pos, true)?;
                out.push(Node::List(c, kids));
            }
            Tok::Close(_) => {
                if !until_close {
                    return Err("unbalanced `)`/`]` in query".into());
                }
                *pos += 1;
                return Ok(out);
            }
            Tok::Atom(s, e) => {
                out.push(Node::Atom(s, e));
                *pos += 1;
            }
        }
    }
    if until_close {
        return Err("unclosed `(`/`[` in query".into());
    }
    Ok(out)
}

fn text<'a>(src: &'a str, n: &Node) -> Option<&'a str> {
    match n {
        Node::Atom(s, e) => Some(&src[*s..*e]),
        _ => None,
    }
}

fn is_predicate(src: &str, n: &Node) -> bool {
    matches!(n, Node::List(b'(', kids) if kids.first().and_then(|k| text(src, k)).is_some_and(|t| t.starts_with('#')))
}

/// Capture definitions (outside predicates) and predicate capture references
/// in `nodes`, as byte ranges.
fn collect(src: &str, nodes: &[Node], defs: &mut Vec<(usize, usize)>, refs: &mut Vec<(usize, usize)>) {
    for n in nodes {
        match n {
            Node::Atom(s, e) if src[*s..*e].starts_with('@') => defs.push((*s, *e)),
            Node::Atom(..) => {}
            Node::List(_, kids) if is_predicate(src, n) => {
                for k in kids {
                    if let Node::Atom(s, e) = k {
                        if src[*s..*e].starts_with('@') {
                            refs.push((*s, *e));
                        }
                    }
                }
            }
            Node::List(_, kids) => collect(src, kids, defs, refs),
        }
    }
}

/// Split an alternation's children into branches: a branch is one node plus
/// any leading prefixes (`field:`, `!field`, `.`) and trailing captures and
/// quantifiers.
fn branches<'n>(src: &str, kids: &'n [Node]) -> Vec<&'n [Node]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let trailing = |n: &Node| text(src, n).is_some_and(|t| t.starts_with('@') || matches!(t, "?" | "*" | "+"));
    let prefix = |n: &Node| text(src, n).is_some_and(|t| t.ends_with(':') || t.starts_with('!') || t == ".");
    while i < kids.len() {
        while i < kids.len() && prefix(&kids[i]) {
            i += 1;
        }
        i += 1; // the branch's node
        while i < kids.len() && trailing(&kids[i]) {
            i += 1;
        }
        let end = i.min(kids.len());
        if end > start {
            out.push(&kids[start..end]);
        }
        start = end;
    }
    out
}

fn walk(src: &str, nodes: &[Node], counter: &mut usize, edits: &mut Vec<(usize, usize, String)>) -> Result<(), String> {
    for n in nodes {
        let Node::List(open, kids) = n else { continue };
        walk(src, kids, counter, edits)?;
        if *open != b'[' {
            continue;
        }
        for br in branches(src, kids) {
            let (mut defs, mut refs) = (Vec::new(), Vec::new());
            collect(src, br, &mut defs, &mut refs);
            if refs.is_empty() {
                continue;
            }
            *counter += 1;
            let tag = format!("{SUFFIX}{counter}");
            let names: std::collections::BTreeSet<&str> = defs.iter().map(|&(s, e)| &src[s..e]).collect();
            for &(s, e) in &refs {
                if !names.contains(&src[s..e]) {
                    return Err(format!(
                        "predicate on {} inside an alternation branch that does not define it: tree-sitter would apply it to every branch; \
                         capture it inside the branch or move the predicate out of the alternation",
                        &src[s..e]
                    ));
                }
            }
            let referenced: std::collections::BTreeSet<&str> = refs.iter().map(|&(s, e)| &src[s..e]).collect();
            for &(s, e) in defs.iter().chain(refs.iter()) {
                if referenced.contains(&src[s..e]) && !src[s..e].contains(SUFFIX) {
                    edits.push((s, e, format!("{}{tag}", &src[s..e])));
                }
            }
        }
    }
    Ok(())
}

/// `src` with every alternation branch's predicates scoped to that branch.
/// Queries without predicates inside alternations come back unchanged.
pub fn scope_branch_predicates(src: &str) -> Result<String, String> {
    let toks = tokenize(src)?;
    let mut pos = 0;
    let tree = parse(&toks, &mut pos, false)?;
    let mut edits = Vec::new();
    walk(src, &tree, &mut 0, &mut edits)?;
    if edits.is_empty() {
        return Ok(src.to_string());
    }
    edits.sort_by_key(|e| e.0);
    edits.dedup_by_key(|e| e.0);
    let mut out = String::with_capacity(src.len() + edits.len() * 8);
    let mut at = 0;
    for (s, e, t) in edits {
        out.push_str(&src[at..s]);
        out.push_str(&t);
        at = e;
    }
    out.push_str(&src[at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{display_name, scope_branch_predicates};

    #[test]
    fn renames_branch_captures_per_branch() {
        let q = r#"[((call function: (identifier) @_f) @hit (#eq? @_f "print")) ((call function: (identifier) @_f) @hit (#eq? @_f "exit"))]"#;
        let out = scope_branch_predicates(q).unwrap();
        assert!(out.contains("@_f__alt1") && out.contains("@_f__alt2"), "{out}");
        assert_eq!(out.matches("@hit").count(), 2, "unreferenced captures keep their names: {out}");
    }

    #[test]
    fn leaves_queries_without_branch_predicates_alone() {
        for q in [
            r#"((call function: (identifier) @_f) @print (#eq? @_f "print"))"#,
            r#"(binary_expression operator: ["+" "-"]) @arith"#,
            "; comment with [ and (\n(identifier) @x",
        ] {
            assert_eq!(scope_branch_predicates(q).unwrap(), q);
        }
    }

    #[test]
    fn predicate_on_outer_capture_inside_branch_is_an_error() {
        let q = r#"(call function: (identifier) @_f arguments: [((string) (#eq? @_f "x")) (integer)]) @c"#;
        assert!(scope_branch_predicates(q).unwrap_err().contains("@_f"));
    }

    #[test]
    fn strings_with_brackets_and_escapes_are_atoms() {
        let q = r#"[((string) @_s (#match? @_s "\"[a-z]\\(")) ((string) @_s (#eq? @_s "]"))]"#;
        let out = scope_branch_predicates(q).unwrap();
        assert!(out.contains(r#""\"[a-z]\\(""#) && out.contains("@_s__alt1") && out.contains("@_s__alt2"), "{out}");
    }

    #[test]
    fn display_name_strips_only_the_branch_suffix() {
        assert_eq!(display_name("hit__alt12"), "hit");
        assert_eq!(display_name("_f__alt3"), "_f");
        assert_eq!(display_name("x__alt"), "x__alt");
        assert_eq!(display_name("plain"), "plain");
    }
}
