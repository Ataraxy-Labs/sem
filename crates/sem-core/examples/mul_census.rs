//! MUL-A (MUL phase A) violation census: measure, per language family and
//! per file, whether the property that licenses `PrecomputedFileFacts` actually
//! holds on real corpora.
//!
//! The license (`precompute_js_ts_file_facts`'s doc comment) is stated as
//! "declarations never nest across files", which is claimed
//! FALSE for C# (partial classes) and C++ (out-of-line member definitions). But
//! the property the *code* needs is narrower and exactly checkable: pass 2's
//! scope walk consults the corpus-wide `children_by_parent`/`entity_map` only
//! with ids it obtained from `FileEntityLookup` (this file's entities). So the
//! precise soundness predicate per file F is
//!
//!   CLEAN(F)  <=>  for every entity e declared in F,
//!                  { x : x.parent_id == e.id } is a subset of entities(F)
//!
//! i.e. no entity outside F may name an entity of F as its parent. This probe
//! measures exactly that, plus:
//!
//!   * the *structural* half of the property, per file: would this file's tree
//!     still be needed in pass 2 after the walk's outputs were precomputed?
//!     That is true iff it contains an import statement kind
//!     `classify_import_stmt` handles (Python/Rust/Go/Java/TS) or a Python-style
//!     `call` node (ctor-infer's `scan_constructor_calls`) or is `.swift`
//!     (`build_swift_call_signatures`).
//!   * the raw language constructs the property names, counted from source:
//!     C# `partial` type declarations, C++ out-of-line member definitions
//!     (`Type::member(...)` at declaration position).
//!   * source bytes per family, which is the memory arithmetic's input
//!     (`PrecomputedFileFacts` retains `content` for the whole build).
//!
//! Usage: mul_census <repo_root> [label]

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::plugins::code::languages::get_language_config;
use sem_core::parser::plugins::code::{is_pathological_large_file, parse_tree};
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};

fn walk_files(root: &Path, registry: &ParserRegistry) -> Vec<String> {
    let mut files = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    for entry in builder.build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if is_default_excluded(&rel) || is_probably_binary_path(&rel) {
            continue;
        }
        if registry.get_explicit_plugin(&rel).is_none() {
            continue;
        }
        files.push(rel);
    }
    files.sort();
    files
}

fn ext_of(path: &str) -> &str {
    path.rfind('.').map(|i| &path[i..]).unwrap_or("")
}

fn scope_resolvable(path: &str) -> bool {
    get_language_config(ext_of(path))
        .and_then(|c| c.scope_resolve)
        .is_some()
}

/// Family label used for the census tables. Grouped the way the fences are
/// argued, not by extension.
fn family_of(ext: &str) -> &'static str {
    match ext {
        ".ts" | ".tsx" | ".mts" | ".cts" | ".js" | ".jsx" | ".mjs" | ".cjs" | ".es6" => "JS/TS",
        ".py" | ".pyi" => "Python",
        ".cs" => "C#",
        ".cpp" | ".cc" | ".cxx" | ".hpp" | ".hh" | ".hxx" => "C++",
        ".c" | ".h" => "C (no scope_resolve)",
        ".go" => "Go",
        ".rs" => "Rust",
        ".java" => "Java",
        ".swift" => "Swift",
        ".rb" => "Ruby",
        ".kt" | ".kts" => "Kotlin",
        ".scala" | ".sc" | ".sbt" => "Scala",
        ".php" | ".inc" | ".phtml" | ".module" => "PHP",
        ".sh" => "Bash",
        ".dart" => "Dart",
        ".zig" => "Zig",
        _ => "other",
    }
}

/// `classify_import_stmt`'s handled set, replicated here (it is private). A
/// file whose tree contains one of these kinds still needs its tree in pass 2
/// for `replay_import_stmts_pruned`, because the handlers read corpus-wide
/// tables and therefore cannot run in pass 1.
fn is_handled_import_kind(kind: &str, self_keywords: &[&str]) -> bool {
    match kind {
        "import_from_statement" => true,
        "import_statement" => true, // Py (self+cls) or TS (!cls) — both handled
        "export_statement" => !self_keywords.contains(&"cls"),
        "use_declaration" => true,
        "import_declaration" => true,
        _ => false,
    }
}

