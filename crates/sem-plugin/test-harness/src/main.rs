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

    // 1. Build .lixplugin archive
    println!("Step 1: Building .lixplugin archive...");
    let archive = build_lixplugin_archive(&wasm_path)?;

    // 2. Open Lix with wasmtime runtime
    println!("\nStep 2: Opening Lix instance...");
    let runtime = Arc::new(SemWasmtimeRuntime::new());
    let lix = open_lix(OpenLixOptions {
        backend: None,
        wasm_runtime: Some(runtime),
    })
    .await
    .context("Failed to open Lix")?;
    println!("Lix instance ready.");

    // 3. Register sem plugin
    println!("\nStep 3: Registering sem-plugin...");
    let start = std::time::Instant::now();
    let receipt = lix
        .register_plugin(RegisterPluginOptions {
            bytes: archive,
        })
        .await
        .context("Failed to register plugin")?;
    println!("Registered: {} ({:?})", receipt.plugin_key, start.elapsed());

    // 4. Write a TypeScript file (triggers detect-changes)
    println!("\nStep 4: Writing TypeScript file...");
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

    let start = std::time::Instant::now();
    lix.execute(
        "INSERT INTO lix_file (id, path, data) VALUES ('ts-file-1', '/src/services/user.ts', $1)",
        &[Value::Blob(ts_code.as_bytes().to_vec())],
    )
    .await
    .context("Failed to write file")?;
    println!("File written and plugin invoked ({:?})", start.elapsed());

    // 5. Query detected entities
    println!("\nStep 5: Querying detected entities...");
    let result = lix
        .execute(
            "SELECT id, entity_type, entity_name, file_path, line, content, lixcol_file_id FROM sem_entity",
            &[],
        )
        .await
        .context("Failed to query entities")?;

    println!("Found {} entities:\n", result.len());
    for row in result.rows() {
        let vals = row.values();
        println!("  id:          {:?}", vals[0]);
        println!("  entity_type: {:?}", vals[1]);
        println!("  entity_name: {:?}", vals[2]);
        println!("  file_path:   {:?}", vals[3]);
        println!("  line:        {:?}", vals[4]);
        println!("  file_id:     {:?}", vals[6]);
        println!();
    }

    // 6. Write a Python file
    println!("Step 6: Writing Python file...");
    let py_code = r#"
class Calculator:
    def add(self, a: float, b: float) -> float:
        return a + b

    def multiply(self, a: float, b: float) -> float:
        return a * b

def fibonacci(n: int) -> int:
    if n <= 1:
        return n
    return fibonacci(n - 1) + fibonacci(n - 2)
"#;

    let start = std::time::Instant::now();
    lix.execute(
        "INSERT INTO lix_file (id, path, data) VALUES ('py-file-1', '/src/calculator.py', $1)",
        &[Value::Blob(py_code.as_bytes().to_vec())],
    )
    .await
    .context("Failed to write Python file")?;
    println!("File written ({:?})", start.elapsed());

    // Query all entities now
    let result = lix
        .execute("SELECT id, entity_type, entity_name FROM sem_entity", &[])
        .await
        .context("Failed to query all entities")?;

    println!("\nAll entities across both files ({} total):", result.len());
    for row in result.rows() {
        let vals = row.values();
        println!("  {:?}  {:?}  {:?}", vals[0], vals[1], vals[2]);
    }

    // 7. Modify the TypeScript file (update triggers diff)
    println!("\nStep 7: Modifying TypeScript file...");
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

    let start = std::time::Instant::now();
    lix.execute(
        "UPDATE lix_file SET data = $1 WHERE id = 'ts-file-1'",
        &[Value::Blob(ts_modified.as_bytes().to_vec())],
    )
    .await
    .context("Failed to update file")?;
    println!("File updated ({:?})", start.elapsed());

    let result = lix
        .execute(
            "SELECT id, entity_type, entity_name FROM sem_entity WHERE lixcol_file_id = 'ts-file-1'",
            &[],
        )
        .await
        .context("Failed to query updated entities")?;

    println!("\nEntities after update ({} total):", result.len());
    for row in result.rows() {
        let vals = row.values();
        println!("  {:?}  {:?}  {:?}", vals[0], vals[1], vals[2]);
    }

    // Cleanup
    println!("\n=== Done ===");
    lix.close().await.map_err(|e| anyhow::anyhow!("{}", e.message))?;

    Ok(())
}
