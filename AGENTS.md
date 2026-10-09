# AGENTS.md

## Design principles

When making changes to this codebase, follow these principles in order of priority:

1. **Compile-time guarantees:** Prefer types, enums, and trait bounds over runtime checks. If an invariant can be enforced by the compiler, it must be. Use Rust's type system (`enum` variants, `Option`, `Result`, newtypes) to make illegal states unrepresentable. Feature-gated code (`#[cfg(feature = "...")]`) must compile cleanly when enabled.

2. **Design simplicity:** Favour the simplest design that satisfies the requirements. Avoid over-abstraction. A flat function is better than a trait hierarchy with one implementor. If a module exists only to re-export, remove the indirection.

3. **DRY (Don't Repeat Yourself):** Extract shared logic into functions, traits, or shared modules. Duplicated error messages, validation logic, or serialization patterns are bugs waiting to diverge.

Always favour compile errors over runtime convention

## Overview

This repository contains **skipprd** — the self-hosted ELT engine (`discover`, `sync`, `query`). Cloud `skippr` lives in the private `cloud` repo.

Public engineer docs: [skippr.io](https://skippr.io). Markdown is `docs/docs/`; VitePress config is `docs/.vitepress/`. Preview with `npm --prefix docs run dev`. The live apex site is composed in sibling `skippr-web` (`npm run docs:compose` copies this tree into `skippr-web/docs/`) and published with `cloud/scripts/publish-skippr-web.sh`. `https://elt.skippr.io` **301s** to `https://skippr.io`.

## System dependencies (already installed in snapshot)

- Rust 1.88.0 (via `rust-toolchain.toml`)
- `protobuf-compiler` (`protoc`) — required at compile time by Arrow/Lance-related crates
- `libssl-dev` — required by `openssl-sys` crate

## Build, test, and lint

- **Build:** `cargo build`
- **Test:** `cargo test`. Install the pre-commit hook with `./scripts/install-git-hooks.sh` so `cargo test -p skipprd --lib` runs before each commit.
- **Format check:** `cargo fmt --all -- --check` (existing formatting diffs may exist)
- **Clippy:** `cargo clippy` (run without `-D warnings` unless explicitly requested)
- **Runtime e2e harness:** before release/tag work, run the relevant local unit and harness tests plus the targeted runtime e2e path when credentials/services are available. At minimum, validate harness changes with `python3 .github/scripts/test_runtime_e2e_harness.py`.
- **GitHub release CI:** both **Python CI/CD Pipeline** (`.github/workflows/ci.yml`) and **Rust CI/CD Pipeline** (`.github/workflows/rust.yml`) are tag-triggered (plus `workflow_dispatch`), not `main` / master. Use the scratch tag `0.0.0` for engine CI validation reruns, then move the intended release tag only after local tests pass and the relevant `0.0.0` GitHub Actions run is healthy. Python wheels build and test on engine tags and `python-v*` on GitHub-hosted Linux plus the self-hosted `skippr-darwin-arm64` runner; `python-publish` runs on the same unprefixed engine tags as Rust `publish_skipprd` when local wheel filenames are not yet on PyPI (a re-tag of an already-published semver stamps a PEP 427 build number so `pip install skippr==x.y.z` still gets the new wheels). Rust builds the host plus the full workspace plugin catalog on `ubuntu-latest` and `skippr-darwin-arm64`, then `publish_runtime_plugins` uploads protocol-matching manifests. Do not add a `PYPI_API_TOKEN`. Engine tags are unprefixed semver (`0.1.0`), not `v0.1.0`.

## GitHub E2E testing

Release CI on `skipprd-private` is a **full integration test** of user-facing behaviour, not a slim compile check.

- E2E jobs exercise the same paths customers use: `skipprd discover`, `skipprd sync`, runtime plugins, and downstream sinks (SkipprLake, Athena, Snowflake, etc.).
- CI authenticates with `SKIPPR_API_KEY` (API key exchange → JWT) the same way automation customers use; do **not** add CI-only shortcuts that skip auth, run locks, ingest, or other platform steps.
- If an E2E job fails, fix the product or the test scenario—do not bypass the failing step for GitHub Actions only.
- Auth API routes (including `/auth/workspaces/.../runs/lock/*`) must be deployed to production before release tags that depend on them; the CLI assumes those endpoints exist when run locks are enabled.

## Key gotchas

1. **Memory-constrained linking:** test builds can be heavy on memory.
2. **Feature safety:** verify feature-gated code with `cargo check --all-features` when touching features.
