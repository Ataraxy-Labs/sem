use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::{cache::DiskCache, timings::Timings};
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
}

pub fn entities_command(opts: EntitiesOptions) {
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

    // The cloud fast-path only helps the whole-repo single listing, and it
    // can't apply a local kind filter, so skip it when one is active.
    if ext_filter.is_empty()
        && path_args.len() == 1
        && !kind_filter_active
        && super::cloud::try_cloud_entities(&opts).is_some()
    {
        timings.mark("cloud_entities");
        timings.finish();
        return;
    }

    let root = Path::new(&opts.cwd);
    let registry = super::create_registry(&opts.cwd);

    let mut entities: Vec<SemanticEntity> = Vec::new();
    let mut dir_count = 0usize;
    let mut file_arg_count = 0usize;
    let mut processed_file_count = 0usize;
    let mut discovered_file_count = 0usize;
    let mut extracted_entities = false;
    for path_arg in &path_args {
        let (path_label, full_path) = resolve_path(root, path_arg);
        if full_path.is_file() {
            file_arg_count += 1;
            processed_file_count += 1;
            let file_entities = extract_file_entities(&full_path, &registry, &path_label)
                .unwrap_or_else(|e| {
                    eprintln!(
                        "{} Cannot read '{}': {}",
                        "error:".red().bold(),
                        path_label,
                        e
                    );
                    std::process::exit(1);
                });
            entities.extend(file_entities);
            extracted_entities = true;
        } else if full_path.is_dir() {
            dir_count += 1;
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
            if opts.json
                && path_args.len() == 1
                && !kind_filter_active
                && try_write_cached_entities_json(
                    root,
                    &file_paths,
                    &ext_filter,
                    opts.no_default_excludes,
                    &mut timings,
                )
            {
                timings.counter("input_files", processed_file_count as u64);
                timings.counter("input_file_args", file_arg_count as u64);
                timings.counter("input_dirs", dir_count as u64);
                timings.counter("processed_files", processed_file_count as u64);
                timings.counter("discovered_files", discovered_file_count as u64);
                timings.mark("output_serialization");
                timings.finish();
                return;
            }
            if let Some(cached_entities) = try_cached_entities(
                root,
                &file_paths,
                &ext_filter,
                opts.no_default_excludes,
                &mut timings,
            ) {
                entities.extend(cached_entities);
            } else {
                entities.extend(extract_files_entities(root, &file_paths, &registry));
                extracted_entities = true;
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

    if opts.json {
        let json_bytes = write_entities_json(&entities, include_file).unwrap_or_else(|e| {
            eprintln!("{} Cannot write JSON output: {}", "error:".red().bold(), e);
            std::process::exit(1);
        });
        timings.counter("json_bytes", json_bytes);
    } else if should_group_by_file(&entities) {
        print_grouped_entities(&display_label, &entities);
    } else if let Some(file_path) = entities.first().map(|e| e.file_path.as_str()) {
        print_file_entities(file_path, &entities);
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

fn try_cached_entities(
    root: &Path,
    file_paths: &[String],
    ext_filter: &[String],
    no_default_excludes: bool,
    timings: &mut Timings,
) -> Option<Vec<SemanticEntity>> {
    let source_scope = super::graph::cache_source_scope(root, ext_filter, no_default_excludes);
    let cache = match DiskCache::open_existing_readonly(root) {
        Ok(cache) => {
            timings.mark("cache_open");
            cache
        }
        Err(_) => {
            timings.mark("cache_open_failed");
            return None;
        }
    };

    match cache.query_entities_listing(root, file_paths, source_scope) {
        Ok(Some(entities)) => {
            timings.counter("cached_entities", entities.len() as u64);
            timings.mark("cache_entities_query");
            Some(entities.into_iter().map(entity_info_to_entity).collect())
        }
        Ok(None) => {
            timings.mark("cache_entities_miss");
            None
        }
        Err(_) => {
            timings.mark("cache_entities_query_failed");
            None
        }
    }
}

fn try_write_cached_entities_json(
    root: &Path,
    file_paths: &[String],
    ext_filter: &[String],
    no_default_excludes: bool,
    timings: &mut Timings,
) -> bool {
    let source_scope = super::graph::cache_source_scope(root, ext_filter, no_default_excludes);
    let cache = match DiskCache::open_existing_readonly(root) {
        Ok(cache) => {
            timings.mark("cache_open");
            cache
        }
        Err(_) => {
            timings.mark("cache_open_failed");
            return false;
        }
    };

    let stdout = io::stdout();
    let mut writer = CountingWriter::new(stdout.lock());
    match cache.write_entities_listing_json(root, file_paths, source_scope, true, &mut writer) {
        Ok(Some(entity_count)) => {
            timings.counter("cached_entities", entity_count);
            timings.counter("entities", entity_count);
            timings.counter("json_bytes", writer.bytes());
            timings.mark("cache_entities_query");
            true
        }
        Ok(None) => {
            timings.mark("cache_entities_miss");
            false
        }
        Err(error) => {
            eprintln!(
                "{} Cannot write cached JSON output: {}",
                "error:".red().bold(),
                error
            );
            std::process::exit(1);
        }
    }
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
        start_line: entity.start_line,
        end_line: entity.end_line,
        metadata: None,
    }
}

fn file_path_for_entity(root: &Path, path: &Path) -> String {
    super::files::file_path_for_entity(root, path)
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
    parent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<&'a str>,
}

fn write_entities_json(entities: &[SemanticEntity], include_file: bool) -> io::Result<u64> {
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
            parent_id: entity.parent_id.as_deref(),
            file: include_file.then_some(entity.file_path.as_str()),
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

fn print_file_entities(file_path: &str, entities: &[SemanticEntity]) {
    println!("{} {}\n", "entities:".green().bold(), file_path.bold());
    print_entity_rows(entities, "  ");
}

fn should_group_by_file(entities: &[SemanticEntity]) -> bool {
    let files: BTreeSet<&str> = entities.iter().map(|e| e.file_path.as_str()).collect();
    files.len() > 1
}

fn print_grouped_entities(path_label: &str, entities: &[SemanticEntity]) {
    println!("{} {}\n", "entities:".green().bold(), path_label.bold());

    let mut current_file: Option<&str> = None;
    let entities_by_id = entities_by_id(entities);
    for entity in entities {
        if current_file != Some(entity.file_path.as_str()) {
            current_file = Some(entity.file_path.as_str());
            println!("  {}", entity.file_path.bold());
        }

        let indent = entity_indent(entity, &entities_by_id, "    ");
        print_entity_row(entity, &indent);
    }
}

fn print_entity_rows(entities: &[SemanticEntity], base_indent: &str) {
    let entities_by_id = entities_by_id(entities);
    for entity in entities {
        let indent = entity_indent(entity, &entities_by_id, base_indent);
        print_entity_row(entity, &indent);
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

fn print_entity_row(entity: &SemanticEntity, indent: &str) {
    println!(
        "{}{} {} (L{}:{})",
        indent,
        entity.entity_type.dimmed(),
        entity.name.bold(),
        entity.start_line,
        entity.end_line,
    );
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
            start_line: 1,
            end_line: 1,
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
}
