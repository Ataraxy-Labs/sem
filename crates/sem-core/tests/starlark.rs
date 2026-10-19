//! Bazel and Buck2 build files parse as Starlark: targets in BUILD/BUCK files,
//! functions in `.bzl` files (#519).

use sem_core::parser::plugins::create_default_registry;

fn names(path: &str, src: &str) -> Vec<(String, String)> {
    create_default_registry()
        .extract_entities(path, src)
        .into_iter()
        .map(|e| (e.entity_type, e.name))
        .collect()
}

#[test]
fn build_file_targets_are_entities() {
    let src = r#"load("@rules_cc//cc:defs.bzl", "cc_binary", "cc_library")

cc_library(
    name = "greet",
    srcs = ["greet.cc"],
)

my_macro(name = 'hello', deps = [":greet"])
"#;
    let want = vec![
        ("target".to_string(), "greet".to_string()),
        ("target".to_string(), "hello".to_string()),
    ];
    for path in [
        "BUILD",
        "pkg/BUCK",
        "BUILD.bazel",
        "pkg/PACKAGE",
        "WORKSPACE",
    ] {
        assert_eq!(names(path, src), want, "{path}");
    }
}

#[test]
fn bzl_functions_are_entities_and_their_calls_are_not_targets() {
    let src = "def my_test(name, srcs):\n    native.sh_test(name = name, srcs = srcs)\n";
    assert_eq!(
        names("tools/macros.bzl", src),
        vec![("function".to_string(), "my_test".to_string())]
    );
}

#[test]
fn python_files_are_unchanged() {
    let src = "setup(name = \"pkg\")\n\ndef main():\n    pass\n";
    assert_eq!(
        names("setup.py", src),
        vec![("function".to_string(), "main".to_string())]
    );
}