#[derive(Default, Clone)]
struct FamilyStats {
    files: u64,
    bytes: u64,
    entities: u64,
    entities_with_parent: u64,
    /// entities whose parent_id names an entity produced by a *different* file
    cross_file_children: u64,
    /// files that own at least one entity whose child lives in another file
    dirty_parent_files: u64,
    /// files that contain at least one entity whose parent lives elsewhere
    dirty_child_files: u64,
    /// files whose tree is still needed in pass 2 after the walk is precomputed
    needs_tree_imports: u64,
    needs_tree_ctor: u64,
    needs_tree_swift: u64,
    needs_tree_any: u64,
    parse_failures: u64,
    /// language constructs the property names, counted from source
    cs_partial_types: u64,
    cs_files_with_partial: u64,
    cpp_out_of_line_defs: u64,
    cpp_files_with_out_of_line: u64,
    /// entity names containing `::` (what the C++ extractor produces for an
    /// out-of-line member definition, if anything)
    qualified_entity_names: u64,
}

struct FileFacts {
    path: String,
    ext: String,
    bytes: u64,
    entities: Vec<(String, Option<String>)>, // (id, parent_id)
    entity_count: u64,
    qualified_names: u64,
    has_handled_import: bool,
    has_py_call: bool,
    parse_failed: bool,
    cs_partial: u64,
    cpp_out_of_line: u64,
}

/// Count `partial class|struct|interface|record` declarations textually. Cheap
/// and good enough for a census: the token can only appear as a modifier.
fn count_cs_partial(content: &str) -> u64 {
    let mut n = 0u64;
    for line in content.lines() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        if let Some(i) = t.find("partial ") {
            let rest = &t[i + "partial ".len()..];
            let rest = rest.trim_start();
            if rest.starts_with("class ")
                || rest.starts_with("struct ")
                || rest.starts_with("interface ")
                || rest.starts_with("record ")
                || rest.starts_with("void ")
            {
                n += 1;
            }
        }
    }
    n
}

