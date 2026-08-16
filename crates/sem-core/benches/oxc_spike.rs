//! Spike: does an oxc-based typed-AST walk beat tree-sitter's on the thing
//! that actually matters — parse *and* the entity walk together?
//!
//! `cargo bench -p sem-core --bench oxc_spike --features oxc-fastpath`
//!
//! See `OXC-FASTPATH.md` for the measured numbers and the go/no-go verdict.
//! This bench does **not** pin equivalence — the oxc walk here is a
//! reasonable-effort counterpart (named functions, classes, methods,
//! interfaces, type aliases, enums, const-assigned functions/arrows), not a
//! field-identical reproduction of `entity_extractor.rs`. Its only job is to
//! answer: is the typed-AST walk fast enough to be worth building the real
//! thing? Equivalence, if pursued, is a separate, later pass.

mod common;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use sem_core::parser::plugins::code::{language_config_for_content, parse_tree};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Class, Expression, Function, MethodDefinition, TSEnumDeclaration, TSInterfaceDeclaration,
    TSTypeAliasDeclaration, VariableDeclarator,
};
use oxc_ast_visit::Visit;
use oxc_parser::Parser as OxcParser;
use oxc_span::SourceType;
use oxc_syntax::scope::ScopeFlags;

/// A reasonable-effort entity walk over oxc's typed AST. Not pinned to
/// `entity_extractor.rs` output — see the module doc comment.
struct Collector {
    count: usize,
}

impl<'a> Visit<'a> for Collector {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        if it.id.is_some() {
            self.count += 1;
        }
        oxc_ast_visit::walk::walk_function(self, it, flags);
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        self.count += 1;
        oxc_ast_visit::walk::walk_class(self, it);
    }

    fn visit_method_definition(&mut self, it: &MethodDefinition<'a>) {
        self.count += 1;
        oxc_ast_visit::walk::walk_method_definition(self, it);
    }

    fn visit_ts_interface_declaration(&mut self, it: &TSInterfaceDeclaration<'a>) {
        self.count += 1;
        oxc_ast_visit::walk::walk_ts_interface_declaration(self, it);
    }

    fn visit_ts_type_alias_declaration(&mut self, it: &TSTypeAliasDeclaration<'a>) {
        self.count += 1;
        oxc_ast_visit::walk::walk_ts_type_alias_declaration(self, it);
    }

    fn visit_ts_enum_declaration(&mut self, it: &TSEnumDeclaration<'a>) {
        self.count += 1;
        oxc_ast_visit::walk::walk_ts_enum_declaration(self, it);
    }

    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        // `const foo = () => {}` / `const foo = function() {}`: tree-sitter's
        // walk extracts these as named entities too.
        if matches!(
            it.init,
            Some(Expression::ArrowFunctionExpression(_)) | Some(Expression::FunctionExpression(_))
        ) {
            self.count += 1;
        }
        oxc_ast_visit::walk::walk_variable_declarator(self, it);
    }
}

fn oxc_parse_and_walk(source: &str, source_type: SourceType) -> usize {
    let allocator = Allocator::default();
    let ret = OxcParser::new(&allocator, source, source_type).parse();
    let mut collector = Collector { count: 0 };
    collector.visit_program(&ret.program);
    collector.count
}

struct RealFile {
    id: String,
    path: String,
    source: String,
}

/// Large real TS files from a local microsoft/TypeScript checkout, if the
/// librarian cache happens to have one. Skipped (not failed) when absent.
fn real_files() -> Vec<RealFile> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let root =
        Path::new(&home).join(".cache/checkouts/github.com/microsoft/TypeScript/src/compiler");
    let candidates = ["checker.ts", "parser.ts", "utilities.ts", "types.ts"];
    candidates
        .iter()
        .filter_map(|name| {
            let p = root.join(name);
            let source = std::fs::read_to_string(&p).ok()?;
            Some(RealFile {
                id: name.trim_end_matches(".ts").to_string(),
                path: p.to_string_lossy().to_string(),
                source,
            })
        })
        .collect()
}

