//! Compact text rendering for MCP tool results.
//!
//! Tool results are read by models, and by humans expanding the tool widget in
//! an agent UI. A grouped tree carries the same information as a pretty-printed
//! JSON dump at a fraction of the tokens (one file path per file instead of per
//! entity, no repeated keys/braces) and reads at a glance. Nothing is elided:
//! every entity name is present, grouped under its file.

use serde_json::Value;

fn get_str<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn get_u64(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn get_arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Entity display name: `name` for functions/methods, `name (type)` otherwise,
/// so uncommon kinds stay visible without noising the common case.
fn entity_label(e: &Value) -> String {
    // impact entities carry `name`; context entries carry `entity`.
    let name = match get_str(e, "name") {
        "" => get_str(e, "entity"),
        n => n,
    };
    let ty = get_str(e, "type");
    match ty {
        "" | "function" | "method" => name.to_string(),
        _ => format!("{} ({})", name, ty),
    }
}

/// True for names/files that look like tests — used only for display ordering
/// (tests sink to the bottom), never to drop anything.
fn looks_like_test(e: &Value) -> bool {
    let name = get_str(e, "name");
    let file = get_str(e, "file");
    name.starts_with("test") || file.contains("/tests/") || file.ends_with("_test.rs")
}

/// Group entities by file (first-seen order), one branch line per file:
/// `├─▶ path: name, name (type), name` — non-test files first so the callers
/// that matter are on top; every name is preserved.
fn push_grouped(out: &mut String, list: &[Value]) {
    let mut order: Vec<&str> = Vec::new();
    let mut by_file: std::collections::HashMap<&str, (Vec<String>, bool)> =
        std::collections::HashMap::new();
    for e in list {
        let file = get_str(e, "file");
        if !by_file.contains_key(file) {
            order.push(file);
        }
        let entry = by_file.entry(file).or_insert_with(|| (Vec::new(), true));
        entry.0.push(entity_label(e));
        entry.1 = entry.1 && looks_like_test(e);
    }
    // Files whose entries are all tests sink to the bottom (stable otherwise).
    let mut ordered: Vec<&str> = order.clone();
    ordered.sort_by_key(|f| by_file[f].1);
    for (idx, file) in ordered.iter().enumerate() {
        let (names, _) = &by_file[file];
        let branch = if idx + 1 == ordered.len() {
            "╰─▶"
        } else {
            "├─▶"
        };
        let file = if file.is_empty() {
            "(unknown file)"
        } else {
            file
        };
        out.push_str(&format!("{} {}: {}\n", branch, file, names.join(", ")));
    }
}

fn file_count(list: &[Value]) -> usize {
    let mut files: Vec<&str> = list.iter().map(|e| get_str(e, "file")).collect();
    files.sort_unstable();
    files.dedup();
    files.len()
}

/// "direct_dependency" -> "direct dependencies", "direct_dependent" -> "direct dependents".
fn pluralize_role(role: &str) -> String {
    let spaced = role.replace('_', " ");
    if let Some(stem) = spaced.strip_suffix('y') {
        format!("{stem}ies")
    } else {
        format!("{spaced}s")
    }
}

fn footer(v: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(ms) = get_u64(v, "elapsed_ms") {
        parts.push(format!("{}ms", ms));
    }
    let source = get_str(v, "source");
    if !source.is_empty() {
        parts.push(source.to_string());
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join(" · ")
    }
}

/// Render a sem_impact result (any mode: all, deps, dependents, tests).
pub fn impact_text(v: &Value) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "◉ {} · {}\n",
        get_str(v, "entity"),
        get_str(v, "file")
    ));

    let deps = get_arr(v, "dependencies");
    let dependents = get_arr(v, "dependents");
    let tests = get_arr(v, "tests");

    if !dependents.is_empty() {
        out.push_str(&format!(
            "← {} dependents · {} files\n",
            dependents.len(),
            file_count(dependents)
        ));
        push_grouped(&mut out, dependents);
    } else if get_str(v, "mode") == "dependents" || get_str(v, "mode") == "all" {
        out.push_str("← no dependents\n");
    }

    if !deps.is_empty() {
        out.push_str(&format!(
            "→ {} dependencies · {} files\n",
            deps.len(),
            file_count(deps)
        ));
        push_grouped(&mut out, deps);
    } else if get_str(v, "mode") == "deps" || get_str(v, "mode") == "all" {
        out.push_str("→ no dependencies\n");
    }

    if let Some(impact) = v.get("impact") {
        let total = get_u64(impact, "total").unwrap_or(0);
        let entities = get_arr(impact, "entities");
        if total > 0 {
            out.push_str(&format!(
                "⚡ {} transitively affected · {} files\n",
                total,
                file_count(entities)
            ));
            // Direct dependents are already listed above; show only the entities
            // beyond them so the transitive tail is visible without repetition.
            let direct: std::collections::HashSet<(&str, &str)> = dependents
                .iter()
                .map(|e| (get_str(e, "file"), get_str(e, "name")))
                .collect();
            let beyond: Vec<Value> = entities
                .iter()
                .filter(|e| !direct.contains(&(get_str(e, "file"), get_str(e, "name"))))
                .cloned()
                .collect();
            if !beyond.is_empty() {
                push_grouped(&mut out, &beyond);
            }
        } else {
            out.push_str("⚡ nothing transitively affected\n");
        }
    }

    if !tests.is_empty() {
        out.push_str(&format!(
            "✓ {} tests affected · {} files\n",
            tests.len(),
            file_count(tests)
        ));
        push_grouped(&mut out, tests);
    }
    if let Some(n) = get_u64(v, "tests_affected") {
        if n == 0 {
            out.push_str("✓ no tests affected\n");
        }
    }

    let f = footer(v);
    if !f.is_empty() {
        out.push_str(&f);
        out.push('\n');
    }
    out
}

