# AGENTS.md

## Cursor Cloud specific instructions

### Overview

This is **Skippr** — a Rust-based data ingestion, transformation, and AI-powered analytics platform. It is a Cargo workspace with two main binaries:

- **`skippr`** — Data pipeline CLI for ingesting data from S3, transforming it, and writing to a datalake/warehouse (S3 Parquet, AWS Athena/Glue).
- **`react`** — WebSocket-based ReAct agent runtime. Hosts an AI agent server that routes client requests through suites (`data_engineer`, `kb`).

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
