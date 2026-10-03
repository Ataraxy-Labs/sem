//! `sem certify <base>..<head>`: a review certificate for a change.
//!
//! Machine-computed facts about what a commit range touches and what it can
//! reach, for a reviewer to read next to the diff. Every section is derived
//! from the two trees alone (plus any laws the repo keeps in
//! `.sem/promises/*.json` or passed with `--laws`):
//!
//! - entities added / modified / deleted / renamed (the semantic diff);
//! - signature changes, with every static caller at head marked as updated
//!   in this change or not;
//! - for every modified or deleted entity, its static callers that this
//!   change does not touch;
//! - calls a modified entity stopped making or started making (callee delta);
//! - laws: kept, newly broken (with witness), or fixed, base vs head;
//! - JS/TS: module edges added/removed and runtime reachability growth;
//! - tests that statically reach a changed entity;
//! - the static reference cone: what has no reference path to the change.
//!
//! Static only. Dynamic dispatch the resolver cannot pin, reflection,
//! string-keyed lookups and non-code consumers are not modeled, and the
//! certificate says so rather than claiming more than it computed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use sem_core::git::bridge::GitBridge;
use sem_core::git::types::DiffScope;
use sem_core::model::change::{ChangeType, SemanticChange};
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::differ::compute_semantic_diff;
use sem_core::parser::graph::{EntityGraph, EntityInfo, RefType};

use super::topology::{check, Common, Ctx};

pub struct CertifyOptions {
    pub cwd: String,
    pub range: String,
    pub laws: Vec<PathBuf>,
    pub json: bool,
    /// Cap on items listed per section in the markdown render.
    pub max_items: usize,
    /// Cap on the markdown render's total size, in characters.
    pub max_chars: usize,
}

pub(crate) struct Tree {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) graph: EntityGraph,
    pub(crate) entities: Vec<SemanticEntity>,
    /// The files analyzed: every supported file, or a diff-scoped region's.
    pub(crate) files: Vec<String>,
    /// Every supported file of the tree.
    pub(crate) all_files: Vec<String>,
    /// The region `files` was restricted to (`None`: the whole tree).
    pub(crate) scope: Option<HashSet<String>>,
}

pub(crate) fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").current_dir(root).args(args).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `git archive <sha> | tar -x -C <dir>`: the committed tree, nothing else.
fn materialize(root: &Path, sha: &str) -> Result<tempfile::TempDir, String> {
    let dir = tempfile::Builder::new().prefix("sem-certify-").tempdir().map_err(|e| e.to_string())?;
    let mut archive = Command::new("git")
        .current_dir(root)
        .args(["archive", "--format=tar", sha])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let status = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dir.path())
        .stdin(archive.stdout.take().ok_or("no archive stdout")?)
        .status()
        .map_err(|e| e.to_string())?;
    let a = archive.wait().map_err(|e| e.to_string())?;
    if !status.success() || !a.success() {
        return Err(format!("could not materialize {sha}"));
    }
    Ok(dir)
}

/// A materialized tree and its supported source files.
fn list_tree(root: &Path, sha: &str) -> Result<(tempfile::TempDir, Vec<String>), String> {
    let dir = materialize(root, sha)?;
    let registry = super::create_registry(&dir.path().to_string_lossy());
    let files = super::graph::find_supported_files_public(dir.path(), &registry, &[]);
    Ok((dir, files))
}

fn graph_tree(dir: tempfile::TempDir, all_files: Vec<String>, scope: Option<&HashSet<String>>) -> Tree {
    let registry = super::create_registry(&dir.path().to_string_lossy());
    let files: Vec<String> = match scope {
        Some(r) => all_files.iter().filter(|f| r.contains(f.as_str())).cloned().collect(),
        None => all_files.clone(),
    };
    let (graph, entities) = EntityGraph::build(dir.path(), &files, &registry);
    Tree { dir, graph, entities, files, all_files, scope: scope.cloned() }
}

/// How much of the two trees `arch-diff` analyzes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Scope {
    /// Every file.
    Full,
    /// The diff's region (see `region`), within this many source bytes.
    Diff { budget: u64 },
    /// Full when the head tree has at most `full_max` bytes of code, else
    /// diff-scoped.
    Auto { full_max: u64, budget: u64 },
}

/// The paths a range touches and its semantic changes.
pub(crate) fn semantic_changes(root: &Path, base: &str, head: &str) -> Result<(BTreeSet<String>, Vec<SemanticChange>), Box<dyn std::error::Error>> {
    let bridge = GitBridge::open(root)?;
    let file_changes = bridge.get_changed_files(&DiffScope::Range { from: base.to_string(), to: head.to_string() }, &[])?;
    let registry = super::create_registry(&root.to_string_lossy());
    let diff = compute_semantic_diff(&file_changes, &registry, None, None);
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for f in &file_changes {
        paths.insert(f.file_path.clone());
        if let Some(o) = &f.old_file_path {
            paths.insert(o.clone());
        }
    }
    Ok((paths, diff.changes))
}

/// Both trees of a range under `scope`, and the region when diff-scoped.
pub(crate) fn build_trees_in(root: &Path, base: &str, head: &str, scope: Scope) -> Result<(Tree, Tree, Option<super::region::Region>), Box<dyn std::error::Error>> {
    let (bl, hl) = std::thread::scope(|s| {
        let b = s.spawn(|| list_tree(root, base));
        let h = s.spawn(|| list_tree(root, head));
        (b.join().expect("base materialize"), h.join().expect("head materialize"))
    });
    let ((bd, bf), (hd, hf)) = (bl?, hl?);
    let head_bytes = sem_core::parser::graph::source_bytes(hd.path(), &hf);
    // the whole-tree cost is that of code (data, markup and config files are cheap)
    let code: Vec<String> = hf.iter().filter(|f| super::region::is_code(f)).cloned().collect();
    let code_bytes = sem_core::parser::graph::source_bytes(hd.path(), &code);
    let budget = match scope {
        Scope::Full => None,
        Scope::Diff { budget } => Some(budget),
        Scope::Auto { full_max, budget } => (code_bytes > full_max).then_some(budget),
    };
    let region = match budget {
        Some(budget) => {
            let (changed, changes) = semantic_changes(root, base, head)?;
            Some(super::region::select([(bd.path(), &bf), (hd.path(), &hf)], &changed, &changes, budget))
        }
        None => None,
    };
    if std::env::var_os("SEM_TIMINGS").is_some() {
        match &region {
            Some(r) => eprintln!("scope diff: head {head_bytes} source bytes, {code_bytes} code; region {} files, {} bytes of {} files; {} names not followed", r.files.len(), r.bytes, r.repo_files, r.unexplored.len()),
            None => eprintln!("scope full: head {head_bytes} source bytes, {code_bytes} code"),
        }
    }
    let r = region.as_ref().map(|r| &r.files);
    let analyzed = match r {
        Some(_) => region.as_ref().map_or(0, |r| r.bytes),
        None => head_bytes,
    };
    if analyzed > PARALLEL_BUILD_MAX_BYTES {
        let bt = graph_tree(bd, bf, r);
        let ht = graph_tree(hd, hf, r);
        return Ok((bt, ht, region));
    }
    let (bt, ht) = std::thread::scope(|s| {
        let b = s.spawn(|| graph_tree(bd, bf, r));
        let h = s.spawn(|| graph_tree(hd, hf, r));
        (b.join().expect("base build"), h.join().expect("head build"))
    });
    Ok((bt, ht, region))
}