/// Render a sem_context result: a compact header, then each entry with its
/// role and verbatim content (the content is the product; never elide it).
pub fn context_text(v: &Value) -> String {
    let mut out = String::new();
    let entries = get_arr(v, "context");
    let used = get_u64(v, "tokens_used").unwrap_or(0);
    let budget = get_u64(v, "token_budget").unwrap_or(0);
    let mut header = format!(
        "⊕ context · {} entries · {}/{} tokens",
        entries.len(),
        used,
        budget
    );
    let f = footer(v);
    if !f.is_empty() {
        header.push_str(&format!(" · {}", f));
    }
    out.push_str(&header);
    out.push('\n');
    if v.get("truncated").and_then(Value::as_bool).unwrap_or(false) {
        out.push_str("(truncated to fit the token budget)\n");
    }
    if v.get("target_omitted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.push_str("(target too large for the budget; showing neighbors)\n");
    }

    for e in entries {
        let tokens = get_u64(e, "tokens")
            .map(|t| format!(" · {} tok", t))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n── {} · {} · {}{}\n",
            get_str(e, "role"),
            entity_label(e),
            get_str(e, "file"),
            tokens
        ));
        let content = get_str(e, "content");
        if !content.is_empty() {
            out.push_str(content);
            if !content.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    let omitted = get_arr(v, "omitted");
    if !omitted.is_empty() {
        let parts: Vec<String> = omitted
            .iter()
            .map(|t| {
                let role = pluralize_role(get_str(t, "role"));
                let n = get_u64(t, "entities").unwrap_or(0);
                let tests = get_u64(t, "tests").unwrap_or(0);
                if tests > 0 {
                    format!("+{} {} ({} tests)", n, role, tests)
                } else {
                    format!("+{} {}", n, role)
                }
            })
            .collect();
        out.push_str(&format!(
            "\n╰ not packed: {} — sem_impact lists them\n",
            parts.join(" · ")
        ));
    }
    out
}

