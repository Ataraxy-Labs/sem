//! Qualified entity addresses: `Class.method`, `module.func`,
//! `pkg.mod.Class.method`, `Owner::member`, `path/to/file.py::Owner::member`
//! and `name@line` / `name@L<line>`.
//!
//! One matcher, two parent-chain sources: the mmap index
//! (`index::Entity::parent_index`) and the in-memory graph (`parent_id`).
//! Both call [`matches`], so `find`/`callers`/`refs` (index) and
//! `impact`/`context` (graph) agree on what a qualified name names.
//!
//! Semantics of `a.b.C.m` against an entity named `m`:
//! - the qualifiers are consumed right to left against the entity's owner
//!   chain (`C` must be the direct owner, then `b` the owner's owner, …);
//!   an owner chain may stop early (a top-level class has none);
//! - whatever qualifiers remain must name the entity's *file*, as a
//!   contiguous run of its module path (`django/db/models/query.py` is the
//!   module path `django.db.models.query`; `__init__`, `mod`, `index` name
//!   their directory), so `query.QuerySet.acreate`,
//!   `models.query.QuerySet.acreate` and `django.db.models.query.QuerySet.acreate`
//!   all match, and `sets.QuerySet.acreate` does not;
//! - a qualifier that looks like a path (`/` or a file extension) must equal
//!   the file path, and the id-form kind words (`class`, `function`, …) are
//!   skipped, so a pasted entity id resolves too.

use sem_core::index::QueryIndex;
use sem_core::parser::graph::{EntityGraph, EntityInfo};

/// A parsed query. `exact` is the raw string (names may contain `.`, `::` or
/// spaces: a JS `Foo.bar` entity, a `getter value`), always tried first.
#[derive(Debug, Clone)]
pub struct Query<'a> {
    pub raw: &'a str,
    pub kind: Option<&'a str>,
    /// Qualifiers, outermost first, then the bare name last.
    pub segments: Vec<&'a str>,
    pub line: Option<usize>,
}

impl<'a> Query<'a> {
    pub fn bare(&self) -> &'a str {
        self.segments.last().copied().unwrap_or(self.raw)
    }
}

const ID_KIND_WORDS: &[&str] = &[
    "class", "function", "method", "struct", "enum", "trait", "impl", "interface", "module",
    "variable", "constant", "property", "type", "field", "orphan",
];

pub fn parse(raw: &str) -> Query<'_> {
    let mut rest = raw.trim();
    let mut kind = None;
    if let Some((t, n)) = rest.split_once(' ') {
        if !t.is_empty() && !n.is_empty() && !t.contains(['.', ':', '/']) {
            kind = Some(t);
            rest = n.trim();
        }
    }
    let mut line = None;
    if let Some((name, sel)) = rest.rsplit_once('@') {
        let digits = sel.strip_prefix('L').unwrap_or(sel);
        if !name.is_empty() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            line = digits.parse().ok();
            rest = name;
        }
    }
    let segments: Vec<&str> = if rest.contains("::") {
        rest.split("::").filter(|s| !s.is_empty()).collect()
    } else if looks_like_path(rest) {
        vec![rest]
    } else {
        rest.split('.').filter(|s| !s.is_empty()).collect()
    };
    Query { raw, kind, segments, line }
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/')
}

