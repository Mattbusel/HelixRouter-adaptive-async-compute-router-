# Contributing to HelixRouter

Thank you for your interest in HelixRouter. This document explains how to set up a development environment, run tests and benchmarks, follow the code style, add a new routing strategy, and submit a pull request.

---

## Development setup

### Prerequisites

1. **Rust toolchain** — install via [rustup](https://rustup.rs/):
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   rustup update stable
   ```
   Rust 1.75 or later is required (`edition = "2021"`, `lto = true` in release profile).

2. **Clippy and rustfmt** (included with standard toolchain):
   ```bash
   rustup component add clippy rustfmt
   ```

3. **Clone the repo**:
   ```bash
   git clone https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-
   cd HelixRouter-adaptive-async-compute-router-
   ```

### Environment variables

| Variable | Purpose | Default |
|---|---|---|
| `HELIX_PORT` | HTTP port for the web dashboard and API | `8081` |
| `HELIX_CONFIG_PATH` | Path to a JSON `RouterConfig` file for hot-reload | *(none — hot-reload disabled)* |
| `RUST_LOG` | `tracing` log filter (e.g. `helixrouter=debug,info`) | `info` |

Example:
```bash
HELIX_CONFIG_PATH=./config/default.json RUST_LOG=helixrouter=debug cargo run --release
```

---

## Running tests

```bash
# All unit and integration tests
cargo test

# With verbose output
cargo test -- --nocapture

# A specific test
cargo test test_choose_strategy_inline

# With the optional distributed feature
cargo test --features distributed
```

All tests must pass before a PR is merged. The CI gate rejects any PR with failing tests.

---

## Running benchmarks

```bash
cargo bench
```

Benchmarks live in `benches/routing.rs` and use [Criterion](https://bheisler.github.io/criterion.rs/book/). The primary benchmark measures `choose_strategy()` across all five strategy paths and confirms sub-microsecond routing decisions.

To compare against a baseline:
```bash
cargo bench -- --save-baseline before
# make your changes
cargo bench -- --baseline before
```

HTML reports are written to `target/criterion/`.

---

## Code style

All code must pass the following checks before committing:

```bash
# Format
cargo fmt --all

# Lint (zero warnings policy — -D warnings is enforced in Cargo.toml)
cargo clippy --all-targets -- -D warnings

# Tests
cargo test
```

The project's `Cargo.toml` sets `clippy::unwrap_used`, `clippy::expect_used`, and `clippy::panic` to `deny` in all build profiles. Do not use `unwrap()`, `expect()`, or `panic!()` in any production code path. Use `Result`, `Option`, or explicit matching instead.

---

## How to add a new routing strategy

Adding a strategy involves changes across five files. Follow these steps in order:

### Step 1 — Add the variant to `Strategy` in `src/types.rs`

```rust
pub enum Strategy {
    Inline,
    Spawn,
    CpuPool,
    Batch,
    Drop,
    YourNewStrategy, // add here
}
```

Update the `Display` implementation and the `serde(rename_all = "snake_case")` rendering in the same file.

### Step 2 — Add a compute kernel in `src/strategies.rs`

If the strategy requires a new execution kernel, add a function:

```rust
/// Brief description of what your kernel computes.
pub fn your_kernel(job: &Job) -> Output {
    // deterministic, no I/O, no side effects
    Output::U64(/* result */)
}
```

Add a match arm in `execute_job()` if the kernel maps to a new `JobKind`.

### Step 3 — Add selection logic in `src/router.rs`

Update `choose_strategy()` with a rule for when the new strategy applies:

```rust
pub fn choose_strategy(cfg: &RouterConfig, job: &Job, cpu_busy: usize) -> Strategy {
    // ... existing rules ...
    if /* your condition */ {
        return Strategy::YourNewStrategy;
    }
    // ...
}
```

### Step 4 — Add execution dispatch in `Router::submit()` in `src/router.rs`

Add a match arm in the strategy dispatch block inside `submit()` that executes work under the new strategy. Ensure all outcomes (success and failure) record metrics and call `neural.record_outcome()`.

### Step 5 — Add the neural router strategy index in `src/neural_router.rs`

Update `N_STRATEGIES`, add a new `IDX_*` constant, and update `strategy_index()`:

```rust
const N_STRATEGIES: usize = 6; // was 5
const IDX_YOUR_NEW: usize = 5;

fn strategy_index(s: Strategy) -> usize {
    match s {
        // existing arms ...
        Strategy::YourNewStrategy => IDX_YOUR_NEW,
    }
}
```

Also update `warm_start_from_heuristics()` with a sensible initial weight row for the new strategy.

### Step 6 — Write tests

Add unit tests in the relevant module's `#[cfg(test)]` block covering:
- `choose_strategy()` returns your new strategy under the expected conditions.
- The kernel produces correct output for representative inputs.
- The strategy appears in Prometheus output (`prometheus_text`).
- The neural router can select and record outcomes for the new strategy.

---

## PR checklist

Before opening a pull request, confirm all of the following:

- [ ] `cargo fmt --all` — no formatting diff
- [ ] `cargo clippy --all-targets -- -D warnings` — zero warnings
- [ ] `cargo test` — all tests pass
- [ ] New production code has corresponding unit tests
- [ ] No `unwrap()`, `expect()`, or `panic!()` in production paths
- [ ] Public functions, structs, and enums have `///` doc comments
- [ ] PR description explains the *why*, not just the *what*
- [ ] If a new config field is added: `RouterConfig::validate()` is updated, `RouterConfigPatch` has the optional field, and the `PATCH /api/config` handler applies it

### Commit format

```
[feat|fix|test|refactor|perf|docs] short description

Body explaining the motivation and any tradeoffs.
```

---

## Questions

Open a [Discussion](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/discussions) for design questions or "would you accept a PR for X?" conversations.
