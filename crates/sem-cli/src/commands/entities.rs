use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::timings::Timings;
use colored::Colorize;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::EntityInfo;
use sem_core::parser::registry::ParserRegistry;
use serde::Serialize;

pub struct EntitiesOptions {
    pub cwd: String,
    pub paths: Vec<String>,
    pub json: bool,
    pub no_default_excludes: bool,
    pub file_exts: Vec<String>,
    /// Keep only entities whose kind is in this list (empty = no filter).
    pub only_kinds: Vec<String>,
    /// Drop entities whose kind is in this list (empty = no filter).
    pub except_kinds: Vec<String>,
    /// Exact substring to search for inside entity bodies (entity-addressed
    /// grep replacement). When set, listing flags are ignored.
    pub text: Option<String>,
    /// Show each entity's header (signature plus first doc-comment line)
    /// under its row.
    pub signatures: bool,
}

pub fn entities_command(opts: EntitiesOptions) {
    if let Some(needle) = opts
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        text_search(&opts, needle);
        return;
    }

    let mut timings = Timings::from_env("entities");

    // --only and --except are mutually exclusive.
    if !opts.only_kinds.is_empty() && !opts.except_kinds.is_empty() {
        eprintln!(
            "{} --only and --except cannot be used together",
            "error:".red().bold()
        );
        std::process::exit(2);
    }
    let kind_filter_active = !opts.only_kinds.is_empty() || !opts.except_kinds.is_empty();

    // Normalize to a non-empty list of path args, defaulting to ".".
    let path_args: Vec<String> = {
        let cleaned: Vec<String> = opts
            .paths
            .iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if cleaned.is_empty() {
            vec![".".to_string()]
        } else {
            cleaned
        }
    };
    timings.counter("input_paths", path_args.len() as u64);
    timings.mark("path_args");

    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let root = Path::new(&opts.cwd);

    // Cloud is a capability — cross-repo xref, repos with no local index
    // — not a latency tier, so it must not race a
    // local index read over the network. It only gets a turn for the
    // whole-repo single listing when there is no local index yet to answer
    // from (first-ever run, or a scope the index was never built for);
    // once a local index exists, the directory loop below answers this
    // exact shape from it directly (item 1) and cloud is never reached.
    let no_local_index_for_default_scope = ext_filter.is_empty()
        && !opts.no_default_excludes
        && super::query::open_index(root).is_none();
    if ext_filter.is_empty()
        && path_args.len() == 1
        && !kind_filter_active
        && no_local_index_for_default_scope
        && super::cloud::try_cloud_entities(&opts).is_some()
    {
        timings.mark("cloud_entities");
        timings.finish();
        return;
    }

    let registry = super::create_registry(&opts.cwd);

    let mut entities: Vec<SemanticEntity> = Vec::new();
    let mut dir_count = 0usize;
    let mut file_arg_count = 0usize;
    let mut processed_file_count = 0usize;
    let mut discovered_file_count = 0usize;
    let mut extracted_entities = false;
    // The index-root lookup is one repo-scoped fact, not a per-file one —
    // computed at most once, lazily, on the first file-shaped `path_arg`
    // (never for a directory-only invocation, which doesn't need it: see
    // `try_index_entities_for_dir`, which reads through the raw `root`
    // instead). `SEM_NO_INDEX` is checked here, before any discovery work
    // runs, not downstream inside `open_index` — the outer `None` means
    // "index reroute disabled," the inner `None` means "not inside a repo."
    let mut index_repo_root: Option<Option<PathBuf>> = None;
    for path_arg in &path_args {
        let (path_label, full_path) = resolve_path(root, path_arg);
        if full_path.is_file() {
            file_arg_count += 1;
            processed_file_count += 1;
            let repo_root = index_repo_root.get_or_insert_with(|| {
                if std::env::var_os("SEM_NO_INDEX").is_some() {
                    None
                } else {
                    Some(super::repo_root_or_cwd(&opts.cwd))
                }
            });
            let file_entities = match repo_root
                .as_deref()
                .and_then(|repo_root| try_index_entities_for_file(&opts.cwd, repo_root, &full_path))
            {
                Some(indexed) => {
                    timings.mark("index_entities_query");
                    indexed
                }
                None => {
                    extracted_entities = true;
                    extract_file_entities(&full_path, &registry, &path_label).unwrap_or_else(|e| {
                        eprintln!(
                            "{} Cannot read '{}': {}",
                            "error:".red().bold(),
                            path_label,
                            e
                        );
                        std::process::exit(1);
                    })
                }
            };
            entities.extend(file_entities);
        } else if full_path.is_dir() {
            dir_count += 1;
            // Query-index reroute first (
            // unblocked this change by `QueryIndex::files_under`): skips the
            // filesystem walk entirely when the index can answer for this
            // exact scope. `None` — never a wrong answer, only "cannot
            // answer fast" — falls through to the unchanged walk below.
            match try_index_entities_for_dir(
                &opts.cwd,
                root,
                &full_path,
                &registry,
                &ext_filter,
                opts.no_default_excludes,
            ) {
                Some((file_count, indexed)) => {
                    timings.mark("index_entities_dir_query");
                    discovered_file_count += file_count;
                    processed_file_count += file_count;
                    entities.extend(indexed);
                }
                None => {
                    let file_paths = super::files::find_supported_files_in_path(
                        root,
                        &full_path,
                        &registry,
                        &ext_filter,
                        opts.no_default_excludes,
                    );
                    discovered_file_count += file_paths.len();
                    processed_file_count += file_paths.len();
                    timings.mark("file_discovery");
                    entities.extend(extract_files_entities(root, &file_paths, &registry));
                    extracted_entities = true;
                }
            }
        } else {
            eprintln!("{} Path not found '{}'", "error:".red().bold(), path_arg);
            std::process::exit(1);
        }
    }
    if extracted_entities {
        timings.mark("extract_entities");
    }

    // Overlapping paths (e.g. a directory and a file inside it) can surface the
    // same entity twice; sort and drop exact duplicates by id.
    entities.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.end_line.cmp(&b.end_line))
            .then(a.entity_type.cmp(&b.entity_type))
            .then(a.name.cmp(&b.name))
    });
    entities.dedup_by(|a, b| a.id == b.id);
    timings.mark("sort_dedup");

    // Apply --only / --except kind filters. Entity kinds are language-dependent,
    // so validate requested kinds against the kinds actually present in this
    // scan and, on a miss, show the user what kinds exist here.
    if kind_filter_active {
        entities = match filter_by_kind(entities, &opts.only_kinds, &opts.except_kinds) {
            Ok(filtered) => filtered,
            Err(msg) => {
                eprintln!("{} {msg}", "error:".red().bold());
                std::process::exit(2);
            }
        };
        timings.counter("kind_filtered", entities.len() as u64);
    }

    // Show the file column whenever results span more than one file. This keeps
    // the prior single-file vs directory behavior and covers multi-path input.
    let distinct_files = {
        let mut files: Vec<&str> = entities.iter().map(|e| e.file_path.as_str()).collect();
        files.sort_unstable();
        files.dedup();
        files.len()
    };
    let include_file = dir_count > 0 || distinct_files > 1;
    let display_label = path_args.join(" ");
    timings.counter("input_files", processed_file_count as u64);
    timings.counter("input_file_args", file_arg_count as u64);
    timings.counter("input_dirs", dir_count as u64);
    timings.counter("processed_files", processed_file_count as u64);
    timings.counter("discovered_files", discovered_file_count as u64);
    timings.counter("distinct_files", distinct_files as u64);
    timings.counter("entities", entities.len() as u64);

    // One header per entity id (`sem_core::parser::header`), computed only
    // under --signatures. `entity.file_path` here is relative to `opts.cwd`
    // (see `resolve_path`/`file_path_for_entity`), so `root` is the join
    // base. A file that can't be read just leaves its entities without a
    // header; the listing itself never fails over the zoom detail.
    let headers = opts.signatures.then(|| {
        sem_core::parser::header::headers_by_id(
            root,
            entities.iter().map(|e| {
                (
                    e.id.as_str(),
                    e.file_path.as_str(),
                    e.start_line,
                    e.end_line,
                )
            }),
        )
    });
    let headers = headers.as_ref();

    if opts.json {
        let json_bytes =
            write_entities_json(&entities, include_file, headers).unwrap_or_else(|e| {
                eprintln!("{} Cannot write JSON output: {}", "error:".red().bold(), e);
                std::process::exit(1);
            });
        timings.counter("json_bytes", json_bytes);
    } else if should_group_by_file(&entities) {
        print_grouped_entities(&display_label, &entities, headers);
    } else if let Some(file_path) = entities.first().map(|e| e.file_path.as_str()) {
        print_file_entities(file_path, &entities, headers);
    } else {
        println!("{} {}\n", "entities:".green().bold(), display_label.bold());
    }
    timings.mark("output_serialization");
    timings.finish();
}

