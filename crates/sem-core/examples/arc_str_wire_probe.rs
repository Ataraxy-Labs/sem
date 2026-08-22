//! Throwaway probe (interning-for-memory wave, semx-taq6): does `Arc<str>`
//! serialize to byte-identical CBOR as `String`, and does an old
//! `String`-shaped payload decode cleanly into an `Arc<str>`-shaped struct?
//! If both hold, per-file `Arc<str>` interning of `AstRefKind`'s identifier
//! fields needs no `FACTS_SCHEMA_VERSION` bump — the wire format is
//! unchanged, only the in-memory representation is. Not wired into any
//! build; delete once its answer is recorded in RESOLUTION-PROFILE.md.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct OldShape {
    name: String,
    receiver: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct NewShape {
    name: Arc<str>,
    receiver: Arc<str>,
}

fn main() {
    let old = OldShape {
        name: "foo".to_string(),
        receiver: "bar".to_string(),
    };
    let new = NewShape {
        name: Arc::from("foo"),
        receiver: Arc::from("bar"),
    };

    let mut old_bytes = Vec::new();
    ciborium::into_writer(&old, &mut old_bytes).unwrap();
    let mut new_bytes = Vec::new();
    ciborium::into_writer(&new, &mut new_bytes).unwrap();

    println!("old_bytes = {old_bytes:?}");
    println!("new_bytes = {new_bytes:?}");
    println!("byte_identical = {}", old_bytes == new_bytes);

    // Cross-decode: old String-shaped bytes -> new Arc<str>-shaped struct.
    let decoded_new: NewShape = ciborium::from_reader(&old_bytes[..]).unwrap();
    println!("cross_decode_old_to_new = {decoded_new:?}");
    assert_eq!(decoded_new, new);

    // And the reverse: new Arc<str>-shaped bytes -> old String-shaped struct.
    let decoded_old: OldShape = ciborium::from_reader(&new_bytes[..]).unwrap();
    println!("cross_decode_new_to_old = {decoded_old:?}");
    assert_eq!(decoded_old, old);

    println!(
        "PROBE VERDICT: wire-compatible = {}",
        old_bytes == new_bytes
    );
}
