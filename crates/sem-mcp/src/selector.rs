//! Shared selectors for graph-backed definitions, context and callers.
use sem_core::parser::graph::{EntityInfo, EntityInfoMap};

pub(crate) fn matches(entities: &EntityInfoMap, entity: &EntityInfo, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() { return false; }
    if entity.id.as_str() == query { return true; }
    let (kind, name) = match query.split_once(' ') {
        Some((k, n)) if !k.contains(['.', ':']) && !n.trim().is_empty() => (Some(k), n.trim()),
        _ => (None, query),
    };
    if kind.is_some_and(|k| k != entity.entity_type && !matches!((k, entity.entity_type.as_str()), ("function", "method") | ("method", "function"))) {
        return false;
    }
    if entity.name == name || entity.id.as_str() == name { return true; }
    // Only actual ancestry qualifies a symbol; do not guess from suffixes.
    let normalized = name.replace("::", ".");
    let mut parts = normalized.rsplit('.');
    if parts.next() != Some(entity.name.as_str()) { return false; }
    let mut current = entity;
    let mut qualified = false;
    for part in parts {
        qualified = true;
        if part.is_empty() { return false; }
        let Some(parent) = current.parent_id.as_ref().and_then(|id| entities.get(id)) else { return false; };
        if parent.name != part { return false; }
        current = parent;
    }
    qualified
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qualified_selectors_resolve_real_ancestry() {
        let mut map = EntityInfoMap::default();
        for (id, name, kind, parent) in [
            ("outer", "Outer", "class", None),
            ("class", "Example", "class", Some("outer")),
            ("method", "toDict", "method", Some("class")),
            ("other", "Other", "class", None),
            ("other_method", "toDict", "method", Some("other")),
            ("orphan", "missing", "function", Some("unknown")),
        ] {
            map.insert(id.into(), EntityInfo { id: id.into(), name: name.into(), entity_type: kind.into(), file_path: "a.py".into(), parent_id: parent.map(Into::into), start_line: 1, end_line: 2 });
        }
        for q in ["toDict", "Example.toDict", "Outer.Example.toDict", "Outer::Example::toDict", "method", "function Example.toDict", "method Example.toDict"] {
            assert!(matches(&map, &map["method"], q), "{q}");
        }
        for q in ["", "Other.toDict", "Outer.toDict", "class Example.toDict", ".Example.toDict", "Example..toDict"] {
            assert!(!matches(&map, &map["method"] , q), "{q}");
        }
        assert!(!matches(&map, &map["other_method"], "Example.toDict"));
        assert!(!matches(&map, &map["orphan"], "Unknown.missing"));
    }
}