fn has_source_ext(s: &str) -> bool {
    matches!(
        s.rsplit_once('.').map(|(_, e)| e),
        Some("py" | "pyi" | "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "go" | "java" | "rb" | "php" | "kt" | "swift" | "c" | "h" | "cc" | "cpp" | "hpp" | "cs" | "scala" | "dart" | "ex" | "exs" | "vue" | "svelte")
    )
}

/// `--file` names a file, or a directory: agents pass `django/contrib/admin`
/// as often as `django/contrib/admin/options.py`.
pub fn in_scope(path: &str, file: &str) -> bool {
    let f = file.trim_end_matches('/');
    f.is_empty() || f == "." || path == f || (path.len() > f.len() && path.starts_with(f) && path.as_bytes()[f.len()] == b'/')
}

/// The module path of a file: extension dropped, `/` separated, a trailing
/// package-marker file (`__init__`, `mod`, `index`, `lib`) naming its
/// directory instead.
pub fn module_segments(file_path: &str) -> Vec<&str> {
    let no_ext = match file_path.rsplit_once('.') {
        Some((stem, ext)) if !ext.contains('/') => stem,
        _ => file_path,
    };
    let mut segs: Vec<&str> = no_ext.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    if segs.len() > 1 && matches!(segs.last(), Some(&("__init__" | "mod" | "index" | "lib"))) {
        segs.pop();
    }
    segs
}

fn contiguous_run(hay: &[&str], needle: &[&str]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

/// Does an entity with this `name`, owner chain (innermost owner first),
/// `file_path` and span answer `q`?
pub fn matches(q: &Query<'_>, name: &str, kind: &str, owners: &[&str], file_path: &str, span: (usize, usize)) -> bool {
    if q.kind.is_some_and(|k| k != kind) {
        return false;
    }
    if let Some(l) = q.line {
        if l < span.0 || l > span.1 {
            return false;
        }
    }
    if q.bare() != name {
        return false;
    }
    let quals = &q.segments[..q.segments.len().saturating_sub(1)];
    // consume owners right to left
    let mut i = quals.len();
    let mut o = 0;
    while i > 0 && o < owners.len() && quals[i - 1] == owners[o] {
        i -= 1;
        o += 1;
    }
    let rest: Vec<&str> = quals[..i].iter().copied().filter(|s| !ID_KIND_WORDS.contains(s)).collect();
    if rest.is_empty() {
        return true;
    }
    // a pasted id or `file.py::Owner::m`: the path qualifier names the file
    if let Some(p) = rest.iter().find(|s| looks_like_path(s) || has_source_ext(s)) {
        let others: Vec<&str> = rest.iter().copied().filter(|s| s != p).collect();
        let path_ok = file_path == *p || file_path.ends_with(&format!("/{p}"));
        return path_ok && others.is_empty();
    }
    contiguous_run(&module_segments(file_path), &rest)
}

/// Owner names of an index entity, innermost first.
pub fn index_owners<'a>(idx: &'a QueryIndex, at: usize) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut cur = idx.entity(at).parent_index();
    let mut guard = 0;
    while let Some(p) = cur {
        let e = idx.entity(p);
        out.push(e.name());
        cur = e.parent_index();
        guard += 1;
        if guard > 64 {
            break;
        }
    }
    out
}

/// Owner names of a graph entity, innermost first.
pub fn graph_owners<'a>(graph: &'a EntityGraph, e: &EntityInfo) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut cur = e.parent_id.as_ref();
    let mut guard = 0;
    while let Some(pid) = cur {
        let Some(p) = graph.entities.get(pid.as_str()) else { break };
        out.push(p.name.as_str());
        cur = p.parent_id.as_ref();
        guard += 1;
        if guard > 64 {
            break;
        }
    }
    out
}

/// Owner names from a flat list of entities of one file (a re-extract).
pub fn list_owners<'a>(all: &'a [EntityInfo], e: &EntityInfo) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut cur = e.parent_id.as_ref();
    let mut guard = 0;
    while let Some(pid) = cur {
        let Some(p) = all.iter().find(|x| &x.id == pid) else { break };
        out.push(p.name.as_str());
        cur = p.parent_id.as_ref();
        guard += 1;
        if guard > 64 {
            break;
        }
    }
    out
}

/// `Owner.Inner.name` display form (owners outermost first).
pub fn display_name(owners: &[&str], name: &str) -> String {
    let mut s = String::new();
    for o in owners.iter().rev() {
        s.push_str(o);
        s.push('.');
    }
    s.push_str(name);
    s
}

/// With a line selector several nested entities can contain the line
/// (a method and its class): keep the one starting on it, else the innermost.
pub fn narrow_by_line<T>(q: &Query<'_>, hits: Vec<T>, span: impl Fn(&T) -> (usize, usize)) -> Vec<T> {
    let Some(l) = q.line else { return hits };
    if hits.len() <= 1 {
        return hits;
    }
    if hits.iter().any(|h| span(h).0 == l) {
        return hits.into_iter().filter(|h| span(h).0 == l).collect();
    }
    let min = hits.iter().map(|h| span(h).1 - span(h).0).min().unwrap_or(0);
    hits.into_iter().filter(|h| span(h).1 - span(h).0 == min).collect()
}

