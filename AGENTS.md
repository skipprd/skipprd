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

## Private `react-cargo` registry (CodeArtifact)

`skipprd` depends on generic React crates published to AWS CodeArtifact (`react-cargo` on domain `skippr`, account `132355036174`, `us-east-1`). Cargo reads them via `[registries.react-cargo]` in `.cargo/config.toml`. The token env var name is **`CARGO_REGISTRIES_REACT_CARGO_TOKEN`** (12-hour TTL).

Same login step as `react/.github/workflows/react-ci.yml` and `skipprd/.github/actions/setup-builder`:

```bash
export CARGO_REGISTRIES_REACT_CARGO_TOKEN="$(
  aws codeartifact get-authorization-token \
    --domain skippr \
    --domain-owner 132355036174 \
    --region us-east-1 \
    --query authorizationToken \
    --output text
)"
```

Set **`AWS_PROFILE`** (or default credentials) to an IAM principal that has `codeartifact:GetAuthorizationToken` on that domain. In maintainer docs we often use `AWS_PROFILE=skippr-prod`; Picnic/local work may use `circles-prod` only if that profile is granted CodeArtifact access on account `132355036174` (otherwise use `skippr-prod` or the local-react path below).

**Without a token:** when `skipprd` and `react` are sibling checkouts, use path patches instead of the registry:

```bash
./scripts/cargo-with-local-react.sh build -p skipprd
# optional: export SKIPPR_REACT_ROOT=/path/to/react
```

More detail: `docs/docs/maintainers/local-development.md`.

## Build, test, and lint

- **Build:** `cargo build`
- **Test:** `cargo test`
- **Format check:** `cargo fmt --all -- --check` (existing formatting diffs may exist)
- **Clippy:** `cargo clippy` (run without `-D warnings` unless explicitly requested)
- **Runtime e2e harness:** before release/tag work, run the relevant local unit and harness tests plus the targeted runtime e2e path when credentials/services are available. At minimum, validate harness changes with `python3 .github/scripts/test_runtime_e2e_harness.py`.
- **GitHub release CI:** the release workflow is tag-triggered. Use the scratch tag `0.0.0` for CI validation reruns, then move the intended release tag only after local tests pass and the relevant `0.0.0` GitHub Actions run is healthy.

## GitHub E2E testing

Release CI on `skipprd-private` is a **full integration test** of user-facing behaviour, not a slim compile check.

- E2E jobs exercise the same paths customers use: `skippr doctor`, `discover`, `sync`, `model`, workspace run locks on `auth.skippr.io`, runtime plugins, and downstream sinks (Iceberg/Glue/Athena, Snowflake, etc.).
- CI authenticates with `SKIPPR_API_KEY` (API key exchange → JWT) the same way automation customers use; do **not** add CI-only shortcuts that skip auth, run locks, ingest, or other platform steps.
- If an E2E job fails, fix the product or the test scenario—do not bypass the failing step for GitHub Actions only.
- Auth API routes (including `/auth/workspaces/.../runs/lock/*`) must be deployed to production before release tags that depend on them; the CLI assumes those endpoints exist when run locks are enabled.

## Key gotchas

1. **Memory-constrained linking:** test builds can be heavy on memory.
2. **Feature safety:** verify feature-gated code with `cargo check --all-features` when touching features.