/// Filter entities by the `--only` / `--except` kind lists. Entity kinds are
/// language-dependent, so a requested kind that matches nothing in `entities`
/// is an error whose message lists the kinds actually present. Assumes the
/// caller has already rejected the only+except combination.
fn filter_by_kind(
    mut entities: Vec<SemanticEntity>,
    only: &[String],
    except: &[String],
) -> Result<Vec<SemanticEntity>, String> {
    if only.is_empty() && except.is_empty() {
        return Ok(entities);
    }
    let present: BTreeSet<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
    let requested = if only.is_empty() { except } else { only };
    if let Some(unknown) = requested.iter().find(|k| !present.contains(k.as_str())) {
        let valid = present.into_iter().collect::<Vec<_>>().join(", ");
        return Err(format!(
            "unknown entity kind \"{unknown}\"\n\nkinds found here: {}",
            if valid.is_empty() {
                "(none)".to_string()
            } else {
                valid
            }
        ));
    }
    if !only.is_empty() {
        entities.retain(|e| only.iter().any(|k| k == &e.entity_type));
    } else {
        entities.retain(|e| !except.iter().any(|k| k == &e.entity_type));
    }
    Ok(entities)
}

fn resolve_path(root: &Path, path_arg: &str) -> (String, PathBuf) {
    let path = Path::new(path_arg);
    let full_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };

    let label = if path.is_absolute() {
        file_path_for_entity(root, &full_path)
    } else {
        path_arg.to_string()
    };

    (label, full_path)
}

