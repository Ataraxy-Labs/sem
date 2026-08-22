pub mod code;
pub mod csv_plugin;
#[cfg(feature = "lang-erb")]
pub mod erb;
pub mod fallback;
pub mod json;
pub mod latex;
pub mod markdown;
#[cfg(feature = "lang-svelte")]
pub mod svelte;
pub mod toml_plugin;
pub mod vue;
pub mod yaml;

use crate::parser::registry::ParserRegistry;

pub fn create_default_registry() -> ParserRegistry {
    let mut registry = ParserRegistry::new();

    registry.register(Box::new(json::JsonParserPlugin));
    registry.register(Box::new(code::CodeParserPlugin));
    #[cfg(feature = "lang-svelte")]
    registry.register(Box::new(svelte::SvelteParserPlugin));
    registry.register(Box::new(vue::VueParserPlugin));
    registry.register(Box::new(yaml::YamlParserPlugin));
    registry.register(Box::new(toml_plugin::TomlParserPlugin));
    registry.register(Box::new(csv_plugin::CsvParserPlugin));
    registry.register(Box::new(markdown::MarkdownParserPlugin));
    registry.register(Box::new(latex::LatexParserPlugin));
    #[cfg(feature = "lang-erb")]
    registry.register(Box::new(erb::ErbParserPlugin));
    // Fallback must be last
    registry.register(Box::new(fallback::FallbackParserPlugin));

    registry
}

/// Every plugin's entities must carry a byte span consistent with their line
/// span, so a consumer can slice the exact original bytes out of the file
/// given only `file_path` + `start_byte`/`end_byte` (size estimates,
/// byte-range splicing edits).
///
/// The rule mirrors the tree-sitter code plugin's own convention (see
/// `code::entity_extractor`, which sets `start_byte`/`end_byte` straight from
/// `node.start_byte()`/`node.end_byte()`): `start_byte` is the offset of the
/// first byte of `start_line`; `end_byte` is the offset one past the last
/// byte of `end_line`, EXCLUDING that line's own trailing newline.
/// Equivalently: `content[start_byte..end_byte]` equals the exact text of
/// lines `start_line..=end_line`, joined by `\n`, with no leading or
/// trailing newline.
#[cfg(test)]
mod byte_range_invariants {
    use super::create_default_registry;

    /// `line_starts[n]` is the byte offset of the first byte of 1-based line
    /// `n + 1`; the vector has one extra trailing entry (= `content.len()`)
    /// so `line_starts[end_line]` gives "one past the end of `end_line`,
    /// including its own trailing newline if any".
    fn line_starts(content: &str) -> Vec<usize> {
        let mut starts = vec![0usize];
        let mut pos = 0usize;
        for line in content.split_inclusive('\n') {
            pos += line.len();
            starts.push(pos);
        }
        starts
    }

    /// Expected `[start_byte, end_byte)` for a 1-based inclusive line range,
    /// per the rule stated above.
    fn expected_byte_range(content: &str, start_line: usize, end_line: usize) -> (usize, usize) {
        let starts = line_starts(content);
        let start_byte = starts[start_line - 1];
        let raw_end = starts[end_line];
        let end_byte = content[..raw_end].trim_end_matches(['\n', '\r']).len();
        (start_byte, end_byte)
    }

    struct Fixture {
        plugin: &'static str,
        file_path: &'static str,
        content: &'static str,
    }

    const FIXTURES: &[Fixture] = &[
        // Control: the tree-sitter code plugin already fills byte ranges.
        Fixture {
            plugin: "code",
            file_path: "add.rs",
            content: "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        },
        Fixture {
            plugin: "markdown",
            file_path: "doc.md",
            content: "# Title\n\nbody text\n",
        },
        Fixture {
            plugin: "toml",
            file_path: "Cargo.toml",
            content: "[package]\nname = \"my-app\"\n",
        },
        Fixture {
            plugin: "yaml",
            file_path: "config.yaml",
            content: "name: my-app\nversion: 1.0.0\n",
        },
        Fixture {
            plugin: "json",
            file_path: "package.json",
            content: "{\n  \"name\": \"my-app\"\n}\n",
        },
        Fixture {
            plugin: "csv",
            file_path: "data.csv",
            content: "name,age\nalice,30\n",
        },
        Fixture {
            plugin: "latex",
            file_path: "doc.tex",
            content: "\\newcommand{\\mybox}[1]{%\n  \\fbox{%\n    \\parbox{0.9\\textwidth}{#1}%\n  }%\n}\n\\begin{document}\n\\section{Body}\nText.\n\\end{document}\n",
        },
        Fixture {
            plugin: "vue",
            file_path: "test.vue",
            content: "<template>\n  <div>hi</div>\n</template>\n\n<script lang=\"ts\">\nfunction hello() {\n  return 'hello'\n}\n</script>\n",
        },
        Fixture {
            plugin: "svelte",
            file_path: "Hello.svelte",
            content: "<script lang=\"ts\">\nfunction hello() {\n  return \"hello\";\n}\n</script>\n\n<div>{hello()}</div>\n",
        },
        Fixture {
            plugin: "erb",
            file_path: "views/dashboard.html.erb",
            content: "<div class=\"container\">\n  <% if @user.admin? %>\n    <h1>Admin Panel</h1>\n    <%= @user.name %>\n  <% else %>\n    <p>Access denied</p>\n  <% end %>\n\n  <% @items.each do |item| %>\n    <li><%= item.title %></li>\n  <% end %>\n\n  <%# This is a comment, should be skipped %>\n  <% @count = @items.length %>\n</div>\n",
        },
        Fixture {
            plugin: "fallback",
            file_path: "notes.unknownext",
            content: "line one\nline two\nline three\n",
        },
    ];

    #[test]
    fn every_plugin_emits_byte_ranges_consistent_with_line_ranges() {
        let registry = create_default_registry();
        for fixture in FIXTURES {
            let entities = registry.extract_entities(fixture.file_path, fixture.content);
            assert!(
                !entities.is_empty(),
                "{}: fixture produced no entities",
                fixture.plugin
            );
            for entity in &entities {
                let start_byte = entity.start_byte.unwrap_or_else(|| {
                    panic!(
                        "{}: entity {:?} (lines {}..={}) has no start_byte",
                        fixture.plugin, entity.name, entity.start_line, entity.end_line
                    )
                });
                let end_byte = entity.end_byte.unwrap_or_else(|| {
                    panic!(
                        "{}: entity {:?} (lines {}..={}) has no end_byte",
                        fixture.plugin, entity.name, entity.start_line, entity.end_line
                    )
                });
                assert!(
                    end_byte > start_byte,
                    "{}: entity {:?} has end_byte {} <= start_byte {}",
                    fixture.plugin,
                    entity.name,
                    end_byte,
                    start_byte
                );

                let (expected_start, expected_end) =
                    expected_byte_range(fixture.content, entity.start_line, entity.end_line);
                assert_eq!(
                    (start_byte, end_byte),
                    (expected_start, expected_end),
                    "{}: entity {:?} byte range does not match lines {}..={}",
                    fixture.plugin,
                    entity.name,
                    entity.start_line,
                    entity.end_line
                );

                let sliced = &fixture.content[start_byte..end_byte];
                let expected_text: String = fixture
                    .content
                    .lines()
                    .skip(entity.start_line - 1)
                    .take(entity.end_line - entity.start_line + 1)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert_eq!(
                    sliced, expected_text,
                    "{}: entity {:?} byte slice does not equal the joined text of lines {}..={}",
                    fixture.plugin, entity.name, entity.start_line, entity.end_line
                );
            }
        }
    }
}
