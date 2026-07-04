#[cfg(feature = "lang-elm")]
mod elm {
    use sem_core::parser::plugins::create_default_registry;

    #[test]
    fn elm_extracts_all_entity_types() {
        let registry = create_default_registry();
        let elm_code = r#"module Main exposing (..)

import Html exposing (text)

type alias Model =
    { count : Int
    , name : String
    }

type Msg
    = Increment
    | Decrement

port sendMessage : String -> Cmd msg

update : Msg -> Model -> Model
update msg model =
    case msg of
        Increment ->
            { model | count = model.count + 1 }
        Decrement ->
            { model | count = model.count - 1 }

view model =
    text (String.fromInt model.count)

infix right 5 (|>) = apR
"#;

        let entities = registry.extract_entities("Main.elm", elm_code);
        assert!(
            !entities.is_empty(),
            "Should extract entities from Elm code"
        );

        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"Model"),
            "Should find type alias Model, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Msg"),
            "Should find type Msg, got: {:?}",
            names
        );
        assert!(
            names.contains(&"update"),
            "Should find value update, got: {:?}",
            names
        );
        assert!(
            names.contains(&"view"),
            "Should find value view, got: {:?}",
            names
        );
        assert!(
            names.contains(&"sendMessage"),
            "Should find port sendMessage, got: {:?}",
            names
        );
        assert!(
            names.contains(&"|>"),
            "Should find infix |>, got: {:?}",
            names
        );

        // Verify file paths are correct
        for entity in &entities {
            assert_eq!(entity.file_path, "Main.elm");
        }
    }

    #[test]
    fn elm_local_let_bindings_not_extracted() {
        let registry = create_default_registry();
        let elm_code = r#"module Main exposing (..)

greet name =
    let
        greeting = "Hello, "
    in
    greeting ++ name
"#;

        let entities = registry.extract_entities("Main.elm", elm_code);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"greet"),
            "Should find top-level greet, got: {:?}",
            names
        );
        // "greeting" is a local let binding — should NOT be a top-level entity
        assert!(
            !names.contains(&"greeting"),
            "Should not extract local let binding, got: {:?}",
            names
        );
    }
}
