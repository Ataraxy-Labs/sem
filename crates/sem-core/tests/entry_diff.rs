//! `sem diff --entries`: keys of container literals diffed as entries.
//! Fixtures are shaped like the real code they stand for: sqlglot's parser and
//! generator registries (Python), a JS/TS command registry, expr's builtin
//! table and a Go map literal.

use sem_core::git::types::{FileChange, FileStatus};
use sem_core::model::change::{ChangeType, EntryRef, SemanticChange};
use sem_core::parser::differ::{
    compute_semantic_diff, compute_semantic_diff_with_options, DiffResult, SemanticDiffOptions,
};
use sem_core::parser::plugins::create_default_registry;

fn modified(path: &str, before: &str, after: &str) -> FileChange {
    FileChange {
        file_path: path.to_string(),
        status: FileStatus::Modified,
        old_file_path: None,
        before_content: Some(before.to_string()),
        after_content: Some(after.to_string()),
    }
}

fn diff(path: &str, before: &str, after: &str) -> DiffResult {
    compute_semantic_diff_with_options(
        &[modified(path, before, after)],
        &create_default_registry(),
        None,
        None,
        SemanticDiffOptions { entries: true },
    )
}

fn summary(r: &DiffResult) -> Vec<String> {
    let mut v: Vec<String> = r
        .changes
        .iter()
        .map(|c| format!("{} {} {}", c.change_type, c.entity_type, c.entity_name))
        .collect();
    v.sort();
    v
}

fn entry(c: &SemanticChange) -> &EntryRef {
    c.entry.as_ref().expect("entry change carries its container and key")
}

const CLICKHOUSE_PARSER: &str = r#"from __future__ import annotations

import typing as t

from sqlglot import exp, parser
from sqlglot.helper import seq_get


def _build_count_if(args, dialect):
    if len(args) == 1:
        return exp.CountIf(this=seq_get(args, 0))
    return exp.CombinedAggFunc(this="countIf", expressions=args)


class ClickHouseParser(parser.Parser):
    MODIFIERS_ATTACHED_TO_SET_OP = False
    INTERVAL_SPANS = False

    FUNCTIONS: t.ClassVar[dict[str, t.Callable]] = {
        **{k: v for k, v in parser.Parser.FUNCTIONS.items() if k not in ("TRANSFORM",)},
        **parser.Parser.FUNCTIONS,
        "ANY": exp.AnyValue.from_arg_list,
        "ARRAYSUM": exp.ArraySum.from_arg_list,
        "COUNTIF": _build_count_if,
        "DATE_ADD": lambda args, dialect: exp.DateAdd(
            this=seq_get(args, 2), expression=seq_get(args, 1), unit=seq_get(args, 0)
        ),
    }

    def _parse_types(self, check_func=False, schema=False, allow_identifiers=True):
        return super()._parse_types(check_func=check_func, schema=schema)
"#;

