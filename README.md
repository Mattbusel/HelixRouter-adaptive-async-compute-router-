# HelixRouter

[![CI](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/helixrouter.svg)](https://crates.io/crates/helixrouter)
[![docs.rs](https://docs.rs/helixrouter/badge.svg)](https://docs.rs/helixrouter)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.75+](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

**Adaptive async compute routing engine for Rust.**

HelixRouter is a runtime execution control plane that decides *how* work runs -- inline, spawned, pooled, batched, or dropped -- based on live system pressure, compute cost, latency budgets, and online-learned strategy quality estimates. Routing decisions are made per-job in sub-microsecond time with zero blocking in the async runtime.
