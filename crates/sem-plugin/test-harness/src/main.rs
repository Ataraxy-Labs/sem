use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use lix_engine::wasm::{WasmComponentInstance, WasmLimits, WasmRuntime};
use lix_engine::LixError;
use lix_rs_sdk::{open_lix, OpenLixOptions, RegisterPluginOptions, Value};
use serde::{Deserialize, Serialize};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{IoView, WasiCtx, WasiCtxBuilder, WasiView};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

// Generate host-side bindings from WIT for wasmtime
wasmtime::component::bindgen!({
    path: "../wit/lix-plugin.wit",
    world: "plugin",
});

// ---------------------------------------------------------------------------
// Wasmtime runtime implementation (mirrors lix engine test support)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SemWasmtimeRuntime {
    engine: Engine,
}

impl SemWasmtimeRuntime {
    fn new() -> Self {
        Self {
            engine: Engine::default(),
        }
    }
}

#[async_trait]
impl WasmRuntime for SemWasmtimeRuntime {
    async fn init_component(
        &self,
        bytes: Vec<u8>,
        _limits: WasmLimits,
    ) -> Result<Arc<dyn WasmComponentInstance>, LixError> {
        let component = Component::from_binary(&self.engine, &bytes).map_err(lix_err)?;
        let mut linker = Linker::<PluginHostState>::new(&self.engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker).map_err(lix_err)?;
        let mut store = Store::new(&self.engine, PluginHostState::default());
        let bindings =
            Plugin::instantiate(&mut store, &component, &linker).map_err(lix_err)?;
        Ok(Arc::new(WasmtimePluginInstance {
            inner: Mutex::new(WasmtimeInner { store, bindings }),
        }))
    }
}

struct WasmtimePluginInstance {
    inner: Mutex<WasmtimeInner>,
}

struct WasmtimeInner {
    store: Store<PluginHostState>,
    bindings: Plugin,
}

#[async_trait]
impl WasmComponentInstance for WasmtimePluginInstance {
    async fn call(&self, export: &str, input: &[u8]) -> Result<Vec<u8>, LixError> {
        let mut guard = self.inner.lock().map_err(|_| LixError {
            code: LixError::CODE_UNKNOWN.to_string(),
            message: "lock poisoned".into(),
            hint: None,
            details: None,
        })?;

        match export {
            "detect-changes" | "api#detect-changes" => {
                let input: DetectChangesInput =
                    serde_json::from_slice(input).map_err(|e| lix_err(e))?;

                let before = input.before.map(to_component_file);
                let after = to_component_file(input.after);
                let state_ctx = input.state_context.map(to_component_state_context);

                let WasmtimeInner { ref mut store, ref bindings } = *guard;
                let result = bindings
                    .lix_plugin_api()
                    .call_detect_changes(
                        store,
                        before.as_ref(),
                        &after,
                        state_ctx.as_ref(),
                    )
                    .map_err(lix_err)?;

                match result {
                    Ok(changes) => {
                        let output: Vec<EntityChangeOutput> =
                            changes.into_iter().map(from_component_change).collect();
                        serde_json::to_vec(&output).map_err(|e| lix_err(e))
                    }
                    Err(e) => Err(plugin_err(e)),
                }
            }
            "apply-changes" | "api#apply-changes" => {
                let input: ApplyChangesInput =
                    serde_json::from_slice(input).map_err(|e| lix_err(e))?;

                let file = to_component_file(input.file);
                let changes: Vec<_> = input
                    .changes
                    .into_iter()
                    .map(to_component_change)
                    .collect();

                let WasmtimeInner { ref mut store, ref bindings } = *guard;
                let result = bindings
                    .lix_plugin_api()
                    .call_apply_changes(store, &file, &changes)
                    .map_err(lix_err)?;

                match result {
                    Ok(data) => serde_json::to_vec(&data).map_err(|e| lix_err(e)),
                    Err(e) => Err(plugin_err(e)),
                }
            }
            other => Err(LixError {
                code: LixError::CODE_UNKNOWN.to_string(),
                message: format!("unknown export '{other}'"),
                hint: None,
                details: None,
            }),
        }
    }
}

struct PluginHostState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl Default for PluginHostState {
    fn default() -> Self {
        Self {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
        }
    }
}