fn extract_files_entities(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
) -> Vec<SemanticEntity> {
    registry.extract_all_entities_brief(root, file_paths)
}

/// Reroute of `sem entities <single file>` onto the query index (
/// item 2's "missing path" note: this call used to
/// consult no cache at all and always re-parse, 229ms on the monster). A
/// `None` here — index absent, file unknown to it, or Verified freshness
/// finding it stale — falls through to the unchanged `extract_file_entities`
/// re-parse below; this function never produces a wrong answer, only
/// "cannot answer fast."
fn try_index_entities_for_file(
    cwd: &str,
    repo_root: &Path,
    full_path: &Path,
) -> Option<Vec<SemanticEntity>> {
    let idx = super::query::open_index(repo_root)?;
    let rel = super::normalize_repo_relative_path(
        Path::new(cwd),
        repo_root,
        &full_path.to_string_lossy(),
    );
    if super::query::is_file_stale(&idx, repo_root, &rel) {
        return None;
    }
    // Confirms the index actually knows this file, distinguishing "no
    // entities in this file" (valid, e.g. an empty file) from "this file is
    // outside the indexed scope" (must fall through, not report zero rows).
    idx.file_fingerprint(&rel)?;
    Some(
        idx.entities_in_file(&rel)
            .iter()
            .map(|e| entity_info_to_entity(e.to_entity_info()))
            .collect(),
    )
}