/// Every index entity a (possibly qualified) query names.
pub fn resolve_index(idx: &QueryIndex, raw: &str, file: Option<&str>) -> Vec<usize> {
    let q = parse(raw);
    let hits: Vec<usize> = idx
        .lookup(q.bare())
        .iter()
        .filter(|e| file.is_none_or(|f| in_scope(e.file_path(), f)))
        .filter(|e| {
            let owners = index_owners(idx, e.index());
            matches(&q, e.name(), e.entity_type(), &owners, e.file_path(), (e.start_line(), e.end_line()))
        })
        .map(|e| e.index())
        .collect();
    narrow_by_line(&q, hits, |&at| {
        let e = idx.entity(at);
        (e.start_line(), e.end_line())
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NearMatch {
    pub qualified_name: String,
    #[serde(rename = "type")]
    pub entity_type: String,
    pub file: String,
    pub start_line: usize,
    pub why: &'static str,
}

fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Levenshtein distance, early-exit above `max`.
fn edit_distance(a: &str, b: &str, max: usize) -> Option<usize> {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max {
            return None;
        }
        prev = cur;
    }
    (prev[b.len()] <= max).then_some(prev[b.len()])
}

/// Near matches for a query that resolved to nothing, best first:
/// 1. the bare name under another owner / in another module;
/// 2. the same name up to case and punctuation (`bad-name-rgxs`);
/// 3. a qualified form within a small edit distance (`Permutaton._af_new`);
/// 4. names containing the bare name, or within edit distance 2 of it.
pub fn near_matches(idx: &QueryIndex, raw: &str, file: Option<&str>, cap: usize) -> Vec<NearMatch> {
    let q = parse(raw);
    let bare = q.bare();
    let want_display = q.segments.join(".");
    let norm_bare = normalize(bare);
    let mut scored: Vec<(u8, usize, NearMatch)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |rank: u8, dist: usize, at: usize, why: &'static str, scored: &mut Vec<(u8, usize, NearMatch)>| {
        if !seen.insert(at) {
            return;
        }
        let e = idx.entity(at);
        let owners = index_owners(idx, at);
        scored.push((
            rank,
            dist,
            NearMatch {
                qualified_name: display_name(&owners, e.name()),
                entity_type: e.entity_type().to_string(),
                file: e.file_path().to_string(),
                start_line: e.start_line(),
                why,
            },
        ));
    };
    for e in idx.lookup(bare) {
        let why = if file.is_some_and(|f| !in_scope(e.file_path(), f)) { "same name, other file" } else { "same name, other owner or module" };
        push(0, 0, e.index(), why, &mut scored);
    }
    let n = idx.entity_count();
    let short = norm_bare.len() < 4;
    for at in 0..n {
        if scored.len() > 4000 {
            break;
        }
        let e = idx.entity(at);
        let name = e.name();
        if name == bare || name.is_empty() {
            continue;
        }
        let norm = normalize(name);
        if !norm_bare.is_empty() && norm == norm_bare {
            push(1, 0, at, "same name up to case or punctuation", &mut scored);
            continue;
        }
        if !short {
            if let Some(d) = edit_distance(&norm, &norm_bare, 2) {
                push(3, d, at, "similar name", &mut scored);
                continue;
            }
            if norm.contains(&norm_bare) && name.len() <= bare.len() * 3 {
                push(4, name.len() - bare.len().min(name.len()), at, "name contains the query", &mut scored);
            }
        }
    }
    // qualified-form typos: same bare name, owner chain within edit distance
    if q.segments.len() > 1 {
        for (rank, dist, m) in scored.iter_mut() {
            if *rank == 0 {
                if let Some(d) = edit_distance(&m.qualified_name, &want_display, 3) {
                    *dist = d;
                } else {
                    *dist = 100 + *dist;
                }
            }
        }
    }
    scored.sort_by(|a, b| (a.0, a.1, &a.2.file, a.2.start_line).cmp(&(b.0, b.1, &b.2.file, b.2.start_line)));
    scored.into_iter().take(cap).map(|(_, _, m)| m).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(q: &str, name: &str, owners: &[&str], file: &str) -> bool {
        matches(&parse(q), name, "function", owners, file, (10, 20))
    }

    #[test]
    fn owner_chain_and_module_suffix() {
        let f = "sympy/combinatorics/permutations.py";
        assert!(m("_af_new", "_af_new", &["Permutation"], f));
        assert!(m("Permutation._af_new", "_af_new", &["Permutation"], f));
        assert!(m("permutations.Permutation._af_new", "_af_new", &["Permutation"], f));
        assert!(m("sympy.combinatorics.permutations.Permutation._af_new", "_af_new", &["Permutation"], f));
        assert!(m("sympy.combinatorics.Permutation._af_new", "_af_new", &["Permutation"], f));
        assert!(!m("sets.Permutation._af_new", "_af_new", &["Permutation"], f));
        assert!(!m("Cycle._af_new", "_af_new", &["Permutation"], f));
        assert!(m("Permutation::_af_new", "_af_new", &["Permutation"], f));
        assert!(m("sympy/combinatorics/permutations.py::class::Permutation::_af_new", "_af_new", &["Permutation"], f));
        assert!(m("permutations.py::Permutation::_af_new", "_af_new", &["Permutation"], f));
        assert!(!m("other.py::Permutation::_af_new", "_af_new", &["Permutation"], f));
    }

    #[test]
    fn nested_class_in_function_and_package_init() {
        let f = "django/db/models/fields/related_descriptors.py";
        let owners = ["RelatedManager", "create_reverse_many_to_one_manager"];
        assert!(m("RelatedManager.create", "create", &owners, f));
        assert!(m("create_reverse_many_to_one_manager.RelatedManager.create", "create", &owners, f));
        assert!(m("related_descriptors.RelatedManager.create", "create", &owners, f));
        assert!(m("pkg.helper", "helper", &[], "pkg/__init__.py"));
    }

    #[test]
    fn line_and_kind_selectors() {
        let q = parse("is_subset@349");
        assert_eq!(q.line, Some(349));
        assert_eq!(q.bare(), "is_subset");
        assert!(matches(&q, "is_subset", "function", &["Set"], "a.py", (349, 396)));
        assert!(!matches(&q, "is_subset", "function", &["Set"], "a.py", (1278, 1290)));
        let q = parse("is_subset@L349");
        assert_eq!(q.line, Some(349));
        let q = parse("class Permutation");
        assert_eq!(q.kind, Some("class"));
        assert!(matches(&q, "Permutation", "class", &[], "a.py", (1, 2)));
        assert!(!matches(&q, "Permutation", "function", &[], "a.py", (1, 2)));
        // `@` without digits is part of a name (decorator-ish ids stay intact)
        assert_eq!(parse("foo@bar").bare(), "foo@bar");
    }

    #[test]
    fn file_scope_is_a_file_or_a_directory() {
        assert!(in_scope("a/b/c.py", "a/b/c.py"));
        assert!(in_scope("a/b/c.py", "a/b"));
        assert!(in_scope("a/b/c.py", "a/b/"));
        assert!(!in_scope("a/bc/c.py", "a/b"));
        assert!(!in_scope("a/b/c.py", "a/b/c"));
    }

    #[test]
    fn module_segments_drop_package_markers() {
        assert_eq!(module_segments("a/b/__init__.py"), vec!["a", "b"]);
        assert_eq!(module_segments("src/lib.rs"), vec!["src"]);
        assert_eq!(module_segments("a/b/c.py"), vec!["a", "b", "c"]);
    }

    #[test]
    fn edit_distance_bounds() {
        assert_eq!(edit_distance("Permutaton", "Permutation", 2), Some(1));
        assert_eq!(edit_distance("abc", "xyz", 2), None);
    }
}
