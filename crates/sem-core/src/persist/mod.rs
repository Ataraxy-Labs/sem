//! On-disk persistence that outlives a single process: `cache.db` today
//! (`disk_cache`), alongside the crate's other persistence code
//! (`parser::facts_store`'s CBOR facts layer, `index::{writer,reader}`'s
//! query-index format). Gated behind the `disk-cache` feature — see its
//! doc in `Cargo.toml` for why (`rusqlite`'s `bundled` feature is a C
//! dependency, which the crate otherwise keeps optional for `wasm`).
#[cfg(feature = "disk-cache")]
pub mod disk_cache;