impl IoView for PluginHostState {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl WasiView for PluginHostState {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.ctx
    }
}

// ---------------------------------------------------------------------------
// JSON serialization types (engine communicates with plugins via JSON)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DetectChangesInput {
    before: Option<FileInput>,
    after: FileInput,
    #[serde(default)]
    state_context: Option<DetectStateContextInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApplyChangesInput {
    file: FileInput,
    changes: Vec<EntityChangeOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileInput {
    id: String,
    path: String,
    data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DetectStateContextInput {
    #[serde(default)]
    active_state: Option<Vec<ActiveStateRowInput>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveStateRowInput {
    entity_id: String,
    schema_key: Option<String>,
    snapshot_content: Option<String>,
    file_id: Option<String>,
    plugin_key: Option<String>,
    version_id: Option<String>,
    change_id: Option<String>,
    metadata: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EntityChangeOutput {
    entity_id: String,
    schema_key: String,
    snapshot_content: Option<String>,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn to_component_file(f: FileInput) -> exports::lix::plugin::api::File {
    exports::lix::plugin::api::File {
        id: f.id,
        path: f.path,
        data: f.data,
    }
}

fn to_component_state_context(
    sc: DetectStateContextInput,
) -> exports::lix::plugin::api::DetectStateContext {
    exports::lix::plugin::api::DetectStateContext {
        active_state: sc.active_state.map(|rows| {
            rows.into_iter()
                .map(|r| exports::lix::plugin::api::ActiveStateRow {
                    entity_id: r.entity_id,
                    schema_key: r.schema_key,
                    snapshot_content: r.snapshot_content,
                    file_id: r.file_id,
                    plugin_key: r.plugin_key,
                    version_id: r.version_id,
                    change_id: r.change_id,
                    metadata: r.metadata,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                })
                .collect()
        }),
    }
}

fn to_component_change(c: EntityChangeOutput) -> exports::lix::plugin::api::EntityChange {
    exports::lix::plugin::api::EntityChange {
        entity_id: c.entity_id,
        schema_key: c.schema_key,
        snapshot_content: c.snapshot_content,
    }
}

fn from_component_change(c: exports::lix::plugin::api::EntityChange) -> EntityChangeOutput {
    EntityChangeOutput {
        entity_id: c.entity_id,
        schema_key: c.schema_key,
        snapshot_content: c.snapshot_content,
    }
}

fn lix_err(e: impl std::fmt::Display) -> LixError {
    LixError {
        code: LixError::CODE_UNKNOWN.to_string(),
        message: format!("{e}"),
        hint: None,
        details: None,
    }
}

fn plugin_err(e: exports::lix::plugin::api::PluginError) -> LixError {
    let msg = match e {
        exports::lix::plugin::api::PluginError::InvalidInput(s) => {
            format!("plugin invalid input: {s}")
        }
        exports::lix::plugin::api::PluginError::Internal(s) => {
            format!("plugin internal error: {s}")
        }
    };
    LixError {
        code: LixError::CODE_UNKNOWN.to_string(),
        message: msg,
        hint: None,
        details: None,
    }
}

// ---------------------------------------------------------------------------
// Build .lixplugin archive
// ---------------------------------------------------------------------------

fn build_lixplugin_archive(wasm_path: &str) -> Result<Vec<u8>> {
    let wasm_bytes = std::fs::read(wasm_path)
        .with_context(|| format!("Failed to read WASM: {wasm_path}"))?;
    println!("WASM size: {:.1} MB", wasm_bytes.len() as f64 / 1_048_576.0);

    let manifest = serde_json::json!({
        "key": "sem-semantic-diff",
        "runtime": "wasm-component-v1",
        "api_version": "0.1.0",
        "match": {
            "path_glob": "*.{ts,tsx,js,jsx,py,go,rs,java,rb,c,cpp,cs,php,kt,swift,ex,exs,sh,bash,tf,hcl,scala,zig,nix,dart,pl,ml,mli,svelte,vue}",
            "content_type": "text"
        },
        "entry": "plugin.wasm",
        "schemas": ["schema/sem_entity.json"]
    });

    let schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "x-lix-key": "sem_entity",
        "x-lix-primary-key": ["/id"],
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Unique entity identifier" },
            "entity_type": { "type": "string", "description": "Type of semantic entity" },
            "entity_name": { "type": "string", "description": "Name of the entity" },
            "file_path": { "type": "string", "description": "Relative file path" },
            "line": { "type": "integer", "description": "Start line (1-indexed)" },
            "content": { "type": ["string", "null"], "description": "Source content" }
        },
        "required": ["id", "entity_type", "entity_name", "file_path", "line"],
        "additionalProperties": false
    });

    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut cursor);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

        zip.start_file("manifest.json", opts)?;
        zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;

        zip.start_file("plugin.wasm", opts)?;
        zip.write_all(&wasm_bytes)?;

        zip.start_file("schema/sem_entity.json", opts)?;
        zip.write_all(serde_json::to_string_pretty(&schema)?.as_bytes())?;

        zip.finish()?;
    }
    let archive = cursor.into_inner();
    println!("Plugin archive: {:.1} MB", archive.len() as f64 / 1_048_576.0);
    Ok(archive)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let wasm_path = std::env::args().nth(1).unwrap_or_else(|| {
        let dir = env!("CARGO_MANIFEST_DIR");
        format!("{dir}/../../target/wasm32-wasip1/release-wasm/sem_plugin.wasm")
    });

    println!("=== Lix + sem-plugin integration test ===\n");

    let total_start = std::time::Instant::now();
    let mut timings: Vec<(&str, std::time::Duration)> = Vec::new();

    // 1. Build .lixplugin archive
    print!("1. Building .lixplugin archive... ");
    let t = std::time::Instant::now();
    let archive = build_lixplugin_archive(&wasm_path)?;
    let d = t.elapsed();
    println!("{d:?}");
    timings.push(("Build archive", d));

    // 2. Open Lix with wasmtime runtime
    print!("2. Opening Lix instance... ");
    let t = std::time::Instant::now();
    let runtime = Arc::new(SemWasmtimeRuntime::new());
    let lix = open_lix(OpenLixOptions {
        backend: None,
        wasm_runtime: Some(runtime),
    })
    .await
    .context("Failed to open Lix")?;
    let d = t.elapsed();
    println!("{d:?}");
    timings.push(("Open Lix", d));

    // 3. Register sem plugin (includes WASM compilation)
    print!("3. Registering sem-plugin... ");
    let t = std::time::Instant::now();
    let receipt = lix
        .register_plugin(RegisterPluginOptions {
            bytes: archive,
        })
        .await
        .context("Failed to register plugin")?;
    let d = t.elapsed();
    println!("{} ({d:?})", receipt.plugin_key);
    timings.push(("Register plugin", d));

    // 4. Write TypeScript file (cold start — first detect-changes call)
    let ts_code = r#"
export function greet(name: string): string {
    return `Hello, ${name}!`;
}

export class UserService {
    private users: Map<string, User> = new Map();

    async getUser(id: string): Promise<User | null> {
        return this.users.get(id) ?? null;
    }

    async createUser(name: string): Promise<User> {
        const user = { id: crypto.randomUUID(), name };
        this.users.set(user.id, user);
        return user;
    }
}

interface User {
    id: string;
    name: string;
}
"#;

    print!("4. Write TypeScript file (cold start)... ");
    let t = std::time::Instant::now();
    lix.execute(
        "INSERT INTO lix_file (id, path, data) VALUES ('ts-file-1', '/src/services/user.ts', $1)",
        &[Value::Blob(ts_code.as_bytes().to_vec())],
    )
    .await
    .context("Failed to write file")?;
    let d = t.elapsed();
    let result = lix
        .execute(
            "SELECT id, entity_type, entity_name FROM sem_entity WHERE lixcol_file_id = 'ts-file-1'",
            &[],
        )
        .await?;
    println!("{} entities ({d:?})", result.len());
    for row in result.rows() {
        let v = row.values();
        println!("     {:?} {:?}", v[1], v[2]);
    }
    timings.push(("TypeScript new file (cold)", d));

    // 5. Write Python file (warm — parser registry cached)
    let py_code = b"\
class Calculator:
    def __init__(self):
        self.history = []

    def add(self, a: float, b: float) -> float:
        result = a + b
        self.history.append(result)
        return result

    def multiply(self, a: float, b: float) -> float:
        result = a * b
        self.history.append(result)
        return result

def fibonacci(n: int) -> int:
    if n <= 1:
        return n
    return fibonacci(n - 1) + fibonacci(n - 2)
";

    print!("5. Write Python file (warm)... ");
    let t = std::time::Instant::now();
    lix.execute(
        "INSERT INTO lix_file (id, path, data) VALUES ('py-file-1', '/src/calculator.py', $1)",
        &[Value::Blob(py_code.to_vec())],
    )
    .await?;
    let d = t.elapsed();
    let result = lix
        .execute(
            "SELECT id, entity_type, entity_name FROM sem_entity WHERE lixcol_file_id = 'py-file-1'",
            &[],
        )
        .await?;
    println!("{} entities ({d:?})", result.len());
    for row in result.rows() {
        let v = row.values();
        println!("     {:?} {:?}", v[1], v[2]);
    }
    timings.push(("Python new file (warm)", d));

    // 6. Write Rust file
    let rs_code = b"\
pub struct Config {
    pub host: String,
    pub port: u16,
}