/// Reroute of `sem entities <dir>`'s file discovery onto the query index
/// (unblocked this change by `QueryIndex::files_under`
/// — inherited blocker: "the reader *could* answer it... wiring correct
/// output ordering, exclusion semantics, and per-file Verified freshness...
/// is real feature work"). Replaces the SQLite listing fast path
/// (`query_entities_listing`/`write_entities_listing_json`, deleted with this
/// change — item 4, partial) with a direct index read: no walk, no SQL.
///
/// `None` — never a wrong answer, only "cannot answer fast" — whenever:
/// the request doesn't match the index's own build scope (a custom
/// `--file-exts`/`--no-default-excludes`/`.semignore` combination, i.e.
/// anything but `CacheSourceScope::Default`, since the index was built once,
/// at default scope); or the index is absent/stale-salted. On `Some`, every
/// listed file that is individually Verified-stale is self-healed by
/// a per-file re-extract, the same discipline `query.rs`'s def-side repair
/// uses — a directory reroute never bails an entire large listing over one
/// stale file the way the entity-scoped verbs bail an entire *related* set.
///
/// **Disclosed gap, not a regression**: this is `Verified`, not `Complete`
/// — a file created under this directory since the index's last build
/// is invisible until the next full rebuild repairs the image. `DIRS` (the
/// section `Complete` needs to prove corpus-wide membership cheaply) is
/// still reserved, so this capability cannot offer more than S2's
/// `find`/`callers`/`refs` verbs already ship with for a name that exists
/// only in a brand-new file. A future `--complete` flag, if the gap proves
/// to matter in practice, is the natural way to opt back into the walk.
fn try_index_entities_for_dir(
    cwd: &str,
    root: &Path,
    full_dir: &Path,
    registry: &ParserRegistry,
    ext_filter: &[String],
    no_default_excludes: bool,
) -> Option<(usize, Vec<SemanticEntity>)> {
    if !matches!(
        super::graph::cache_source_scope(root, ext_filter, no_default_excludes),
        sem_mcp::cache::CacheSourceScope::Default
    ) {
        return None;
    }
    let idx = super::query::open_index(root)?;
    let rel =
        super::normalize_repo_relative_path(Path::new(cwd), root, &full_dir.to_string_lossy());
    let prefix = if rel == "." {
        String::new()
    } else {
        format!("{rel}/")
    };

    let files: Vec<String> = idx
        .files_under(&prefix)
        .into_iter()
        .map(str::to_string)
        .collect();
    let file_count = files.len();

    let mut out = Vec::with_capacity(files.len());
    for file in &files {
        if super::query::is_file_stale(&idx, root, file) {
            let Ok(content) = std::fs::read_to_string(root.join(file)) else {
                continue; // deleted since the index listed it — a walk would
                          // simply not have found it either; drop, don't fail.
            };
            out.extend(
                registry
                    .extract_entities_brief(file, &content)
                    .into_iter()
                    .map(|e| {
                        entity_info_to_entity(EntityInfo {
                            id: e.id,
                            name: e.name,
                            entity_type: e.entity_type,
                            file_path: e.file_path,
                            parent_id: e.parent_id,
                            start_line: e.start_line,
                            end_line: e.end_line,
                        })
                    }),
            );
        } else {
            out.extend(
                idx.entities_in_file(file)
                    .iter()
                    .map(|e| entity_info_to_entity(e.to_entity_info())),
            );
        }
    }
    Some((file_count, out))
}

fn entity_info_to_entity(entity: EntityInfo) -> SemanticEntity {
    SemanticEntity {
        id: entity.id,
        file_path: entity.file_path,
        entity_type: entity.entity_type,
        name: entity.name,
        parent_id: entity.parent_id,
        content: String::new(),
        content_hash: String::new(),
        structural_hash: None,

        kappa: None,
        start_line: entity.start_line,
        end_line: entity.end_line,
        start_byte: None,
        end_byte: None,
        metadata: None,
    }
}

fn file_path_for_entity(root: &Path, path: &Path) -> String {
    super::files::file_path_for_entity(root, path)
}

/// Entity-addressed text search: one line per hit (file, innermost entity,
/// line, matched text) instead of whole bodies — the token-cheap way to
/// verify a call site or find a string.
///
/// This still runs the full local-graph rebuild path below rather than
/// `sem grep`'s mmap trigram tier — extending this
/// entity-addressed shape onto the index is out of GREP-KILLER S4's scope
/// not something this deletion could reach; flagged as a
/// disclosed follow-on rather than silently left implying it's fast.
const TEXT_SEARCH_LIMIT: usize = 50;

fn text_search(opts: &EntitiesOptions, needle: &str) {
    use sem_core::git::bridge::GitBridge;

    let root = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(git) => git.repo_root().to_path_buf(),
        Err(_) => Path::new(&opts.cwd).to_path_buf(),
    };

    let registry = super::create_registry(&root.to_string_lossy());
    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let source_scope =
        super::graph::cache_source_scope(&root, &ext_filter, opts.no_default_excludes);
    let file_paths = super::graph::find_supported_files_with_options(
        &root,
        &registry,
        &ext_filter,
        opts.no_default_excludes,
    );
    let (_, all_entities) =
        super::graph::get_or_build_graph(&root, &file_paths, &registry, false, source_scope);
    print!(
        "{}",
        sem_mcp::server::SemServer::render_text_hits(&all_entities, needle, TEXT_SEARCH_LIMIT)
    );
}

fn extract_file_entities(
    full_path: &Path,
    registry: &ParserRegistry,
    file_path: &str,
) -> Result<Vec<SemanticEntity>, std::io::Error> {
    let content = std::fs::read_to_string(&full_path)?;
    Ok(registry.extract_entities_brief(file_path, &content))
}

