//! Compiles `src/route_core.rs` from the main crate, unchanged, so the loom
//! tests in `tests/` model-check the exact code `HelixRouter` runs.
#![allow(dead_code)]

#[path = "../../src/route_core.rs"]
pub mod route_core;
