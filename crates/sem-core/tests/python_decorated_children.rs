#![cfg(feature = "lang-python")]

use sem_core::parser::plugins::create_default_registry;

#[test]
fn decorated_definitions_preserve_nested_entities_and_ranges() {
    let source = r#"@first
@second(option=True)
class FormattedExcinfo:
    def repr_excinfo(self, excinfo):
        return excinfo

    @staticmethod
    def helper():
        def nested():
            return 1
        return nested()

    @decorate
    class Inner:
        def method(self):
            return 2

@decorate
def factory():
    def child():
        return 3
    return child
"#;
    let registry = create_default_registry();
    let entities = registry.extract_entities("example.py", source);
    let expected = [
        ("FormattedExcinfo", None),
        ("repr_excinfo", Some("FormattedExcinfo")),
        ("helper", Some("FormattedExcinfo")),
        ("nested", Some("helper")),
        ("Inner", Some("FormattedExcinfo")),
        ("method", Some("Inner")),
        ("factory", None),
        ("child", Some("factory")),
    ];
    for (name, parent) in expected {
        let matches: Vec<_> = entities.iter().filter(|e| e.name == name).collect();
        assert_eq!(matches.len(), 1, "expected exactly one {name}");
        let entity = matches[0];
        let parent_id = parent.map(|p| &entities.iter().find(|e| e.name == p).unwrap().id);
        assert_eq!(entity.parent_id.as_ref(), parent_id, "parent of {name}");
        assert_eq!(
            &source[entity.start_byte.unwrap()..entity.end_byte.unwrap()],
            entity.content,
            "source range of {name}"
        );
    }
    assert_eq!(entities.len(), expected.len());
    let class = entities
        .iter()
        .find(|e| e.name == "FormattedExcinfo")
        .unwrap();
    assert!(class.content.starts_with("@first\n@second"));
    let helper = entities.iter().find(|e| e.name == "helper").unwrap();
    assert!(helper.content.starts_with("@staticmethod"));
}