#[derive(Serialize)]
struct EntityJsonRow<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    entity_type: &'a str,
    start_line: usize,
    end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_byte: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_byte: Option<usize>,
    parent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    header: Option<&'a [String]>,
}

fn write_entities_json(
    entities: &[SemanticEntity],
    include_file: bool,
    headers: Option<&HashMap<String, Vec<String>>>,
) -> io::Result<u64> {
    let stdout = io::stdout();
    let mut writer = CountingWriter::new(stdout.lock());
    writer.write_all(b"[")?;
    for (index, entity) in entities.iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        let row = EntityJsonRow {
            name: &entity.name,
            entity_type: &entity.entity_type,
            start_line: entity.start_line,
            end_line: entity.end_line,
            start_byte: entity.start_byte,
            end_byte: entity.end_byte,
            parent_id: entity.parent_id.as_deref(),
            file: include_file.then_some(entity.file_path.as_str()),
            header: headers
                .and_then(|h| h.get(entity.id.as_str()))
                .map(Vec::as_slice),
        };
        serde_json::to_writer(&mut writer, &row)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
    }
    writer.write_all(b"]\n")?;
    Ok(writer.bytes())
}

struct CountingWriter<W> {
    inner: W,
    bytes: u64,
}

impl<W> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, bytes: 0 }
    }

    fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn print_file_entities(
    file_path: &str,
    entities: &[SemanticEntity],
    headers: Option<&HashMap<String, Vec<String>>>,
) {
    println!("{} {}\n", "entities:".green().bold(), file_path.bold());
    print_entity_rows(entities, "  ", headers);
}

fn should_group_by_file(entities: &[SemanticEntity]) -> bool {
    let files: BTreeSet<&str> = entities.iter().map(|e| e.file_path.as_str()).collect();
    files.len() > 1
}

fn print_grouped_entities(
    path_label: &str,
    entities: &[SemanticEntity],
    headers: Option<&HashMap<String, Vec<String>>>,
) {
    println!("{} {}\n", "entities:".green().bold(), path_label.bold());

    let mut current_file: Option<&str> = None;
    let entities_by_id = entities_by_id(entities);
    for entity in entities {
        if current_file != Some(entity.file_path.as_str()) {
            current_file = Some(entity.file_path.as_str());
            println!("  {}", entity.file_path.bold());
        }

        let indent = entity_indent(entity, &entities_by_id, "    ");
        print_entity_row(entity, &indent, headers);
    }
}

fn print_entity_rows(
    entities: &[SemanticEntity],
    base_indent: &str,
    headers: Option<&HashMap<String, Vec<String>>>,
) {
    let entities_by_id = entities_by_id(entities);
    for entity in entities {
        let indent = entity_indent(entity, &entities_by_id, base_indent);
        print_entity_row(entity, &indent, headers);
    }
}

fn entities_by_id(entities: &[SemanticEntity]) -> HashMap<&str, &SemanticEntity> {
    entities
        .iter()
        .map(|entity| (entity.id.as_str(), entity))
        .collect()
}

fn entity_indent(
    entity: &SemanticEntity,
    entities_by_id: &HashMap<&str, &SemanticEntity>,
    base_indent: &str,
) -> String {
    format!(
        "{base_indent}{}",
        "  ".repeat(entity_depth(entity, entities_by_id))
    )
}

fn entity_depth(entity: &SemanticEntity, entities_by_id: &HashMap<&str, &SemanticEntity>) -> usize {
    let mut depth = 0;
    let mut current_parent = entity.parent_id.as_deref();
    let mut seen = HashSet::new();

    while let Some(parent_id) = current_parent {
        if !seen.insert(parent_id) {
            break;
        }
        depth += 1;
        current_parent = entities_by_id
            .get(parent_id)
            .and_then(|parent| parent.parent_id.as_deref());
    }

    depth
}