impl Config {
    pub fn new() -> Self {
        Self {
            host: \"localhost\".into(),
            port: 8080,
        }
    }

    pub fn from_env() -> Self {
        Self {
            host: std::env::var(\"HOST\").unwrap_or_else(|_| \"0.0.0.0\".into()),
            port: std::env::var(\"PORT\").ok().and_then(|p| p.parse().ok()).unwrap_or(3000),
        }
    }
}

pub trait Service {
    fn name(&self) -> &str;
    fn start(&self) -> Result<(), Box<dyn std::error::Error>>;
}

pub fn start_server(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    println!(\"Starting on {}:{}\", config.host, config.port);
    Ok(())
}
";

    print!("6. Write Rust file (warm)... ");
    let t = std::time::Instant::now();
    lix.execute(
        "INSERT INTO lix_file (id, path, data) VALUES ('rs-file-1', '/src/server.rs', $1)",
        &[Value::Blob(rs_code.to_vec())],
    )
    .await?;
    let d = t.elapsed();
    let result = lix
        .execute(
            "SELECT id, entity_type, entity_name FROM sem_entity WHERE lixcol_file_id = 'rs-file-1'",
            &[],
        )
        .await?;
    println!("{} entities ({d:?})", result.len());
    for row in result.rows() {
        let v = row.values();
        println!("     {:?} {:?}", v[1], v[2]);
    }
    timings.push(("Rust new file (warm)", d));

    // 7. Write Go file
    let go_code = b"\
package main

import (
\t\"fmt\"
\t\"net/http\"
)

type Server struct {
\tHost string
\tPort int
}

func NewServer(host string, port int) *Server {
\treturn &Server{Host: host, Port: port}
}

func (s *Server) Start() error {
\taddr := fmt.Sprintf(\"%s:%d\", s.Host, s.Port)
\treturn http.ListenAndServe(addr, nil)
}

func healthHandler(w http.ResponseWriter, r *http.Request) {
\tw.WriteHeader(http.StatusOK)
\tfmt.Fprintf(w, \"ok\")
}
";

    print!("7. Write Go file (warm)... ");
    let t = std::time::Instant::now();
    lix.execute(
        "INSERT INTO lix_file (id, path, data) VALUES ('go-file-1', '/cmd/server/main.go', $1)",
        &[Value::Blob(go_code.to_vec())],
    )
    .await?;
    let d = t.elapsed();
    let result = lix
        .execute(
            "SELECT id, entity_type, entity_name FROM sem_entity WHERE lixcol_file_id = 'go-file-1'",
            &[],
        )
        .await?;
    println!("{} entities ({d:?})", result.len());
    for row in result.rows() {
        let v = row.values();
        println!("     {:?} {:?}", v[1], v[2]);
    }
    timings.push(("Go new file (warm)", d));

    // 8. Write Java file
    let java_code = b"\
public class UserRepository {
    private final Map<String, User> users = new HashMap<>();

    public User findById(String id) {
        return users.get(id);
    }

    public List<User> findAll() {
        return new ArrayList<>(users.values());
    }

    public User save(User user) {
        users.put(user.getId(), user);
        return user;
    }

    public void deleteById(String id) {
        users.remove(id);
    }
}
";

    print!("8. Write Java file (warm)... ");
    let t = std::time::Instant::now();
    lix.execute(
        "INSERT INTO lix_file (id, path, data) VALUES ('java-file-1', '/src/UserRepository.java', $1)",
        &[Value::Blob(java_code.to_vec())],
    )
    .await?;
    let d = t.elapsed();
    let result = lix
        .execute(
            "SELECT id, entity_type, entity_name FROM sem_entity WHERE lixcol_file_id = 'java-file-1'",
            &[],
        )
        .await?;
    println!("{} entities ({d:?})", result.len());
    for row in result.rows() {
        let v = row.values();
        println!("     {:?} {:?}", v[1], v[2]);
    }
    timings.push(("Java new file (warm)", d));

    // 9. Modify TypeScript file (semantic diff)
    let ts_modified = r#"
export function greet(name: string, greeting: string = "Hello"): string {
    return `${greeting}, ${name}!`;
}

export class UserService {
    private users: Map<string, User> = new Map();

    async getUser(id: string): Promise<User | null> {
        return this.users.get(id) ?? null;
    }

    async createUser(name: string): Promise<User> {
        const user = { id: crypto.randomUUID(), name };
        this.users.set(user.id, user);
        return user;
    }

    async deleteUser(id: string): Promise<void> {
        this.users.delete(id);
    }
}

interface User {
    id: string;
    name: string;
    email?: string;
}
"#;

    print!("9. Update TypeScript file (diff)... ");
    let t = std::time::Instant::now();
    lix.execute(
        "UPDATE lix_file SET data = $1 WHERE id = 'ts-file-1'",
        &[Value::Blob(ts_modified.as_bytes().to_vec())],
    )
    .await?;
    let d = t.elapsed();
    let result = lix
        .execute(
            "SELECT id, entity_type, entity_name FROM sem_entity WHERE lixcol_file_id = 'ts-file-1'",
            &[],
        )
        .await?;
    println!("{} entities ({d:?})", result.len());
    timings.push(("TypeScript update (diff)", d));

    // 10. Write a larger file (~200 lines)
    let large_ts = generate_large_ts(50); // 50 functions ~200 lines
    print!("10. Write large TS file (~{} bytes, 50 functions)... ", large_ts.len());
    let t = std::time::Instant::now();
    lix.execute(
        "INSERT INTO lix_file (id, path, data) VALUES ('ts-large-1', '/src/large-module.ts', $1)",
        &[Value::Blob(large_ts.into_bytes())],
    )
    .await?;
    let d = t.elapsed();
    let result = lix
        .execute(
            "SELECT count(*) FROM sem_entity WHERE lixcol_file_id = 'ts-large-1'",
            &[],
        )
        .await?;
    let count = result.rows()[0].values();
    println!("{:?} entities ({d:?})", count[0]);
    timings.push(("Large TS (50 fns)", d));

    // 11. Query total entity count
    let result = lix
        .execute("SELECT count(*) FROM sem_entity", &[])
        .await?;
    let total = &result.rows()[0].values()[0];
    println!("\nTotal entities in Lix: {:?}", total);

    let total_time = total_start.elapsed();

    // Summary
    println!("\n========================================");
    println!("  PERFORMANCE SUMMARY");
    println!("========================================");
    println!("{:<35} {:>12}", "Phase", "Duration");
    println!("{:-<35} {:->12}", "", "");
    for (label, duration) in &timings {
        println!("{:<35} {:>12?}", label, duration);
    }
    println!("{:-<35} {:->12}", "", "");
    println!("{:<35} {:>12?}", "TOTAL", total_time);
    println!("========================================");

    lix.close().await.map_err(|e| anyhow::anyhow!("{}", e.message))?;

    Ok(())
}

fn generate_large_ts(num_functions: usize) -> String {
    let mut code = String::new();
    for i in 0..num_functions {
        code.push_str(&format!(
            "export function process_{i}(input: string): string {{\n    const trimmed = input.trim();\n    const result = trimmed.toUpperCase();\n    return result;\n}}\n\n"
        ));
    }
    code
}
