//! Module topology: the reference graph of a JS/TS workspace and the math over it.
//!
//! `workspace` finds packages, `imports` reads every module reference with oxc,
//! `resolve` pins each to a file / package / outside, `graph` projects the
//! result to package or module granularity, and `algo` is the graph math.

pub mod algo;
pub mod glob;
pub mod graph;
pub mod imports;
pub mod metrics;
pub mod pattern;
pub mod pyimports;
pub mod query_scope;
pub mod resolve;
pub mod tsconfig;
pub mod workspace;