fn print_entity_row(
    entity: &SemanticEntity,
    indent: &str,
    headers: Option<&HashMap<String, Vec<String>>>,
) {
    println!(
        "{}{} {} (L{}:{})",
        indent,
        entity.entity_type.dimmed(),
        entity.name.bold(),
        entity.start_line,
        entity.end_line,
    );
    if let Some(header) = headers.and_then(|h| h.get(entity.id.as_str())) {
        for line in header {
            println!("{indent}  {}", line.dimmed());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(id: &str, parent_id: Option<&str>) -> SemanticEntity {
        SemanticEntity {
            id: id.to_string(),
            file_path: "a.ts".to_string(),
            entity_type: "field".to_string(),
            name: id.rsplit("::").next().unwrap_or(id).to_string(),
            parent_id: parent_id.map(String::from),
            content: String::new(),
            content_hash: String::new(),
            structural_hash: None,

            kappa: None,
            start_line: 1,
            end_line: 1,
            start_byte: None,
            end_byte: None,
            metadata: None,
        }
    }

    fn kinded(name: &str, kind: &str) -> SemanticEntity {
        let mut e = entity(name, None);
        e.entity_type = kind.to_string();
        e
    }

    #[test]
    fn only_keeps_listed_kinds() {
        let es = vec![
            kinded("a", "function"),
            kinded("b", "struct"),
            kinded("c", "import"),
        ];
        let out = filter_by_kind(es, &["function".into(), "struct".into()], &[]).unwrap();
        let kinds: Vec<&str> = out.iter().map(|e| e.entity_type.as_str()).collect();
        assert_eq!(kinds, vec!["function", "struct"]);
    }

    #[test]
    fn except_drops_listed_kinds() {
        let es = vec![kinded("a", "function"), kinded("b", "import")];
        let out = filter_by_kind(es, &[], &["import".into()]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entity_type, "function");
    }

    #[test]
    fn unknown_kind_errors_and_lists_present_kinds() {
        let es = vec![kinded("a", "function"), kinded("b", "struct")];
        let err = filter_by_kind(es, &["tests".into()], &[]).unwrap_err();
        assert!(err.contains("unknown entity kind \"tests\""));
        assert!(err.contains("function") && err.contains("struct"));
    }

    #[test]
    fn no_filter_returns_all() {
        let es = vec![kinded("a", "function"), kinded("b", "struct")];
        assert_eq!(filter_by_kind(es, &[], &[]).unwrap().len(), 2);
    }

    #[test]
    fn entity_depth_follows_parent_chain() {
        let root = entity("a.ts::class::L1", None);
        let child = entity("a.ts::class::L1::L2", Some("a.ts::class::L1"));
        let grandchild = entity("a.ts::class::L1::L2::L3", Some("a.ts::class::L1::L2"));
        let entities = vec![root, child, grandchild];
        let entities_by_id = entities_by_id(&entities);

        assert_eq!(entity_depth(&entities[0], &entities_by_id), 0);
        assert_eq!(entity_depth(&entities[1], &entities_by_id), 1);
        assert_eq!(entity_depth(&entities[2], &entities_by_id), 2);
        assert_eq!(entity_indent(&entities[2], &entities_by_id, "  "), "      ");
    }

    #[test]
    fn entity_depth_handles_missing_or_cyclic_parents() {
        let missing = entity("a.ts::field::missing", Some("a.ts::field::unknown"));
        let cyclic = entity("a.ts::field::cyclic", Some("a.ts::field::cyclic"));
        let entities = vec![missing, cyclic];
        let entities_by_id = entities_by_id(&entities);

        assert_eq!(entity_depth(&entities[0], &entities_by_id), 1);
        assert_eq!(entity_depth(&entities[1], &entities_by_id), 1);
    }

    #[test]
    fn entities_command_computes_repo_root_once_for_multiple_file_args() {
        // SEM_NO_INDEX unset is this test's precondition: it's proving the
        // index-root lookup is hoisted out of the per-file loop, which only
        // matters on the path where that lookup runs at all. Guard against
        // ambient state leaking in from elsewhere in the process.
        std::env::remove_var("SEM_NO_INDEX");

        let temp = tempfile::TempDir::new().unwrap();
        git2::Repository::init(temp.path()).unwrap();
        for name in ["a.ts", "b.ts", "c.ts"] {
            std::fs::write(temp.path().join(name), "export const x = 1;\n").unwrap();
        }

        super::super::reset_repo_root_lookup_count_for_test();

        entities_command(EntitiesOptions {
            cwd: temp.path().to_string_lossy().to_string(),
            paths: vec!["a.ts".into(), "b.ts".into(), "c.ts".into()],
            json: true,
            no_default_excludes: false,
            file_exts: vec![],
            only_kinds: vec![],
            except_kinds: vec![],
            text: None,
            signatures: false,
        });

        assert_eq!(
            super::super::repo_root_lookup_count_for_test(),
            1,
            "3 file arguments against the same cwd must share one repo-root lookup, not one per file"
        );
    }
}
