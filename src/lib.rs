//! Library target of the `mirako` binary: exposes the modules to the integration tests under
//! `tests/` and the benchmarks under `benches/`. `main.rs` is the CLI on top of this.

pub mod client;
pub mod config;
pub mod delta;
pub mod gc;
pub mod index;
pub mod patterns;
pub mod progress;
pub mod proto;
pub mod rewrite;
pub mod server;
pub mod setup;
pub mod shim;
pub mod xfer;