/// Render repo-level history analytics (sem_log with no entity): hotspots and
/// co-change pairs. Caps mirror the CLI (15 hotspots, 12 pairs) with an
/// explicit "more" note, so nothing is silently hidden.
pub fn history_text(a: &sem_core::parser::hotspot::HistoryAnalytics) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "⊕ repo history · last {} commits\n",
        a.commits_scanned
    ));
    if a.hotspots.is_empty() {
        out.push_str("no entity changes found\n");
        return out;
    }

    out.push_str("\nhotspots — most-changed entities\n");
    for h in a.hotspots.iter().take(15) {
        out.push_str(&format!(
            "  {}× {} · {} · {} author{} · last {}\n",
            h.commits,
            h.entity_name,
            h.file_path,
            h.authors,
            if h.authors == 1 { "" } else { "s" },
            h.last_short_sha,
        ));
    }
    if a.hotspots.len() > 15 {
        out.push_str(&format!("  … {} more\n", a.hotspots.len() - 15));
    }

    if !a.co_changes.is_empty() {
        out.push_str("\nco-changes — entities that change together\n");
        for p in a.co_changes.iter().take(12) {
            let files = if p.a_file == p.b_file {
                p.a_file.clone()
            } else {
                format!("{} ↔ {}", p.a_file, p.b_file)
            };
            out.push_str(&format!(
                "  {}× {} ↔ {} · {:.0}% · {}\n",
                p.together,
                p.a_name,
                p.b_name,
                p.confidence * 100.0,
                files,
            ));
        }
        if a.co_changes.len() > 12 {
            out.push_str(&format!("  … {} more pairs\n", a.co_changes.len() - 12));
        }
    }

    if a.pair_commits_skipped > 0 {
        out.push_str(&format!(
            "\nnote: {} bulk commits (>50 entities) excluded from co-change pairing\n",
            a.pair_commits_skipped
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn impact_groups_by_file_and_keeps_every_name() {
        let v = json!({
            "entity": "compute", "file": "src/a.rs", "mode": "all",
            "dependencies": [
                {"name": "helper", "type": "function", "file": "src/a.rs"},
                {"name": "Config", "type": "struct", "file": "src/b.rs"},
            ],
            "dependents": [
                {"name": "run", "type": "function", "file": "src/c.rs"},
                {"name": "run", "type": "function", "file": "src/d.rs"},
            ],
            "impact": {"total": 3, "entities": [
                {"name": "run", "type": "function", "file": "src/c.rs"},
                {"name": "run", "type": "function", "file": "src/d.rs"},
                {"name": "main", "type": "function", "file": "src/main.rs"},
            ]},
            "tests": [],
            "elapsed_ms": 12, "source": "local",
        });
        let text = impact_text(&v);
        assert!(text.contains("◉ compute · src/a.rs"));
        assert!(text.contains("← 2 dependents · 2 files"));
        assert!(text.contains("├─▶ src/c.rs: run") || text.contains("╰─▶ src/c.rs: run"));
        assert!(text.contains("src/b.rs: Config (struct)"));
        assert!(text.contains("⚡ 3 transitively affected · 3 files"));
        // transitive tail shows only what direct dependents didn't already list
        assert!(text.contains("src/main.rs: main"));
        assert!(text.contains("12ms · local"));
    }

    #[test]
    fn impact_dependents_mode_without_edges_says_so() {
        let v = json!({
            "entity": "orphan", "file": "src/a.rs", "mode": "dependents",
            "dependents": [], "elapsed_ms": 3, "source": "local",
        });
        let text = impact_text(&v);
        assert!(text.contains("← no dependents"));
    }

    #[test]
    fn context_renders_entries_with_verbatim_content() {
        let v = json!({
            "token_budget": 8000, "tokens_used": 950, "truncated": false,
            "target_omitted": false,
            "context": [
                {"entity": "sync", "type": "function", "file": "src/s.rs",
                 "role": "target", "tokens": 700, "content": "fn sync() {}\n"},
                {"entity": "helper", "type": "function", "file": "src/h.rs",
                 "role": "dependency", "tokens": 250, "content": "fn helper() {}"},
            ],
            "entries": 2, "elapsed_ms": 7, "source": "local",
        });
        let text = context_text(&v);
        assert!(text.contains("⊕ context · 2 entries · 950/8000 tokens · 7ms · local"));
        assert!(text.contains("── target · sync · src/s.rs · 700 tok"));
        assert!(text.contains("fn sync() {}"));
        assert!(text.contains("── dependency · helper · src/h.rs · 250 tok"));
        assert!(text.contains("fn helper() {}"));
    }
}
