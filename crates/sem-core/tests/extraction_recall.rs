//! Definitions that used to be missed, checked against each language's own
//! parser or Universal Ctags on real repositories.

use sem_core::parser::plugins::create_default_registry;

fn names(path: &str, src: &str) -> Vec<String> {
    create_default_registry()
        .extract_entities(path, src)
        .into_iter()
        .map(|e| e.name)
        .collect()
}

fn has(path: &str, src: &str, want: &[&str]) {
    let got = names(path, src);
    for w in want {
        assert!(got.iter().any(|n| n == w), "{path}: missing {w}, got {got:?}");
    }
}

#[test]
fn cpp_headers_and_template_members() {
    // A `.h` header written in C++ is read as C++; members of a template
    // class are found.
    has(
        "fmt.h",
        "namespace fmt {\ntemplate <typename T> class buffer {\n public:\n  void flush() {}\n};\nusing bigit = unsigned;\n}\n",
        &["buffer", "flush", "bigit"],
    );
}

#[test]
fn c_function_pointer_typedef() {
    has("m.h", "typedef int (*Handler)(void *ctx);\n", &["Handler"]);
}

#[test]
fn rust_trait_method_signatures() {
    has("lib.rs", "trait Flag {\n    fn name(&self) -> &str;\n}\n", &["Flag", "name"]);
}

#[test]
fn java_annotation_members() {
    has(
        "Intercept.java",
        "public @interface Intercept {\n  Class<?> postDeserialize();\n}\n",
        &["postDeserialize"],
    );
}

#[test]
fn csharp_operators() {
    has(
        "JToken.cs",
        "class JToken {\n  public static explicit operator decimal(JToken v) { return 0; }\n  public static JToken operator +(JToken a, JToken b) { return a; }\n}\n",
        &["operator decimal", "operator +"],
    );
}

#[test]
fn kotlin_companions_and_object_expressions() {
    has(
        "Buf.kt",
        "class Buf {\n  companion object {\n    fun of(): Buf = Buf()\n  }\n  fun source() {\n    val s = object : Source {\n      override fun close() {}\n    }\n  }\n}\n",
        &["Companion", "of", "close"],
    );
}

#[test]
fn kotlin_multiplatform_constructor_is_recovered() {
    // `internal actual constructor(` on its own line is not parsed by the
    // grammar; the definitions in the class are recovered.
    has(
        "ByteString.kt",
        "actual open class ByteString\ninternal actual constructor(\n  internal actual val data: ByteArray,\n) : Comparable<ByteString> {\n  actual open fun hex(): String = commonHex()\n  actual companion object {\n    actual fun of(vararg data: Byte): ByteString = TODO()\n  }\n}\n",
        &["ByteString", "hex"],
    );
}

#[test]
fn elixir_typespecs() {
    has(
        "endpoint.ex",
        "defmodule Phoenix.Endpoint do\n  @type topic :: String.t()\n  @typep event(a) :: a\nend\n",
        &["topic", "event"],
    );
}
