use sem_core::model::entity_id::EntityId;
use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityInfoMap, EntityRef, RefType};
use std::collections::HashMap;

// Keep lifecycle checks in their own test executable: the interner is shared
// process-wide, so unrelated parallel graph tests would change live_count().
#[test]
fn interned_ids_preserve_identity_wire_format_and_reclaim_storage() {
    assert_eq!(
        std::mem::size_of::<EntityId>(),
        std::mem::size_of::<usize>()
    );
    assert_eq!(
        std::mem::size_of::<Option<EntityId>>(),
        std::mem::size_of::<usize>()
    );
    let baseline = EntityId::live_count();
    {
        let text = "src/über.py::function::café";
        let a = EntityId::from(text);
        let b = EntityId::from(text.to_owned());
        assert_eq!(a, b);
        assert_eq!(a.as_str().as_ptr(), b.as_str().as_ptr());
        assert_eq!(EntityId::live_count(), baseline + 1);
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(move || EntityId::from(text)))
            .collect();
        for thread in handles {
            let id = thread.join().unwrap();
            assert_eq!(id.as_str().as_ptr(), a.as_str().as_ptr());
        }
        let map = HashMap::from([(a.clone(), 42)]);
        assert_eq!(map.get(text), Some(&42));
        assert_eq!(map.get(&text.to_owned()), Some(&42));
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(text).unwrap()
        );
        let decoded: EntityId =
            serde_json::from_str(&serde_json::to_string(text).unwrap()).unwrap();
        assert_eq!(decoded.as_str().as_ptr(), a.as_str().as_ptr());
        let mut old_cbor = Vec::new();
        let mut new_cbor = Vec::new();
        ciborium::into_writer(text, &mut old_cbor).unwrap();
        ciborium::into_writer(&a, &mut new_cbor).unwrap();
        assert_eq!(old_cbor, new_cbor);
        let from_old: EntityId = ciborium::from_reader(old_cbor.as_slice()).unwrap();
        assert_eq!(from_old, a);
        #[cfg(feature = "disk-cache")]
        {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            let from_sql: EntityId = conn.query_row("SELECT ?1", [&a], |row| row.get(0)).unwrap();
            assert_eq!(from_sql.as_str().as_ptr(), a.as_str().as_ptr());
        }
        let mut ids = [
            EntityId::from("z"),
            EntityId::from("a"),
            EntityId::from("m"),
        ];
        ids.sort();
        assert_eq!(ids.map(String::from), ["a", "m", "z"]);

        let mut entities = EntityInfoMap::default();
        for id in [a.clone(), "other.py::function::caller".into()] {
            entities.insert(
                id.clone(),
                EntityInfo {
                    id,
                    name: "example".into(),
                    entity_type: "function".into(),
                    file_path: "example.py".into(),
                    parent_id: None,
                    start_line: 1,
                    end_line: 2,
                },
            );
        }
        let edges = vec![EntityRef {
            from_entity: "other.py::function::caller".into(),
            to_entity: a.clone(),
            ref_type: RefType::Calls,
        }];
        let graph = EntityGraph::from_parts(entities, edges);
        let (key, info) = graph.entities.get_key_value(text).unwrap();
        assert_eq!(key.as_str().as_ptr(), info.id.as_str().as_ptr());
        assert_eq!(
            graph.edges[0].to_entity.as_str().as_ptr(),
            a.as_str().as_ptr()
        );
        let adjacent = &graph.dependencies()["other.py::function::caller"][0];
        assert_eq!(adjacent.as_str().as_ptr(), a.as_str().as_ptr());
        assert_eq!(graph.get_dependents(text).len(), 1);
    }
    assert_eq!(
        EntityId::live_count(),
        baseline,
        "dropped graphs must not leak canonical IDs"
    );
    // A deleted then re-added ID retains the same text identity; no persisted
    // handle number or pointer survives reclamation.
    for _ in 0..100 {
        let id = EntityId::from("readded.py::function::f");
        assert_eq!(id.as_str(), "readded.py::function::f");
        drop(id);
        assert_eq!(EntityId::live_count(), baseline);
    }
}
