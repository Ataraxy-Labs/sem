//! `sem system`: widen the closed world, then measure how much of it is
//! knowable.
//!
//! A repo's code is one part of what a system runs with. This module gathers
//! the rest — locked dependency and standard-library sources, the database
//! schema, config / routing / plugin manifests, service contracts and a
//! runtime trace — into one graph, and gives every call site and every
//! boundary site (where data leaves or enters the code) an outcome per
//! layer. Nothing is guessed: a site that no layer resolves stays unknown,
//! with a reason.
//!
//! - [`lockfiles`]: exact versions from lockfiles.
//! - [`locate`]: installed dependency sources, checked against the lock.
//! - [`scan`]: tree-sitter facts (calls, literals, decorators, tables, classes).
//! - [`models`]: declarative boundary models (`models.toml`).
//! - [`sql`], [`config`], [`contracts`]: the db / config / contract layers.
//! - [`world`]: layered assembly, outcomes, paths, recall.

pub mod config;
pub mod contracts;
pub mod locate;
pub mod lockfiles;
pub mod models;
pub mod scan;
pub mod sql;
pub mod world;