/// Source bytes above which the two trees' graphs are built one after the
/// other: each build already uses every core, and two concurrent builds of a
/// large tree double the peak.
const PARALLEL_BUILD_MAX_BYTES: u64 = 24 * 1024 * 1024;

/// The callable header of an entity: text up to the end of its first
/// parameter list plus any return annotation, whitespace-normalized.
/// Leading decorator/attribute lines are skipped. `None` for entities with
/// no parameter list in their first lines (classes, constants, ...).
fn signature(content: &str) -> Option<String> {
    let body: String = content
        .lines()
        .skip_while(|l| {
            let t = l.trim_start();
            t.starts_with('@') || t.starts_with("#[") || t.starts_with("//") || t.starts_with('#') && !t.starts_with("#!")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let open = body.find('(')?;
    // A parameter list must start on the entity's first (non-decorator) line.
    if body[..open].contains('\n') {
        return None;
    }
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in body[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    // Return annotation: up to the body opener.
    let rest = &body[close + 1..];
    let mut end = rest.len();
    for pat in ["{", "=>", ":\n", ":\r", "\n"] {
        if let Some(i) = rest.find(pat) {
            end = end.min(i);
        }
    }
    if rest[..end].trim_end().ends_with(':') {
        end = rest[..end].trim_end().len() - 1;
    }
    let raw = format!("{}{}", &body[..=close], &rest[..end]);
    let norm = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let norm = norm.replace("( ", "(").replace(" )", ")").replace(",)", ")");
    Some(norm.chars().take(240).collect())
}

/// Entity types that declare a value, not a callable.
const VALUE_TYPES: &[&str] = &["variable", "constant", "const", "var", "static", "field", "property"];

/// Does a value declaration hold a function (`const f = (a) => ..`,
/// `var h = func(..)`)? Then its parameter list is a contract.
fn holds_callable(content: &str) -> bool {
    let first = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let rhs = first.split_once('=').map(|x| x.1).unwrap_or("").trim_start();
    content.contains("=>") || rhs.starts_with("func(") || rhs.starts_with("function") || rhs.starts_with("async") || rhs.starts_with("lambda")
}

/// The declared type of a value declaration, whitespace-normalized
/// (`var (x T = e)`, `var x T = e`, `const x: T = e`, `static X: T = e`
/// -> `T`); empty when none is written. The initializer is not part of
/// the contract, nor is the `var ( .. )` grouping or a comment.
fn declared_type(content: &str) -> String {
    let code: String = content
        .lines()
        .map(|l| {
            let t = l.trim();
            let t = t.split_once("//").map(|x| x.0).unwrap_or(t);
            if t.starts_with('#') { "" } else { t }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut t = code.trim();
    loop {
        let before = t;
        for kw in ["export ", "declare ", "pub(crate) ", "pub ", "public ", "private ", "protected ", "readonly ", "static ", "var ", "let ", "const ", "mut "] {
            t = t.strip_prefix(kw).unwrap_or(t).trim_start();
        }
        if let Some(x) = t.strip_prefix('(') {
            t = x.trim_start();
            t = t.strip_suffix(')').unwrap_or(t).trim_end();
        }
        if t == before {
            break;
        }
    }
    // up to the top-level `=` (not `=>`, `==`, `<=`, `>=`, `!=`, `:=`)
    let b = t.as_bytes();
    let mut depth = 0i32;
    let mut end = t.len();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' | b'>' => depth -= 1,
            b'=' if depth <= 0 => {
                let next = b.get(i + 1).copied();
                let prev = if i > 0 { b[i - 1] } else { b' ' };
                if next != Some(b'=') && next != Some(b'>') && !matches!(prev, b'=' | b'!' | b'<' | b'>' | b':') {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    let decl = t[..end].trim();
    // drop the name
    let rest = decl.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_' || c == '$').trim_start();
    let rest = rest.trim_start_matches('?').trim_start_matches(':').trim();
    rest.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The contract text of an entity for signature comparison: the callable
/// header, or for a value declaration its declared type.
fn contract(entity_type: &str, content: &str) -> Option<String> {
    if VALUE_TYPES.contains(&entity_type) && !holds_callable(content) {
        return Some(format!("type {}", declared_type(content)).trim_end().to_string());
    }
    signature(content)
}

fn kind_str(c: &ChangeType) -> &'static str {
    match c {
        ChangeType::Added => "added",
        ChangeType::Modified => "modified",
        ChangeType::Deleted => "deleted",
        ChangeType::Moved => "moved",
        ChangeType::Renamed => "renamed",
        ChangeType::Reordered => "reordered",
    }
}

fn loc(e: &EntityInfo) -> String {
    format!("{}:{}", e.file_path, e.start_line)
}

fn ent_label(e: &EntityInfo) -> String {
    format!("{} `{}` ({})", e.entity_type, e.name, loc(e))
}

/// Incoming edges (from, kind) for every entity id, one pass over the edges.
fn incoming(g: &EntityGraph) -> HashMap<&str, Vec<(&str, &RefType)>> {
    let mut m: HashMap<&str, Vec<(&str, &RefType)>> = HashMap::new();
    for e in &g.edges {
        if e.from_entity.as_str() != e.to_entity.as_str() {
            m.entry(e.to_entity.as_str()).or_default().push((e.from_entity.as_str(), &e.ref_type));
        }
    }
    m
}

fn outgoing(g: &EntityGraph) -> HashMap<&str, Vec<&str>> {
    let mut m: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &g.edges {
        if e.from_entity.as_str() != e.to_entity.as_str() && !matches!(e.ref_type, RefType::TypeRef | RefType::Imports) {
            m.entry(e.from_entity.as_str()).or_default().push(e.to_entity.as_str());
        }
    }
    m
}

/// Is `id` (or any ancestor via parent_id) in `set`?
fn within(g: &EntityGraph, id: &str, set: &HashSet<String>) -> bool {
    let mut cur = Some(id.to_string());
    let mut guard = 0;
    while let Some(c) = cur {
        if set.contains(&c) {
            return true;
        }
        guard += 1;
        if guard > 16 {
            break;
        }
        cur = g.entities.get(c.as_str()).and_then(|e| e.parent_id.as_ref().map(|p| p.as_str().to_string()));
    }
    false
}

/// Production callers before test callers, then by location; duplicates
/// (several edges from one caller) collapse to one.
fn sort_callers(v: &mut Vec<Value>) {
    v.sort_by(|a, b| {
        (a["test"] == true, a["file"].as_str(), a["line"].as_u64()).cmp(&(b["test"] == true, b["file"].as_str(), b["line"].as_u64()))
    });
    v.dedup_by(|a, b| a["file"] == b["file"] && a["line"] == b["line"] && a["entity"] == b["entity"]);
}

/// Violation identity across trees: the violation with line/col dropped.
fn violation_key(law: &str, d: &Value) -> String {
    let mut d = d.clone();
    if let Some(o) = d.as_object_mut() {
        o.remove("line");
        o.remove("col");
    }
    format!("{law}\u{0}{d}")
}

/// `<base>..<head>` / `<base>...<head>` (merge base) / `<ref>` (`..HEAD`)
/// as two commit shas.
pub(crate) fn resolve_range(root: &Path, range: &str) -> Result<(String, String), Box<dyn std::error::Error>> {
    let (base_ref, head_ref) = match range.split_once("...") {
        Some((a, b)) => {
            let b = if b.is_empty() { "HEAD" } else { b };
            (git(&root, &["merge-base", a, b])?, b.to_string())
        }
        None => match range.split_once("..") {
            Some((a, b)) => (a.to_string(), if b.is_empty() { "HEAD".to_string() } else { b.to_string() }),
            None => (range.to_string(), "HEAD".to_string()),
        },
    };
    let base = git(root, &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")])?;
    let head = git(root, &["rev-parse", "--verify", &format!("{head_ref}^{{commit}}")])?;
    Ok((base, head))
}

/// Both trees of a range, whole.
pub(crate) fn build_trees(root: &Path, base: &str, head: &str) -> Result<(Tree, Tree), Box<dyn std::error::Error>> {
    let (bt, ht, _) = build_trees_in(root, base, head, Scope::Full)?;
    Ok((bt, ht))
}

pub fn certify_command(opts: CertifyOptions) -> Result<(), Box<dyn std::error::Error>> {
    let root = super::repo_root_or_cwd(&opts.cwd);
    let (base, head) = resolve_range(&root, &opts.range)?;
    let (bt, ht) = build_trees(&root, &base, &head)?;
    let cert = certificate(&root, &base, &head, &bt, &ht, &opts.laws)?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&cert)?);
    } else {
        print!("{}", render(&cert, opts.max_items, opts.max_chars));
    }
    Ok(())
}

/// The certificate for `base..head` over two built trees.
pub(crate) fn certificate(root: &Path, base: &str, head: &str, bt: &Tree, ht: &Tree, laws: &[PathBuf]) -> Result<Value, Box<dyn std::error::Error>> {
    let (base, head) = (base.to_string(), head.to_string());

    // -- the semantic diff ----------------------------------------------------
    let bridge = GitBridge::open(root)?;
    let file_changes = bridge.get_changed_files(&DiffScope::Range { from: base.clone(), to: head.clone() }, &[])?;
    let registry = super::create_registry(&root.to_string_lossy());
    let diff = compute_semantic_diff(&file_changes, &registry, None, None);
    let changed_files: BTreeSet<String> = file_changes.iter().map(|f| f.file_path.clone()).collect();

    let (bg, hg) = (&bt.graph, &ht.graph);
    let (b_in, h_in) = (incoming(bg), incoming(hg));
    let (b_out, h_out) = (outgoing(bg), outgoing(hg));

    // Entities this change touches, by id, in each tree. An entity nested in a
    // touched entity counts as touched.
    let mut touched_head: HashSet<String> = HashSet::new();
    let mut touched_base: HashSet<String> = HashSet::new();
    for c in &diff.changes {
        if hg.entities.contains_key(c.entity_id.as_str()) {
            touched_head.insert(c.entity_id.clone());
        }
        if bg.entities.contains_key(c.entity_id.as_str()) {
            touched_base.insert(c.entity_id.clone());
        }
    }
    // An added file is touched in full.
    for e in hg.entities.values() {
        if changed_files.contains(&e.file_path) && !bg.entities.contains_key(e.id.as_str()) {
            touched_head.insert(e.id.as_str().to_string());
        }
    }
    let head_tests: HashSet<&str> = HashSet::new();
    let is_test_file = |p: &str| {
        let l = p.to_ascii_lowercase();
        l.contains("/test/") || l.contains("/tests/") || l.contains("__tests__") || l.starts_with("test/") || l.starts_with("tests/")
            || l.contains(".test.") || l.contains(".spec.") || l.rsplit('/').next().is_some_and(|f| f.starts_with("test_") || f.ends_with("_test.py") || f.ends_with("_test.go"))
    };

    // -- per-entity facts -----------------------------------------------------
    let mut entities_out: Vec<Value> = Vec::new();
    let mut signature_changes: Vec<Value> = Vec::new();
    let mut untouched_callers: Vec<Value> = Vec::new();
    let mut callee_deltas: Vec<Value> = Vec::new();
    let caller_entry = |g: &EntityGraph, from: &str, kind: &RefType, touched: &HashSet<String>| -> Option<Value> {
        let e = g.entities.get(from)?;
        Some(json!({ "entity": e.name, "type": e.entity_type, "file": e.file_path, "line": e.start_line,
            "kind": kind.as_str(), "touchedByChange": within(g, from, touched), "test": head_tests.contains(from) || is_test_file(&e.file_path) }))
    };
    let mut changes: Vec<&SemanticChange> = diff.changes.iter().collect();
    changes.sort_by(|a, b| (a.file_path.as_str(), a.start_line).cmp(&(b.file_path.as_str(), b.start_line)));
    for c in &changes {
        let id = c.entity_id.as_str();
        let before_sig = c.before_content.as_deref().and_then(|b| contract(&c.entity_type, b));
        let after_sig = c.after_content.as_deref().and_then(|a| contract(&c.entity_type, a));
        entities_out.push(json!({ "change": kind_str(&c.change_type), "type": c.entity_type, "name": c.entity_name,
            "file": c.file_path, "lines": [c.start_line, c.end_line], "oldName": c.old_entity_name, "oldFile": c.old_file_path }));
        let in_head = hg.entities.contains_key(id);
        let in_base = bg.entities.contains_key(id);

        // Signature changes (modified in place, or renamed: old callers matter).
        if matches!(c.change_type, ChangeType::Modified | ChangeType::Renamed | ChangeType::Moved) {
            if let (Some(bs), Some(as_)) = (&before_sig, &after_sig) {
                if bs != as_ {
                    let mut callers: Vec<Value> = if in_head {
                        h_in.get(id).into_iter().flatten().filter_map(|(f, k)| caller_entry(hg, f, k, &touched_head)).collect()
                    } else {
                        Vec::new()
                    };
                    sort_callers(&mut callers);
                    // Callers at base of the old identity that still exist at head untouched.
                    let stale: Vec<Value> = if !in_head && in_base {
                        b_in.get(id).into_iter().flatten()
                            .filter(|(f, _)| hg.entities.contains_key(*f) && !within(hg, f, &touched_head))
                            .filter_map(|(f, k)| caller_entry(hg, f, k, &touched_head))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    signature_changes.push(json!({ "entity": c.entity_name, "type": c.entity_type, "file": c.file_path,
                        "line": c.start_line, "before": bs, "after": as_, "callers": callers, "staleCallersOfOldName": stale }));
                }
            }
        }

        // Untouched callers of modified / deleted entities.
        let (graph, inc, touched) = if in_head { (hg, &h_in, &touched_head) } else { (bg, &b_in, &touched_base) };
        if matches!(c.change_type, ChangeType::Modified | ChangeType::Deleted | ChangeType::Renamed | ChangeType::Moved) {
            let mut list: Vec<Value> = Vec::new();
            let mut seen = HashSet::new();
            for (f, k) in inc.get(id).into_iter().flatten() {
                if within(graph, f, touched) || !seen.insert(*f) {
                    continue;
                }
                // For a deleted entity, only callers that survive at head matter.
                if !in_head && !hg.entities.contains_key(*f) {
                    continue;
                }
                if let Some(v) = caller_entry(graph, f, k, touched) {
                    list.push(v);
                }
            }
            sort_callers(&mut list);
            if !list.is_empty() {
                untouched_callers.push(json!({ "entity": c.entity_name, "type": c.entity_type, "file": c.file_path,
                    "line": c.start_line, "change": kind_str(&c.change_type), "stillExistsAtHead": in_head, "callers": list }));
            }
        }

        // Callee delta of a modified entity (calls/refs/dispatch, by target identity).
        if in_head && in_base && matches!(c.change_type, ChangeType::Modified) {
            let names = |g: &EntityGraph, ids: Option<&Vec<&str>>| -> BTreeMap<String, String> {
                ids.into_iter().flatten().filter_map(|t| g.entities.get(*t).map(|e| (t.to_string(), ent_label(e)))).collect()
            };
            let before = names(bg, b_out.get(id));
            let after = names(hg, h_out.get(id));
            let removed: Vec<&String> = before.iter().filter(|(k, _)| !after.contains_key(*k)).map(|(_, v)| v).collect();
            let added: Vec<&String> = after.iter().filter(|(k, _)| !before.contains_key(*k)).map(|(_, v)| v).collect();
            if !removed.is_empty() || !added.is_empty() {
                callee_deltas.push(json!({ "entity": c.entity_name, "file": c.file_path, "line": c.start_line,
                    "noLongerReferences": removed, "nowReferences": added }));
            }
        }
    }

    // -- completeness of the caller sets this change relies on ----------------
    // The static graph's callers are the ones the resolver pinned. A
    // signature change or a deletion is only safe if those are all of them;
    // say for each such entity (and each modified one) whether they are, from
    // the head tree's source text (commands::completeness).
    let corpus = HeadCorpus::load(ht);
    let mut possible_outside: Vec<Value> = Vec::new();
    let mut assessed = 0usize;
    let mut not_assessed = 0usize;
    let mut sig_index: HashMap<(String, String, usize), usize> = HashMap::new();
    for (i, g) in signature_changes.iter().enumerate() {
        sig_index.insert((s(&g["entity"]), s(&g["file"]), g["line"].as_u64().unwrap_or(0) as usize), i);
    }
    for c in &changes {
        if !matches!(c.change_type, ChangeType::Modified | ChangeType::Deleted | ChangeType::Renamed | ChangeType::Moved) {
            continue;
        }
        if !matches!(c.entity_type.as_str(), "function" | "method" | "class" | "struct" | "constructor" | "property" | "getter" | "setter" | "variable" | "constant" | "interface" | "trait" | "enum" | "type") {
            continue;
        }
        let key = (c.entity_name.clone(), c.file_path.clone(), c.start_line);
        let is_sig = sig_index.contains_key(&key);
        if assessed >= MAX_ASSESSED && !is_sig {
            not_assessed += 1;
            continue;
        }
        assessed += 1;
        let id = c.entity_id.as_str();
        let in_head = hg.entities.contains_key(id);
        // the old name is what stale callers still say
        let name = if matches!(c.change_type, ChangeType::Renamed) { c.old_entity_name.clone().unwrap_or_else(|| c.entity_name.clone()) } else { c.entity_name.clone() };
        let resolved: Vec<(String, usize, usize)> = if in_head {
            h_in.get(id).into_iter().flatten().filter_map(|(f, _)| hg.entities.get(*f)).map(|e| (e.file_path.clone(), e.start_line, e.end_line)).collect()
        } else {
            Vec::new()
        };
        let (def_file, span) = match hg.entities.get(id) {
            Some(e) => (e.file_path.clone(), (e.start_line, e.end_line)),
            None => (c.file_path.clone(), (0, 0)),
        };
        let v = corpus.verdict(&name, &def_file, span, &resolved, if in_head { Some(&c.after_content) } else { None });
        let touched = |file: &str, line: usize| -> bool {
            hg.entities.values().any(|e| e.file_path == file && e.start_line <= line && line <= e.end_line && within(hg, e.id.as_str(), &touched_head))
        };
        let outside: Vec<Value> = v
            .possible_callers
            .iter()
            .filter(|p| !p.sites.iter().all(|st| touched(&p.file, st.line)))
            .map(|p| json!({ "entity": p.entity, "type": p.entity_type, "file": p.file, "line": p.start_line,
                "sites": p.sites.iter().map(|st| json!({"line": st.line, "kind": st.kind, "via": st.via})).collect::<Vec<_>>() }))
            .collect();
        if let Some(&i) = sig_index.get(&key) {
            let g = &mut signature_changes[i];
            g["callersComplete"] = json!(v.complete);
            g["incompleteBecause"] = json!(v.incomplete_because);
            g["possibleCallersNotModified"] = json!(outside);
        }
        if !v.complete {
            possible_outside.push(json!({ "entity": c.entity_name, "type": c.entity_type, "file": c.file_path, "line": c.start_line,
                "change": kind_str(&c.change_type), "incompleteBecause": v.incomplete_because, "possibleCallersNotModified": outside }));
        }
    }

    // -- static reference cone (head) -----------------------------------------
    let mut cone: HashSet<&str> = HashSet::new();
    let mut q: VecDeque<&str> = VecDeque::new();
    for id in &touched_head {
        q.push_back(id.as_str());
    }
    for c in &diff.changes {
        if !hg.entities.contains_key(c.entity_id.as_str()) {
            // deleted: its surviving base dependents seed the head cone
            for (f, _) in b_in.get(c.entity_id.as_str()).into_iter().flatten() {
                if let Some((k, _)) = hg.entities.get_key_value(*f) {
                    q.push_back(k.as_str());
                }
            }
        }
    }
    while let Some(u) = q.pop_front() {
        if cone.len() > 200_000 || !cone.insert(u) {
            continue;
        }
        for (f, _) in h_in.get(u).into_iter().flatten() {
            if !cone.contains(f) {
                q.push_back(f);
            }
        }
    }
    let cone_files: BTreeSet<&str> = cone.iter().filter_map(|id| hg.entities.get(*id).map(|e| e.file_path.as_str())).collect();
    let dependent_files: Vec<&str> = cone_files.iter().copied().filter(|f| !changed_files.contains(*f)).collect();
    let affected_tests: BTreeSet<String> = cone
        .iter()
        .filter_map(|id| hg.entities.get(*id).filter(|e| head_tests.contains(*id) || is_test_file(&e.file_path)).map(|e| e.file_path.clone()))
        .collect();
    let total_files = ht.all_files.len();

    // -- laws: base vs head ---------------------------------------------------
    let mut law_specs: Vec<Value> = Vec::new();
    let mut law_sources: Vec<String> = Vec::new();
    let mut add_file = |p: &Path, label: String| -> Result<(), Box<dyn std::error::Error>> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(p)?)?;
        for (i, l) in v["laws"].as_array().cloned().unwrap_or_default().into_iter().enumerate() {
            let mut l = l;
            if l.get("id").is_none() {
                l["id"] = json!(format!("{label}#{i}"));
            }
            law_specs.push(l);
        }
        law_sources.push(label);
        Ok(())
    };
    for p in super::promises::discover(ht.dir.path()) {
        let label = p.strip_prefix(ht.dir.path()).unwrap_or(&p).to_string_lossy().to_string();
        add_file(&p, label)?;
    }
    for p in laws {
        add_file(p, p.to_string_lossy().to_string())?;
    }
    let mut laws_out: Vec<Value> = Vec::new();
    if !law_specs.is_empty() {
        let bctx = Ctx::new(Common::at(&bt.dir.path().to_string_lossy()));
        let hctx = Ctx::new(Common::at(&ht.dir.path().to_string_lossy()));
        let br = check(&bctx, &law_specs, None)?;
        let hr = check(&hctx, &law_specs, None)?;
        for (b, h) in br.iter().zip(hr.iter()) {
            let id = h["id"].as_str().unwrap_or("").to_string();
            let bkeys: HashSet<String> = b["details"].as_array().into_iter().flatten().map(|d| violation_key(&id, d)).collect();
            let hkeys: HashSet<String> = h["details"].as_array().into_iter().flatten().map(|d| violation_key(&id, d)).collect();
            let new: Vec<Value> = h["details"].as_array().into_iter().flatten().filter(|d| !bkeys.contains(&violation_key(&id, d))).cloned().collect();
            let fixed = bkeys.difference(&hkeys).count();
            let status = if !new.is_empty() { "newly-broken" } else if h["kept"] == true { "kept" } else { "broken-before-and-after" };
            laws_out.push(json!({ "id": id, "promise": h["promise"], "status": status, "newViolations": new,
                "preexistingViolations": bkeys.len(), "fixedViolations": fixed }));
        }
    }

    // -- JS/TS module reachability delta --------------------------------------
    let (bn, badj) = super::topology::module_value_graph(&bt.dir.path().to_string_lossy(), bt.scope.as_ref());
    let (hn, hadj) = super::topology::module_value_graph(&ht.dir.path().to_string_lossy(), ht.scope.as_ref());
    let mut module_out = Value::Null;
    if !hn.is_empty() {
        let edges = |n: &Vec<String>, adj: &sem_core::topology::algo::Adj| -> BTreeSet<(String, String)> {
            adj.iter().enumerate().flat_map(|(u, vs)| vs.iter().map(move |&v| (n[u].clone(), n[v].clone()))).collect()
        };
        let (be, he) = (edges(&bn, &badj), edges(&hn, &hadj));
        let added: Vec<Value> = he.difference(&be).map(|(a, b)| json!({ "from": a, "to": b })).collect();
        let removed: Vec<Value> = be.difference(&he).map(|(a, b)| json!({ "from": a, "to": b })).collect();
        let bidx: HashMap<&str, usize> = bn.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
        // Reachability growth, per module that exists in both trees.
        let mut grown: Vec<(String, Vec<String>)> = Vec::new();
        let mut target_count: BTreeMap<String, usize> = BTreeMap::new();
        if !added.is_empty() {
            for (u, id) in hn.iter().enumerate() {
                let Some(&bu) = bidx.get(id.as_str()) else { continue };
                let hr: BTreeSet<&str> = sem_core::topology::algo::reach(&hadj, u).into_iter().map(|v| hn[v].as_str()).collect();
                let br: BTreeSet<&str> = sem_core::topology::algo::reach(&badj, bu).into_iter().map(|v| bn[v].as_str()).collect();
                let new: Vec<String> = hr.difference(&br).map(|s| s.to_string()).collect();
                if !new.is_empty() {
                    for t in &new {
                        *target_count.entry(t.clone()).or_default() += 1;
                    }
                    grown.push((id.clone(), new));
                }
            }
        }
        let hidx: HashMap<&str, usize> = hn.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
        let changed_modules: Vec<Value> = changed_files
            .iter()
            .filter_map(|f| hidx.get(f.as_str()).map(|&u| (f, u)))
            .map(|(f, u)| {
                let new: Vec<&str> = grown.iter().find(|(id, _)| id == f).map(|(_, n)| n.iter().map(String::as_str).collect()).unwrap_or_default();
                json!({ "module": f, "reachesAtHead": sem_core::topology::algo::reach(&hadj, u).len(),
                    "reachesAtBase": bidx.get(f.as_str()).map(|&b| sem_core::topology::algo::reach(&badj, b).len()),
                    "newlyReached": new })
            })
            .collect();
        let mut top: Vec<(String, usize)> = target_count.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        // Witness path for the most widely newly-reached targets.
        let witnesses: Vec<Value> = top.iter().take(10).map(|(t, n)| {
            let (src, _) = grown.iter().filter(|(_, new)| new.contains(t)).min_by_key(|(s, _)| s.len()).cloned().unwrap_or_default();
            let path = match (hidx.get(src.as_str()), hidx.get(t.as_str())) {
                (Some(&s), Some(&d)) => sem_core::topology::algo::shortest_path(&hadj, s, d).unwrap_or_default().into_iter().map(|v| hn[v].clone()).collect(),
                _ => Vec::new(),
            };
            json!({ "target": t, "modulesNewlyReaching": n, "examplePath": path })
        }).collect();
        module_out = json!({ "modules": hn.len(), "edgesAdded": added, "edgesRemoved": removed,
            "modulesWithGrownReach": grown.len(), "newlyReachedTargets": witnesses, "changedModules": changed_modules });
    }

    let cert = json!({
        "base": base, "head": head,
        "summary": { "filesChanged": changed_files.len(), "entitiesChanged": diff.changes.len(),
            "added": diff.added_count, "modified": diff.modified_count, "deleted": diff.deleted_count,
            "renamed": diff.renamed_count, "moved": diff.moved_count },
        "entities": entities_out,
        "signatureChanges": signature_changes,
        "untouchedCallers": untouched_callers,
        "callerSetsIncomplete": possible_outside,
        "callerSetsAssessed": assessed,
        "callerSetsNotAssessed": not_assessed,
        "calleeDeltas": callee_deltas,
        "laws": { "sources": law_sources, "results": laws_out },
        "moduleReachability": module_out,
        "cone": { "sourceFiles": total_files, "filesWithStaticDependents": dependent_files.len(),
            "dependentFiles": dependent_files, "filesOutsideCone": total_files.saturating_sub(cone_files.len().max(changed_files.len())) },
        "affectedTests": affected_tests,
        "noTestReaches": affected_tests.is_empty(),
        "limits": "static reference graph only: calls the resolver cannot pin (dynamic dispatch, reflection, string-keyed lookup, callbacks registered at runtime) and non-code consumers are not modeled",
    });
    Ok(cert)
}

fn s(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

fn possible_line(x: &Value) -> String {
    let sites: Vec<String> = x["sites"].as_array().cloned().unwrap_or_default().iter().take(4).map(|st| {
        format!("L{} {}{}", st["line"], s(&st["kind"]), st["via"].as_str().map(|v| format!(" via `{v}`")).unwrap_or_default())
    }).collect();
    format!("{}:{} `{}` ({})", s(&x["file"]), x["line"], s(&x["entity"]), sites.join(", "))
}

fn caller_line(c: &Value) -> String {
    format!("{}:{} `{}`{}{}", s(&c["file"]), c["line"], s(&c["entity"]),
        if c["kind"] == "calls" { String::new() } else { format!(" [{}]", s(&c["kind"])) },
        if c["test"] == true { " (test)" } else { "" })
}

/// The compact markdown a reviewer reads. Lists are capped at `max` items
/// with an explicit "+N more", and the whole render at `max_chars`.
pub fn render(c: &Value, max: usize, max_chars: usize) -> String {
    let mut o = String::new();
    let arr = |v: &Value| v.as_array().cloned().unwrap_or_default();
    let more = |n: usize| if n > max { format!(" … +{} more", n - max) } else { String::new() };
    let sm = &c["summary"];
    o += &format!("# sem certificate {}..{}\n", &s(&c["base"])[..12.min(s(&c["base"]).len())], &s(&c["head"])[..12.min(s(&c["head"]).len())]);
    o += &format!("{} files, {} entities changed ({} added, {} modified, {} deleted, {} renamed, {} moved).\n",
        sm["filesChanged"], sm["entitiesChanged"], sm["added"], sm["modified"], sm["deleted"], sm["renamed"], sm["moved"]);
    o += &format!("Limits: {}.\n", s(&c["limits"]));

    o += "\n## Entities changed\n";
    // Module-level fragments ("orphan" entities) are counted, not listed.
    let all = arr(&c["entities"]);
    let ents: Vec<Value> = all.iter().filter(|e| e["type"] != "orphan" && e["type"] != "chunk").cloned().collect();
    if all.len() > ents.len() {
        o += &format!("({} module-level fragment(s) or text chunk(s) also changed, not listed)\n", all.len() - ents.len());
    }
    for e in ents.iter().take(max * 2) {
        o += &format!("- {} {} `{}` {}:{}-{}\n", s(&e["change"]), s(&e["type"]), s(&e["name"]), s(&e["file"]), e["lines"][0], e["lines"][1]);
    }
    if ents.len() > max * 2 {
        o += &format!("- … +{} more\n", ents.len() - max * 2);
    }

    o += "\n## Signature changes\n";
    let sigs = arr(&c["signatureChanges"]);
    if sigs.is_empty() {
        o += "none\n";
    }
    for g in &sigs {
        let callers = arr(&g["callers"]);
        let stale = arr(&g["staleCallersOfOldName"]);
        let not_updated: Vec<&Value> = callers.iter().filter(|x| x["touchedByChange"] != true).collect();
        o += &format!("- `{}` ({}:{})\n  before: `{}`\n  after:  `{}`\n  static callers at head: {} ({} modified in this change, {} NOT modified by this change)\n",
            s(&g["entity"]), s(&g["file"]), g["line"], s(&g["before"]), s(&g["after"]), callers.len(), callers.len() - not_updated.len(), not_updated.len());
        for x in not_updated.iter().take(max) {
            o += &format!("    - not modified by this change: {}\n", caller_line(x));
        }
        if not_updated.len() > max {
            o += &format!("    - … +{} more\n", not_updated.len() - max);
        }
        for x in stale.iter().take(max) {
            o += &format!("    - still refers to old name: {}\n", caller_line(x));
        }
        if g["callersComplete"] == false {
            let pc = arr(&g["possibleCallersNotModified"]);
            o += &format!("  INCOMPLETE caller set: {}\n", arr(&g["incompleteBecause"]).iter().map(|r| s(&r["detail"])).collect::<Vec<_>>().join("; "));
            o += &format!("  possible callers the static graph did not resolve, NOT modified by this change: {}\n", pc.len());
            for x in pc.iter().take(max) {
                o += &format!("    - {}\n", possible_line(x));
            }
            if pc.len() > max {
                o += &format!("    - … +{} more\n", pc.len() - max);
            }
        }
    }

    o += "\n## Callers outside this change (of modified/deleted entities)\n";
    let uc = arr(&c["untouchedCallers"]);
    let inc = arr(&c["callerSetsIncomplete"]);
    if uc.is_empty() && inc.is_empty() {
        o += &format!("none found, and the caller sets of the {} changed entities assessed are complete (every textual mention is a resolved caller)\n", c["callerSetsAssessed"]);
    } else if uc.is_empty() {
        o += "none resolved by the static graph, but that is NOT a proof: see the incomplete caller sets (signature changes above, the rest below)\n";
    }
    for (i, u) in uc.iter().enumerate() {
        if i >= max * 2 {
            o += &format!("- … +{} more entities\n", uc.len() - i);
            break;
        }
        let callers = arr(&u["callers"]);
        let files: BTreeSet<String> = callers.iter().map(|x| s(&x["file"])).collect();
        o += &format!("- {} `{}` ({}:{}): {} static caller(s) in {} file(s) that this change does not modify{}\n", s(&u["change"]), s(&u["entity"]), s(&u["file"]), u["line"],
            callers.len(), files.len(), if u["stillExistsAtHead"] == false { " — entity no longer exists at head" } else { "" });
        for x in callers.iter().take(max) {
            o += &format!("    - {}\n", caller_line(x));
        }
        if callers.len() > max {
            o += &format!("    - … +{} more\n", callers.len() - max);
        }
    }

    // signature changes already listed theirs above
    let in_sigs = |u: &Value| sigs.iter().any(|g| g["entity"] == u["entity"] && g["file"] == u["file"] && g["line"] == u["line"]);
    let inc_rest: Vec<&Value> = inc.iter().filter(|u| !in_sigs(u)).collect();
    if !inc_rest.is_empty() {
        o += &format!("\n## Incomplete caller sets ({} more entit{}): possible callers the static graph could not resolve\n", inc_rest.len(), if inc_rest.len() == 1 { "y" } else { "ies" });
        for u in inc_rest.iter().take(max * 2) {
            let pc = arr(&u["possibleCallersNotModified"]);
            o += &format!("- {} `{}` ({}:{}): {}; {} possible caller(s) not modified by this change\n", s(&u["change"]), s(&u["entity"]), s(&u["file"]), u["line"],
                arr(&u["incompleteBecause"]).iter().map(|r| s(&r["detail"])).collect::<Vec<_>>().join("; "), pc.len());
            for x in pc.iter().take(max) {
                o += &format!("    - {}\n", possible_line(x));
            }
            if pc.len() > max {
                o += &format!("    - … +{} more\n", pc.len() - max);
            }
        }
        if inc_rest.len() > max * 2 {
            o += &format!("- … +{} more entities\n", inc_rest.len() - max * 2);
        }
    }
    if c["callerSetsNotAssessed"].as_u64().unwrap_or(0) > 0 {
        o += &format!("({} further changed entities were not assessed for caller completeness)\n", c["callerSetsNotAssessed"]);
    }

    o += "\n## Calls removed/added inside modified entities\n";
    let cd = arr(&c["calleeDeltas"]);
    if cd.is_empty() {
        o += "none\n";
    }
    for d in cd.iter().take(max * 2) {
        o += &format!("- `{}` ({}:{})\n", s(&d["entity"]), s(&d["file"]), d["line"]);
        for r in arr(&d["noLongerReferences"]).iter().take(max) {
            o += &format!("    - no longer references {}\n", s(r));
        }
        for r in arr(&d["nowReferences"]).iter().take(max) {
            o += &format!("    - now references {}\n", s(r));
        }
    }

    o += "\n## Laws\n";
    let laws = arr(&c["laws"]["results"]);
    if laws.is_empty() {
        o += "no laws declared for this repository\n";
    }
    for l in &laws {
        let status = s(&l["status"]);
        o += &format!("- {} `{}`{} (pre-existing violations at base: {}, fixed: {})\n", status.to_uppercase(), s(&l["id"]),
            l["promise"].as_str().map(|p| format!(" — {p}")).unwrap_or_default(), l["preexistingViolations"], l["fixedViolations"]);
        // A graph law broken by one new edge can yield hundreds of (from, to)
        // violations; the shortest witness paths say the most, so list those.
        let mut nv = arr(&l["newViolations"]);
        nv.sort_by_key(|d| d["path"].as_array().map_or(0, |p| p.len()));
        if !nv.is_empty() {
            o += &format!("    {} new violation(s); shortest witnesses:\n", nv.len());
        }
        let shown = 3.min(max);
        for d in nv.iter().take(shown) {
            let w = match d["path"].as_array() {
                Some(p) => p.iter().map(s).collect::<Vec<_>>().join(" -> "),
                None => super::promises::detail_line(d),
            };
            o += &format!("    - witness: {w}\n");
        }
        if nv.len() > shown {
            o += &format!("    - … +{} more\n", nv.len() - shown);
        }
    }

    let m = &c["moduleReachability"];
    if !m.is_null() {
        o += "\n## Module graph (JS/TS runtime imports)\n";
        let ea = arr(&m["edgesAdded"]);
        let er = arr(&m["edgesRemoved"]);
        o += &format!("{} import edge(s) added, {} removed; {} of {} modules now transitively reach modules they did not reach before.\n",
            ea.len(), er.len(), m["modulesWithGrownReach"], m["modules"]);
        for e in ea.iter().take(max) {
            o += &format!("- added: {} -> {}\n", s(&e["from"]), s(&e["to"]));
        }
        for e in er.iter().take(max) {
            o += &format!("- removed: {} -> {}\n", s(&e["from"]), s(&e["to"]));
        }
        for t in arr(&m["newlyReachedTargets"]).iter().take(max) {
            let path: Vec<String> = arr(&t["examplePath"]).iter().map(s).collect();
            o += &format!("- newly reached: {} (by {} module(s)); e.g. {}\n", s(&t["target"]), t["modulesNewlyReaching"], path.join(" -> "));
        }
    }

    o += "\n## Static reference cone\n";
    let cone = &c["cone"];
    let dep = arr(&cone["dependentFiles"]);
    o += &format!("{} of {} source files hold a static (transitive) dependent of a changed entity outside the changed files; the other {} files have no static reference path to this change.\n",
        dep.len(), cone["sourceFiles"], cone["filesOutsideCone"]);
    let mut dirs: BTreeMap<String, usize> = BTreeMap::new();
    for f in &dep {
        let f = s(f);
        let d = f.rsplit_once('/').map(|x| x.0.to_string()).unwrap_or_default();
        *dirs.entry(d).or_default() += 1;
    }
    let mut dv: Vec<(String, usize)> = dirs.into_iter().collect();
    dv.sort_by(|a, b| b.1.cmp(&a.1));
    if !dv.is_empty() {
        o += &format!("Dependent files by directory: {}\n", dv.iter().take(max).map(|(d, n)| format!("{d}/ ({n})")).collect::<Vec<_>>().join(", "));
    }

    o += "\n## Tests that statically reach the change\n";
    let t = arr(&c["affectedTests"]);
    if t.is_empty() {
        o += "NO TEST REACHES THIS CHANGE: it is unverified. No test file has a static reference path to any changed entity";
        if !inc.is_empty() {
            o += " (the search followed resolved callers only; tests may reach it through the possible callers above)";
        }
        o += ".\n";
    } else {
        o += &format!("{} file(s): {}{}\n", t.len(), t.iter().take(max).map(s).collect::<Vec<_>>().join(", "), more(t.len()));
    }

    if o.chars().count() > max_chars {
        let cut: String = o.chars().take(max_chars).collect();
        let cut = cut.rsplit_once('\n').map(|x| x.0.to_string()).unwrap_or(cut);
        o = format!("{cut}\n… (certificate truncated at {max_chars} characters)\n");
    }
    o
}

/// Changed entities assessed for caller completeness beyond the signature
/// changes (which are always assessed).
const MAX_ASSESSED: usize = 60;

/// The head tree's source text, loaded once, for the completeness scans.
struct HeadCorpus<'a> {
    tree: &'a Tree,
    files: Vec<(String, String)>,
    dynamic: Vec<(String, String, usize)>,
    by_file: HashMap<&'a str, Vec<&'a EntityInfo>>,
}

impl<'a> HeadCorpus<'a> {
    fn load(t: &'a Tree) -> Self {
        let files: Vec<(String, String)> = t
            .all_files
            .iter()
            .filter_map(|f| std::fs::read_to_string(t.dir.path().join(f)).ok().map(|c| (f.clone(), c)))
            .collect();
        let dynamic = files
            .iter()
            .flat_map(|(f, c)| super::completeness::dynamic_prefixes(f, c).into_iter().map(move |(p, l)| (p, f.clone(), l)))
            .collect();
        let mut by_file: HashMap<&str, Vec<&EntityInfo>> = HashMap::new();
        for e in t.graph.entities.values() {
            by_file.entry(e.file_path.as_str()).or_default().push(e);
        }
        HeadCorpus { tree: t, files, dynamic, by_file }
    }

    fn scan(&self, name: &str, only: Option<&str>) -> Vec<super::completeness::Mention> {
        let finder = memchr::memmem::Finder::new(name.as_bytes());
        self.files
            .iter()
            .filter(|(f, _)| only.is_none_or(|o| o == f))
            .filter(|(_, c)| finder.find(c.as_bytes()).is_some())
            .flat_map(|(f, c)| super::completeness::scan_file(f, c, name))
            .collect()
    }

    fn verdict(&self, name: &str, file: &str, span: (usize, usize), resolved: &[(String, usize, usize)], content: Option<&Option<String>>) -> super::completeness::Verdict {
        use super::completeness as c;
        let mentions = self.scan(name, None);
        let alias = c::alias_hop(&mentions, name, |a, scope| self.scan(a, scope));
        let decorators = content
            .and_then(|c| c.as_deref())
            .map(|body| {
                let src = self.files.iter().find(|(f, _)| f == file).map(|(_, c)| c.as_str()).unwrap_or("");
                let mut d = c::decorators(src, span.0);
                d.extend(body.lines().take_while(|l| l.trim_start().starts_with('@')).map(|l| l.trim().to_string()));
                d.sort();
                d.dedup();
                d
            })
            .unwrap_or_default();
        let files_with = self.files.iter().filter(|(_, c)| c.contains(name)).count();
        let owner = self.tree.graph.entities.values()
            .find(|e| e.file_path == file && e.start_line == span.0 && e.name == name)
            .and_then(|e| e.parent_id.as_ref())
            .and_then(|p| self.tree.graph.entities.get(p.as_str()))
            .map(|p| p.name.clone());
        let target = c::Target { name, file, span, decorators, owner: owner.as_deref() };
        c::assess(&target, &mentions, &alias, resolved, &self.dynamic, files_with, None, |f, line| {
            let ents = self.by_file.get(f)?;
            let e = ents.iter().filter(|e| e.start_line <= line && line <= e.end_line).min_by_key(|e| e.end_line - e.start_line)?;
            Some(c::Enclosing {
                display: super::qualified::display_name(&super::qualified::graph_owners(&self.tree.graph, e), &e.name),
                entity_type: e.entity_type.clone(),
                start_line: e.start_line,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{contract, signature};

    #[test]
    fn a_value_declaration_compares_by_declared_type() {
        let grouped = "var (\n\tlogger = logging.DefaultLogger.WithField(\"k\", \"v\")\n)";
        let flat = "var logger = logging.DefaultLogger.WithField(\"k\", \"v\")";
        assert_eq!(contract("variable", grouped), contract("variable", flat));
        let commented = "var (\n\t// retina-mode: basic, advanced\n\tretinaMode = flag.String(\"retina-mode\", \"basic\", \"x\")\n)";
        assert_eq!(contract("variable", commented), contract("variable", "var retinaMode = flag.String(\"retina-mode\", \"basic\", \"x\")"));
        // a new initializer is not a new contract
        assert_eq!(contract("variable", "var errX = errors.New(\"a\")"), contract("variable", "var errX = errors.New(\"b\")"));
        // a new declared type is
        assert_ne!(contract("variable", "var n int = 1"), contract("variable", "var n int64 = 1"));
        assert_ne!(contract("variable", "export const n: number = 1"), contract("variable", "export const n: string = '1'"));
        // a value holding a function keeps its parameter list as the contract
        assert_eq!(contract("variable", "export const f = (a: A, b?: B) => {\n};").as_deref(), Some("export const f = (a: A, b?: B)"));
    }

    #[test]
    fn signature_of_python_def_with_decorator() {
        let c = "@staticmethod\ndef foo(a, b=1) -> int:\n    return a\n";
        assert_eq!(signature(c).as_deref(), Some("def foo(a, b=1) -> int"));
    }

    #[test]
    fn signature_of_multiline_ts_function() {
        let c = "export function foo(\n  a: number,\n  b: string,\n): number {\n  return a;\n}";
        assert_eq!(signature(c).as_deref(), Some("export function foo(a: number, b: string): number"));
    }

    #[test]
    fn signature_of_arrow() {
        let c = "export const foo = (a: A, b?: B) => {\n  return 1;\n};";
        assert_eq!(signature(c).as_deref(), Some("export const foo = (a: A, b?: B)"));
    }

    #[test]
    fn no_signature_for_class() {
        assert_eq!(signature("class Foo:\n    x = 1\n    def bar(self): pass"), None);
    }
}