#[test]
fn python_added_key_is_one_entry_not_the_class() {
    let after = CLICKHOUSE_PARSER.replace(
        "        \"COUNTIF\": _build_count_if,\n",
        "        \"COUNTIF\": _build_count_if,\n        \"TOUNIXTIMESTAMP\": exp.TimeToUnix.from_arg_list,\n",
    );
    let r = diff("sqlglot/parsers/clickhouse.py", CLICKHOUSE_PARSER, &after);
    assert_eq!(summary(&r), vec![r#"added entry FUNCTIONS["TOUNIXTIMESTAMP"]"#]);
    let e = entry(&r.changes[0]);
    assert_eq!(e.container, "ClickHouseParser.FUNCTIONS");
    assert_eq!(e.key, "TOUNIXTIMESTAMP");
    assert_eq!(e.key_kind, "string");
    assert_eq!(
        r.changes[0].entity_id,
        r#"sqlglot/parsers/clickhouse.py::class::ClickHouseParser::FUNCTIONS::FUNCTIONS["TOUNIXTIMESTAMP"]"#
    );
}

#[test]
fn without_the_flag_the_class_is_what_changed() {
    let after = CLICKHOUSE_PARSER.replace(
        "        \"COUNTIF\": _build_count_if,\n",
        "        \"COUNTIF\": _build_count_if,\n        \"TOUNIXTIMESTAMP\": exp.TimeToUnix.from_arg_list,\n",
    );
    let r = compute_semantic_diff(
        &[modified("sqlglot/parsers/clickhouse.py", CLICKHOUSE_PARSER, &after)],
        &create_default_registry(),
        None,
        None,
    );
    assert_eq!(summary(&r), vec!["modified class ClickHouseParser"]);
    assert!(r.changes.iter().all(|c| c.entry.is_none()));
}

#[test]
fn python_modified_and_deleted_entries_and_a_method() {
    let after = CLICKHOUSE_PARSER
        .replace("\"ANY\": exp.AnyValue.from_arg_list,\n", "")
        .replace("unit=seq_get(args, 0)", "unit=exp.var(seq_get(args, 0))")
        .replace("return super()._parse_types(check_func=check_func, schema=schema)",
                 "return super()._parse_types(check_func=check_func, schema=schema, allow_identifiers=allow_identifiers)");
    let r = diff("sqlglot/parsers/clickhouse.py", CLICKHOUSE_PARSER, &after);
    assert_eq!(
        summary(&r),
        vec![
            r#"deleted entry FUNCTIONS["ANY"]"#,
            r#"modified entry FUNCTIONS["DATE_ADD"]"#,
            "modified function _parse_types",
        ]
    );
}

#[test]
fn python_spread_change_is_reported_as_an_entry() {
    let after = CLICKHOUSE_PARSER.replace("**parser.Parser.FUNCTIONS,", "**parser.BaseParser.FUNCTIONS,");
    let r = diff("sqlglot/parsers/clickhouse.py", CLICKHOUSE_PARSER, &after);
    assert_eq!(
        summary(&r),
        vec![
            "added entry FUNCTIONS[**parser.BaseParser.FUNCTIONS]",
            "deleted entry FUNCTIONS[**parser.Parser.FUNCTIONS]",
        ]
    );
    let kinds: Vec<&str> = r.changes.iter().map(|c| entry(c).key_kind.as_str()).collect();
    assert_eq!(kinds, vec!["spread", "spread"]);
}

#[test]
fn python_class_flag_is_a_binding_change() {
    let after = CLICKHOUSE_PARSER.replace("INTERVAL_SPANS = False", "INTERVAL_SPANS = True");
    let r = diff("sqlglot/parsers/clickhouse.py", CLICKHOUSE_PARSER, &after);
    assert_eq!(summary(&r), vec!["modified variable INTERVAL_SPANS"]);
    assert!(r.changes[0].entry.is_none());
}

#[test]
fn python_rekeyed_entry_is_deleted_old_plus_added_new() {
    let after = CLICKHOUSE_PARSER.replace("\"ARRAYSUM\":", "\"ARRAY_SUM\":");
    let r = diff("sqlglot/parsers/clickhouse.py", CLICKHOUSE_PARSER, &after);
    assert_eq!(
        summary(&r),
        vec![r#"added entry FUNCTIONS["ARRAY_SUM"]"#, r#"deleted entry FUNCTIONS["ARRAYSUM"]"#]
    );
}

const PRESTO_GENERATOR: &str = r#"from sqlglot import exp, generator, transforms
from sqlglot.dialects.dialect import rename_func


class PrestoGenerator(generator.Generator):
    TRANSFORMS = {
        **generator.Generator.TRANSFORMS,
        exp.AnyValue: rename_func("ARBITRARY"),
        exp.ArrayContains: rename_func("CONTAINS"),
        exp.Select: transforms.preprocess([transforms.eliminate_qualify]),
    }

    TYPE_MAPPING = {
        **generator.Generator.TYPE_MAPPING,
        exp.DataType.Type.BINARY: "VARBINARY",
    }
"#;

#[test]
fn python_class_and_enum_keys() {
    let after = PRESTO_GENERATOR
        .replace(
            "        exp.ArrayContains: rename_func(\"CONTAINS\"),\n",
            "        exp.ArrayContains: rename_func(\"CONTAINS\"),\n        exp.ArrayReverse: rename_func(\"REVERSE\"),\n",
        )
        .replace("\"VARBINARY\"", "\"VARBINARY(MAX)\"");
    let r = diff("sqlglot/generators/presto.py", PRESTO_GENERATOR, &after);
    assert_eq!(
        summary(&r),
        vec![
            "added entry TRANSFORMS[exp.ArrayReverse]",
            "modified entry TYPE_MAPPING[exp.DataType.Type.BINARY]",
        ]
    );
    for c in &r.changes {
        assert_eq!(entry(c).key_kind, "attribute");
    }
}

#[test]
fn python_module_level_dict() {
    let before = "from sqlglot import exp\n\nEXPRESSION_METADATA = {\n    exp.Abs: {\"returns\": 1},\n}\n";
    let after = "from sqlglot import exp\n\nEXPRESSION_METADATA = {\n    exp.Abs: {\"returns\": 1},\n    exp.Gcd: {\"returns\": 2},\n}\n";
    let r = diff("sqlglot/typing/__init__.py", before, after);
    assert_eq!(summary(&r), vec!["added entry EXPRESSION_METADATA[exp.Gcd]"]);
    assert_eq!(entry(&r.changes[0]).container, "EXPRESSION_METADATA");
}

const COMMANDS_TS: &str = r#"import { base } from "./base";
import { Kind } from "./kind";

export const COMMANDS = {
  ...base,
  build: { run: "tsc", watch: false },
  "lint-fix": { run: "eslint --fix" },
  [Kind.Test]: { run: "vitest" },
} as const;

export class Registry {
  static DEFAULTS = { retries: 1, timeout: 30 };
}
"#;

#[test]
fn typescript_object_literal_entries() {
    let after = COMMANDS_TS
        .replace("  \"lint-fix\": { run: \"eslint --fix\" },\n", "  \"lint-fix\": { run: \"eslint --fix\" },\n  format: { run: \"prettier -w .\" },\n")
        .replace("watch: false", "watch: true")
        .replace("timeout: 30", "timeout: 60");
    let r = diff("src/commands.ts", COMMANDS_TS, &after);
    assert_eq!(
        summary(&r),
        vec![
            "added entry COMMANDS[format]",
            "modified entry COMMANDS[build]",
            "modified entry DEFAULTS[timeout]",
        ]
    );
    let defaults = r.changes.iter().find(|c| c.entity_name == "DEFAULTS[timeout]").unwrap();
    assert_eq!(entry(defaults).container, "Registry.DEFAULTS");
}

#[test]
fn javascript_object_literal_entries() {
    let before = "const handlers = {\n  ping: 'pong',\n  'x-y': 2,\n};\nmodule.exports = handlers;\n";
    let after = "const handlers = {\n  ping: 'pong',\n  'x-y': 3,\n  extra,\n};\nmodule.exports = handlers;\n";
    let r = diff("lib/handlers.js", before, after);
    assert_eq!(summary(&r), vec!["added entry handlers[extra]", r#"modified entry handlers["x-y"]"#]);
}

const EXPR_BUILTINS: &str = r#"package builtin

import (
	"fmt"
	"reflect"
)

var Builtins = []*Function{
	{
		Name: "all",
		Predicate: true,
		Types: types(new(func([]any, func(any) bool) bool)),
	},
	{
		Name: "abs",
		Fast: Abs,
		Validate: func(args []reflect.Type) (reflect.Type, error) {
			if len(args) != 1 {
				return anyType, fmt.Errorf("invalid number of arguments (expected 1, got %d)", len(args))
			}
			return args[0], nil
		},
	},
	{
		Name: "len",
		Fast: Len,
	},
}

var Index = map[string]int{
	"all": 0,
	"abs": 1,
}
"#;

#[test]
fn go_slice_of_structs_keyed_by_name_field() {
    let after = EXPR_BUILTINS
        .replace(
            "\t{\n\t\tName: \"len\",\n\t\tFast: Len,\n\t},\n",
            "\t{\n\t\tName: \"len\",\n\t\tFast: Len2,\n\t},\n\t{\n\t\tName: \"ceil\",\n\t\tFast: Ceil,\n\t},\n",
        )
        .replace("\t\"abs\": 1,\n", "\t\"abs\": 1,\n\t\"ceil\": 2,\n");
    let r = diff("builtin/builtin.go", EXPR_BUILTINS, &after);
    assert_eq!(
        summary(&r),
        vec![
            r#"added entry Builtins["ceil"]"#,
            r#"added entry Index["ceil"]"#,
            r#"modified entry Builtins["len"]"#,
        ]
    );
    let ceil = r.changes.iter().find(|c| c.entity_name == r#"Builtins["ceil"]"#).unwrap();
    assert_eq!(entry(ceil).key_kind, "name_field");
    assert_eq!(entry(ceil).key, "ceil");
    assert_eq!(ceil.change_type, ChangeType::Added);
}

#[test]
fn go_struct_literal_fields() {
    let before = "package cfg\n\nvar Default = Config{\n\tPort: 8080,\n\tHost: \"localhost\",\n}\n";
    let after = "package cfg\n\nvar Default = Config{\n\tPort: 9090,\n\tHost: \"localhost\",\n}\n";
    let r = diff("cfg/cfg.go", before, after);
    assert_eq!(summary(&r), vec!["modified entry Default[Port]"]);
    assert_eq!(entry(&r.changes[0]).key_kind, "field");
}

#[test]
fn unchanged_literal_reports_nothing_and_json_schema_is_unchanged_without_entries() {
    let r = diff("sqlglot/parsers/clickhouse.py", CLICKHOUSE_PARSER, CLICKHOUSE_PARSER);
    assert!(r.changes.is_empty());
    // A non-entry change serializes without an `entry` field.
    let after = CLICKHOUSE_PARSER.replace("INTERVAL_SPANS = False", "INTERVAL_SPANS = True");
    let r = diff("sqlglot/parsers/clickhouse.py", CLICKHOUSE_PARSER, &after);
    let json = sem_core::format::json::format_diff_json(&r);
    assert!(!json.contains("\"entry\""), "{json}");
}

const SPARK_GENERATOR: &str = r#"from sqlglot import exp
from sqlglot.generators.spark2 import Spark2Generator


class SparkGenerator(Spark2Generator):
    TRANSFORMS = {
        k: v
        for k, v in {
            **Spark2Generator.TRANSFORMS,
            exp.ArrayAppend: array_append_sql("ARRAY_APPEND"),
            exp.BitwiseAndAgg: rename_func("BIT_AND"),
        }.items()
        if k not in (exp.ArraySum,)
    }
"#;

#[test]
fn python_filtered_literal_entries_and_its_filter() {
    let after = SPARK_GENERATOR.replace(
        "            exp.BitwiseAndAgg: rename_func(\"BIT_AND\"),\n",
        "            exp.ArrayReverse: rename_func(\"REVERSE\"),\n            exp.BitwiseAndAgg: rename_func(\"BIT_AND\"),\n",
    );
    let r = diff("sqlglot/generators/spark.py", SPARK_GENERATOR, &after);
    assert_eq!(summary(&r), vec!["added entry TRANSFORMS[exp.ArrayReverse]"]);
    assert_eq!(entry(&r.changes[0]).container, "SparkGenerator.TRANSFORMS");

    // A changed filter is not an entry: the binding itself changed.
    let filtered = SPARK_GENERATOR.replace("(exp.ArraySum,)", "(exp.ArraySum, exp.ArrayAppend)");
    let r = diff("sqlglot/generators/spark.py", SPARK_GENERATOR, &filtered);
    assert_eq!(summary(&r), vec!["modified variable TRANSFORMS"]);
}