/// Count C++ out-of-line member definitions: a line whose first `(` is preceded
/// by an identifier chain containing `::` and which is not a call statement
/// (heuristic: line does not end in `;` before the brace and starts at column 0
/// or after a return type). Deliberately generous — an over-count is the
/// conservative direction for this census.
fn count_cpp_out_of_line(content: &str) -> u64 {
    let mut n = 0u64;
    for line in content.lines() {
        if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('#') {
            continue;
        }
        let Some(paren) = line.find('(') else {
            continue;
        };
        let head = &line[..paren];
        if !head.contains("::") {
            continue;
        }
        // `A::f(` where the `::` is in the *declarator*, not a return type like
        // `std::string f(`: require the last `::` to come after the last space.
        let last_sep = head.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
        if head[last_sep..].contains("::") {
            n += 1;
        }
    }
    n
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: mul_census <repo_root> [label]");
        std::process::exit(1);
    }
    let root = PathBuf::from(&args[1]).canonicalize().expect("bad root");
    let label = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| root.file_name().unwrap().to_string_lossy().to_string());

    let mut registry = create_default_registry();
    registry.load_semrc(&root);
    registry.load_gitattributes(&root);
    let files = walk_files(&root, &registry);

    let per_file: Vec<FileFacts> = files
        .par_iter()
        .filter_map(|rel| {
            let ext = ext_of(rel).to_string();
            let full = root.join(rel);
            let content = std::fs::read_to_string(&full).ok()?;
            let bytes = content.len() as u64;
            let entities: Vec<SemanticEntity> = registry.extract_entities(rel, &content);
            let qualified_names = entities.iter().filter(|e| e.name.contains("::")).count() as u64;
            let entity_count = entities.len() as u64;
            let compact: Vec<(String, Option<String>)> =
                entities.into_iter().map(|e| (e.id, e.parent_id)).collect();

            // Structural half: does pass 2 still need this file's tree?
            let mut has_handled_import = false;
            let mut has_py_call = false;
            let mut parse_failed = false;
            if let Some(cfg) = get_language_config(&ext).and_then(|c| c.scope_resolve) {
                let lang_cfg = get_language_config(&ext).expect("checked");
                if is_pathological_large_file(&content) {
                    parse_failed = true;
                } else if let Some(tree) = parse_tree(lang_cfg, &content) {
                    let mut worklist = vec![tree.root_node()];
                    while let Some(node) = worklist.pop() {
                        let mut cursor = node.walk();
                        for child in node.named_children(&mut cursor) {
                            let kind = child.kind();
                            if is_handled_import_kind(kind, cfg.self_keywords) {
                                has_handled_import = true;
                                // extract does not descend into a handled node
                                continue;
                            }
                            if kind == "call" {
                                has_py_call = true;
                            }
                            worklist.push(child);
                        }
                        if has_handled_import && has_py_call {
                            break;
                        }
                    }
                } else {
                    parse_failed = true;
                }
            }

            let cs_partial = if ext == ".cs" {
                count_cs_partial(&content)
            } else {
                0
            };
            let cpp_out_of_line = if matches!(
                ext.as_str(),
                ".cpp" | ".cc" | ".cxx" | ".hpp" | ".hh" | ".hxx"
            ) {
                count_cpp_out_of_line(&content)
            } else {
                0
            };

            Some(FileFacts {
                path: rel.clone(),
                ext,
                bytes,
                entities: compact,
                entity_count,
                qualified_names,
                has_handled_import,
                has_py_call,
                parse_failed,
                cs_partial,
                cpp_out_of_line,
            })
        })
        .collect();

    // Global id -> owning file index. Duplicate ids across files are recorded:
    // a duplicate is the one mechanism by which a "cross-file" parent link could
    // appear even though ids are file-rooted.
    let mut id_owner: HashMap<&str, usize> = HashMap::with_capacity(1 << 20);
    let mut dup_ids_cross_file: u64 = 0;
    let mut dup_ids_same_file: u64 = 0;
    for (i, f) in per_file.iter().enumerate() {
        for (id, _) in &f.entities {
            match id_owner.insert(id.as_str(), i) {
                None => {}
                Some(prev) => {
                    if prev == i {
                        dup_ids_same_file += 1;
                    } else {
                        dup_ids_cross_file += 1;
                    }
                }
            }
        }
    }

    let mut fam: BTreeMap<&'static str, FamilyStats> = BTreeMap::new();
    let mut cross_examples: Vec<(String, String, String)> = Vec::new();
    let mut dirty_parent_idx: HashSet<usize> = HashSet::new();

    // First pass: cross-file child links (child side), and which file owns the
    // parent (parent side = the file whose precompute would be unsound).
    let mut child_dirty: Vec<bool> = vec![false; per_file.len()];
    let mut cross_by_family: BTreeMap<&'static str, u64> = BTreeMap::new();
    for (i, f) in per_file.iter().enumerate() {
        for (id, parent) in &f.entities {
            let Some(pid) = parent else { continue };
            match id_owner.get(pid.as_str()) {
                Some(&owner) if owner != i => {
                    child_dirty[i] = true;
                    dirty_parent_idx.insert(owner);
                    *cross_by_family.entry(family_of(&f.ext)).or_default() += 1;
                    if cross_examples.len() < 25 {
                        cross_examples.push((
                            f.path.clone(),
                            id.clone(),
                            per_file[owner].path.clone(),
                        ));
                    }
                }
                Some(_) => {}
                None => {
                    // Dangling parent: parent id names no extracted entity at
                    // all. Not a cross-file nest (nothing to see), but recorded.
                    if cross_examples.len() < 25 {
                        cross_examples.push((f.path.clone(), id.clone(), "<dangling>".into()));
                    }
                }
            }
        }
    }

    for (i, f) in per_file.iter().enumerate() {
        let s = fam.entry(family_of(&f.ext)).or_default();
        s.files += 1;
        s.bytes += f.bytes;
        s.entities += f.entity_count;
        s.entities_with_parent += f.entities.iter().filter(|(_, p)| p.is_some()).count() as u64;
        s.qualified_entity_names += f.qualified_names;
        if f.parse_failed {
            s.parse_failures += 1;
        }
        if child_dirty[i] {
            s.dirty_child_files += 1;
        }
        if dirty_parent_idx.contains(&i) {
            s.dirty_parent_files += 1;
        }
        let swift = f.ext == ".swift";
        if f.has_handled_import {
            s.needs_tree_imports += 1;
        }
        if f.has_py_call {
            s.needs_tree_ctor += 1;
        }
        if swift {
            s.needs_tree_swift += 1;
        }
        if f.has_handled_import || f.has_py_call || swift {
            s.needs_tree_any += 1;
        }
        s.cs_partial_types += f.cs_partial;
        if f.cs_partial > 0 {
            s.cs_files_with_partial += 1;
        }
        s.cpp_out_of_line_defs += f.cpp_out_of_line;
        if f.cpp_out_of_line > 0 {
            s.cpp_files_with_out_of_line += 1;
        }
    }
    for (family, n) in &cross_by_family {
        if let Some(s) = fam.get_mut(family) {
            s.cross_file_children = *n;
        }
    }

    let total_files: u64 = fam.values().map(|s| s.files).sum();
    let total_entities: u64 = fam.values().map(|s| s.entities).sum();
    let sr_files = per_file
        .iter()
        .filter(|f| scope_resolvable(&f.path))
        .count();
    let sr_bytes: u64 = per_file
        .iter()
        .filter(|f| scope_resolvable(&f.path))
        .map(|f| f.bytes)
        .sum();
    let sr_non_jsts_files = per_file
        .iter()
        .filter(|f| scope_resolvable(&f.path) && family_of(&f.ext) != "JS/TS")
        .count();
    let sr_non_jsts_bytes: u64 = per_file
        .iter()
        .filter(|f| scope_resolvable(&f.path) && family_of(&f.ext) != "JS/TS")
        .map(|f| f.bytes)
        .sum();
    let sr_non_jsts_entities: u64 = per_file
        .iter()
        .filter(|f| scope_resolvable(&f.path) && family_of(&f.ext) != "JS/TS")
        .map(|f| f.entity_count)
        .sum();
    let clean_non_jsts = per_file
        .iter()
        .enumerate()
        .filter(|(i, f)| {
            scope_resolvable(&f.path)
                && family_of(&f.ext) != "JS/TS"
                && !dirty_parent_idx.contains(i)
        })
        .count();
    let clean_and_treeless = per_file
        .iter()
        .enumerate()
        .filter(|(i, f)| {
            scope_resolvable(&f.path)
                && family_of(&f.ext) != "JS/TS"
                && !dirty_parent_idx.contains(i)
                && !f.has_handled_import
                && !f.has_py_call
                && f.ext != ".swift"
        })
        .count();
    let clean_and_treeless_bytes: u64 = per_file
        .iter()
        .enumerate()
        .filter(|(i, f)| {
            scope_resolvable(&f.path)
                && family_of(&f.ext) != "JS/TS"
                && !dirty_parent_idx.contains(i)
                && !f.has_handled_import
                && !f.has_py_call
                && f.ext != ".swift"
        })
        .map(|(_, f)| f.bytes)
        .sum();

    println!("MUL_CENSUS label={label} root={}", root.display());
    println!(
        "MUL_CENSUS totals files={total_files} entities={total_entities} \
         scope_resolvable_files={sr_files} scope_resolvable_bytes={sr_bytes} \
         sr_non_jsts_files={sr_non_jsts_files} sr_non_jsts_bytes={sr_non_jsts_bytes} \
         sr_non_jsts_entities={sr_non_jsts_entities}"
    );
    println!(
        "MUL_CENSUS ids dup_cross_file={dup_ids_cross_file} dup_same_file={dup_ids_same_file}"
    );
    println!(
        "MUL_CENSUS gate sr_non_jsts_files={sr_non_jsts_files} \
         clean_semantics={clean_non_jsts} clean_and_treeless={clean_and_treeless} \
         clean_and_treeless_bytes={clean_and_treeless_bytes}"
    );
    println!(
        "{:<22} {:>8} {:>9} {:>11} {:>11} {:>9} {:>9} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "family",
        "files",
        "entities",
        "ent+parent",
        "xfile_child",
        "dirtyPar",
        "dirtyChd",
        "needTree",
        "imports",
        "pycall",
        "qualNm",
        "MB"
    );
    for (family, s) in &fam {
        println!(
            "{:<22} {:>8} {:>9} {:>11} {:>11} {:>9} {:>9} {:>8} {:>8} {:>8} {:>8} {:>8.1}",
            family,
            s.files,
            s.entities,
            s.entities_with_parent,
            s.cross_file_children,
            s.dirty_parent_files,
            s.dirty_child_files,
            s.needs_tree_any,
            s.needs_tree_imports,
            s.needs_tree_ctor,
            s.qualified_entity_names,
            s.bytes as f64 / (1024.0 * 1024.0)
        );
    }
    for (family, s) in &fam {
        if s.cs_partial_types > 0 || s.cpp_out_of_line_defs > 0 {
            println!(
                "MUL_CENSUS constructs family={family} cs_partial_decls={} cs_files_with_partial={} \
                 cpp_out_of_line_defs={} cpp_files_with_out_of_line={} files={}",
                s.cs_partial_types,
                s.cs_files_with_partial,
                s.cpp_out_of_line_defs,
                s.cpp_files_with_out_of_line,
                s.files
            );
        }
    }
    for (f, id, owner) in cross_examples.iter().take(25) {
        println!("MUL_CENSUS xfile_example child_file={f} child_id={id} parent_file={owner}");
    }
}
