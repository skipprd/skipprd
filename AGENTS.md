# AGENTS.md

## Design principles

When making changes to this codebase, follow these principles in order of priority:

1. **Compile-time guarantees:** Prefer types, enums, and trait bounds over runtime checks. If an invariant can be enforced by the compiler, it must be. Use Rust's type system (`enum` variants, `Option`, `Result`, newtypes) to make illegal states unrepresentable. Feature-gated code (`#[cfg(feature = "...")]`) must compile cleanly when enabled.

2. **Design simplicity:** Favour the simplest design that satisfies the requirements. Avoid over-abstraction. A flat function is better than a trait hierarchy with one implementor. If a module exists only to re-export, remove the indirection.

3. **DRY (Don't Repeat Yourself):** Extract shared logic into functions, traits, or shared modules. Duplicated error messages, validation logic, or serialization patterns are bugs waiting to diverge.

Always favour compile errors over runtime convention

## Overview

This repository contains **Skippr** — a Rust-based data ingestion and transformation CLI.

## System dependencies (already installed in snapshot)

- Rust 1.88.0 (via `rust-toolchain.toml`)
- `protobuf-compiler` (`protoc`) — required at compile time by Arrow/Lance-related crates
- `libssl-dev` — required by `openssl-sys` crate

## Build, test, and lint

- **Build:** `cargo build`
- **Test:** `cargo test`
- **Format check:** `cargo fmt --all -- --check` (existing formatting diffs may exist)
- **Clippy:** `cargo clippy` (run without `-D warnings` unless explicitly requested)
- **Runtime e2e harness:** before release/tag work, run the relevant local unit and harness tests plus the targeted runtime e2e path when credentials/services are available. At minimum, validate harness changes with `python3 .github/scripts/test_runtime_e2e_harness.py`.
- **GitHub release CI:** the release workflow is tag-triggered. Use the scratch tag `0.0.0` for CI validation reruns, then move the intended release tag only after local tests pass and the relevant `0.0.0` GitHub Actions run is healthy.

## Key gotchas

1. **Memory-constrained linking:** test builds can be heavy on memory.
2. **Feature safety:** verify feature-gated code with `cargo check --all-features` when touching features.
