# AGENTS.md

## Design principles

When making changes to this codebase, follow these principles in order of priority:

1. **Compile-time guarantees:** Prefer types, enums, and trait bounds over runtime checks. If an invariant can be enforced by the compiler, it must be. Use Rust's type system (`enum` variants, `Option`, `Result`, newtypes) to make illegal states unrepresentable. Feature-gated code (`#[cfg(feature = "...")]`) must compile cleanly when the feature is enabled — CI runs `--all-features`.

2. **Design simplicity:** Favour the simplest design that satisfies the requirements. Avoid over-abstraction. A flat function is better than a trait hierarchy with one implementor. If a module exists only to re-export, remove the indirection.

3. **DRY (Don't Repeat Yourself):** Extract shared logic into functions, traits, or shared modules. When the same pattern appears in multiple suites/providers/tools, lift it into `react-core` or a shared utility. Duplicated error messages, validation logic, or serialization patterns are bugs waiting to diverge.

## Architecture

```
react-core        Interfaces: agent loop, Suite trait, SuiteCtx, SuiteRegistry,
                  FlowFrame, provider traits, session, storage, resolved_config,
                  enums (WarehouseKind, StorageMode, LlmProvider)
react (runtime)   Framework + CLI binary: config, LLM impls, WS server, helpers
react-suites      Plugin: data_engineer + kb suite implementations
react/modules/*   Plugins: provider implementations (athena, postgres, bigquery,
                  dbt, storage, lance)
```

Dependency direction is enforced by the compiler: core has zero deps on suites/providers/runtime. Suites and providers depend only on core. The react binary wires everything together.

### Future compile-time improvements (documented, not yet implemented)

- **ThreadId/SuiteId newtypes** — bare `String` IDs flow through ~100+ sites; newtypes would prevent mixing thread_id with suite_id or tool_id at the type level.
- **DataEngineerCtx** — `SuiteCtx` has 6 `Option<Arc<dyn ...>>` provider fields that the data_engineer suite checks at ~20 call sites. A suite-scoped context with required fields would eliminate those runtime checks.

## Cursor Cloud specific instructions

### Overview

This is **Skippr** — a Rust-based data ingestion, transformation, and AI-powered analytics platform. It is a Cargo workspace with:

- **`skippr`** — Data pipeline CLI (separate from react, CI disabled).
- **`react`** — ReAct agent framework + CLI. Hosts a WebSocket server that routes client requests through suites (`data_engineer`, `kb`).

### System dependencies (already installed in snapshot)

- Rust 1.88.0 (via `rust-toolchain.toml`)
- `protobuf-compiler` (`protoc`) — required at compile time by LanceDB/Arrow crates
- `libssl-dev` — required by `openssl-sys` crate

### Build, test, and lint

Standard commands — refer to `Cargo.toml` and `react/README.md` for details.

- **Build:** `cargo build`
- **Test (all):** Due to memory constraints, test individual crates rather than the full workspace:
  - `cargo test -p react-core --lib --tests`
  - `cargo test -p react-suites --lib`
  - `cargo test -p react --lib` (may need `CARGO_BUILD_JOBS=1` to avoid OOM during linking)
- **Format check:** `cargo fmt --all -- --check` (existing code has formatting diffs — not blocking)
- **Clippy:** `cargo clippy` (existing code has warnings — run without `-D warnings` to avoid false failures)

### Running the react server

From the workspace root:

```bash
cargo run -p react -- serve --config react/runtime/config.smoketest.yml --port 8787
```

The server speaks WebSocket on `ws://localhost:8787/`. A smoke-test config (`react/runtime/config.smoketest.yml`) is provided with:
- Local storage mode (`./.react`)
- Null LLM provider (no external API key needed)
- Postgres warehouse stub (no real Postgres needed — server boots and handles thread lifecycle)
- All optional providers disabled (catalog, dbt, vector)

For full LLM functionality, set `LLM_API_KEY` env var and update the config's `llm` section.

### Key gotchas

1. **Memory-constrained linking:** The full workspace test build (`cargo test`) may OOM during linking of the `react` binary test target. Use `CARGO_BUILD_JOBS=1` or `CARGO_BUILD_JOBS=2` to reduce parallelism, or test crates individually.
2. **`cargo fmt` diffs exist in the codebase** — this is pre-existing and not a sign of broken code. Don't try to fix formatting unless explicitly asked.
3. **`cargo clippy` warnings exist** — run without `-D warnings` to avoid false build failures on existing code.
4. **OpenAPI generated code** in `react/runtime/src/ws/api_gen/` — this code is auto-generated and has its own formatting style. Don't modify manually.
5. **WebSocket API uses camelCase field names** — e.g. `suiteId`, `agentType`, `threadId` (not snake_case). Refer to the OpenAPI spec in `react/runtime/openapi/ws-core.yaml`.
6. **CI runs `cargo test --release --all --all-features`** for the `skippr` build. Feature-gated code (e.g. `llama_cpp`) must compile even when that feature is active. Always verify with `cargo check --all-features` locally before pushing.