/// The four fixed TS fixtures already checked in for the cache-equivalence
/// tests, plus the criterion-standard small/medium/large synthetic TS
/// fixtures from `benches/common`.
fn small_fixtures() -> Vec<RealFile> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let checked_in = [
        "tests/fixtures/scope_test/typescript/models.ts",
        "tests/fixtures/scope_test/typescript/service.ts",
        "tests/fixtures/scope_test/typescript/database.ts",
        "tests/fixtures/scope_test/typescript/handlers.ts",
    ];
    let mut out: Vec<RealFile> = checked_in
        .iter()
        .filter_map(|rel| {
            let source = std::fs::read_to_string(root.join(rel)).ok()?;
            Some(RealFile {
                id: rel.to_string(),
                path: rel.to_string(),
                source,
            })
        })
        .collect();

    for (size, fixture) in common::all() {
        if fixture.id == "typescript" {
            out.push(RealFile {
                id: format!("synthetic-{size}"),
                path: fixture.path.clone(),
                source: fixture.source,
            });
        }
    }
    out
}

fn source_type_for(path: &str) -> SourceType {
    SourceType::from_path(Path::new(path)).unwrap_or_default()
}

fn bench_group(c: &mut Criterion, group_name: &str, files: &[RealFile]) {
    let mut group = c.benchmark_group(group_name);
    for file in files {
        let config = language_config_for_content(&file.source, &file.path)
            .unwrap_or_else(|| panic!("no tree-sitter language config for {}", file.path));
        let source_type = source_type_for(&file.path);

        let ts_entities = sem_core::parser::plugins::code::extract_entities_from_tree(
            &parse_tree(config, &file.source).expect("tree-sitter parse failed"),
            &file.path,
            config,
            &file.source,
        )
        .len();
        let oxc_entities = oxc_parse_and_walk(&file.source, source_type);
        eprintln!(
            "{group_name}/{}: {} bytes, tree-sitter entities={ts_entities}, oxc entities~={oxc_entities}",
            file.id,
            file.source.len(),
        );

        group.throughput(Throughput::Bytes(file.source.len() as u64));

        group.bench_with_input(BenchmarkId::new("ts_parse", &file.id), file, |b, f| {
            b.iter(|| black_box(parse_tree(config, &f.source)));
        });

        group.bench_with_input(
            BenchmarkId::new("ts_parse_and_walk", &file.id),
            file,
            |b, f| {
                b.iter(|| {
                    let tree = parse_tree(config, &f.source).expect("parse");
                    black_box(sem_core::parser::plugins::code::extract_entities_from_tree(
                        &tree, &f.path, config, &f.source,
                    ))
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("oxc_parse", &file.id), file, |b, f| {
            b.iter(|| {
                let allocator = Allocator::default();
                black_box(OxcParser::new(&allocator, &f.source, source_type).parse());
            });
        });

        group.bench_with_input(
            BenchmarkId::new("oxc_parse_and_walk", &file.id),
            file,
            |b, f| {
                b.iter(|| black_box(oxc_parse_and_walk(&f.source, source_type)));
            },
        );
    }
    group.finish();
}

fn bench_small(c: &mut Criterion) {
    bench_group(c, "oxc_spike_small", &small_fixtures());
}

fn bench_large_real(c: &mut Criterion) {
    let files = real_files();
    if files.is_empty() {
        eprintln!(
            "oxc_spike_large_real: no microsoft/TypeScript checkout at \
             ~/.cache/checkouts/github.com/microsoft/TypeScript — skipping. \
             See OXC-FASTPATH.md."
        );
        return;
    }
    bench_group(c, "oxc_spike_large_real", &files);
}

fn large_real_config() -> Criterion {
    // checker.ts is 3+ MB; the default 100-sample/5s-measurement config would
    // make this bench take many minutes. Trade sample count for a bench that
    // finishes in a spike's worth of time, same tradeoff INCREMENTAL-PARSE.md
    // documents for its own large fixtures.
    Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
}

criterion_group!(small_benches, bench_small);
criterion_group!(name = large_benches; config = large_real_config(); targets = bench_large_real);
criterion_main!(small_benches, large_benches);
