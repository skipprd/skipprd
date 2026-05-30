# Local Development

## System dependencies

The repository assumes:

- Rust from `rust-toolchain.toml`
- `protoc` for Arrow/Lance-related builds
- OpenSSL development headers
- `libssh2` and `pkg-config` on platforms that build the SFTP plugin

CI also installs additional platform-specific dependencies on macOS and Windows for OpenSSL, `libssh2`, and vcpkg-backed builds.

## Fast feedback loop

`skipprd` consumes generic React crates from the private CodeArtifact Cargo registry `skippr/react-cargo`.

### Local IDE / agent (no CodeArtifact token)

When `skipprd` and `react` are sibling checkouts (for example `skippr/skipprd` and `skippr/react`), the Skippr IDE and Skippr Agent patch those crates from disk instead of downloading from `react-cargo`.

One-time build (recommended — the IDE reuses `skipprd/target/debug/skippr` and avoids `cargo run` on every command):

```bash
cd skipprd
./scripts/cargo-with-local-react.sh build -p skippr-cli
```

Optional overrides:

- `SKIPPR_REACT_ROOT` — path to the `react` repo if it is not `../react`
- `SKIPPRD_MANIFEST_PATH` — path to `skipprd/Cargo.toml` if the IDE cannot find skipprd in the workspace
- `SKIPPR_USE_LOCAL_SKIPPRD=0` — use an installed `skippr` on PATH instead of building from source

Any other cargo invocation can use the same wrapper:

```bash
./scripts/cargo-with-local-react.sh check -p skippr-cli
./scripts/cargo-with-local-react.sh test -p skippr-cli sql_prepare
```

### CodeArtifact token (CI and release builds)

`skipprd` and `react` use the private registry **`react-cargo`** on CodeArtifact (domain `skippr`, owner `132355036174`, `us-east-1`). Cargo expects **`CARGO_REGISTRIES_REACT_CARGO_TOKEN`** — same command as `react/.github/workflows/react-ci.yml` (“Login to CodeArtifact”) and `skipprd/.github/actions/setup-builder`.

Before local builds that must resolve published React crate versions from the registry:

```bash
export AWS_PROFILE=skippr-prod   # or any profile with codeartifact:GetAuthorizationToken on domain skippr
export CARGO_REGISTRIES_REACT_CARGO_TOKEN="$(
  aws codeartifact get-authorization-token \
    --domain skippr \
    --domain-owner 132355036174 \
    --region us-east-1 \
    --query authorizationToken \
    --output text
)"
```

`AWS_PROFILE=circles-prod` is fine for Picnic Athena/S3 work but only works for CodeArtifact if that IAM user/role is allowed on account `132355036174`; otherwise keep using `skippr-prod` for `cargo build` or use `./scripts/cargo-with-local-react.sh` with a sibling `react` checkout (no token).

Use these commands as the default local loop:

```bash
cargo check --workspace
cargo test -p skipprd -- --nocapture
python3 -m unittest discover -s .github/scripts -p 'test_*.py'
```

Add these when relevant:

```bash
cargo check --all-features
cargo test -p skippr-plugin-data-sink-postgres -- --nocapture
cargo test -p skipprd --test runtime_plugin_global_guards -- --nocapture
cargo test -p skipprd --test runtime_source_plugin_guards -- --nocapture
```

`cargo check --all-features` matters whenever you touch feature-gated code or shared crates that fan out into many plugin builds.

## Matching CI more closely

The main CI workflow does four distinct validation passes:

1. release-planning and catalog scripts
2. workspace or package compilation on each target platform
3. host test execution on Linux, plus postgres sink tests
4. runtime-plugin AWS e2e scenarios after published manifests are staged

If you only have time for one local compile gate, prefer:

```bash
cargo check --workspace
```

That catches the common hard-cutover failures where a plugin crate compiles shared helper modules but forgot to declare the helper crates it depends on.

## Runtime e2e workflow

The runtime e2e harness lives at `.github/scripts/runtime_e2e_harness.py`.

List supported scenarios:

```bash
python3 .github/scripts/runtime_e2e_harness.py list
```

Prepare shared AWS test state when needed:

```bash
python3 .github/scripts/runtime_e2e_harness.py prepare-aws-state
```

Stage a local release-like manifest tree for the current S3/Athena/Glue runtime plugins:

```bash
python3 .github/scripts/runtime_e2e_harness.py stage-local-runtime-release \
  --skipprd target/debug/skipprd
```

The `run` command rejects a host binary that lives next to runtime plugin executables, because the host is not supposed to ship bundled plugins. For local runs, copy `skipprd` into a clean directory first:

```bash
tmpdir="$(mktemp -d)"
mkdir -p "$tmpdir/debug"
cp target/debug/skipprd "$tmpdir/debug/skipprd"
```

Then run a fast smoke:

```bash
python3 .github/scripts/runtime_e2e_harness.py run bike_hire \
  --mode smoke \
  --skipprd "$tmpdir/debug/skipprd" \
  --local-runtime-manifest-dir /path/to/staged-local-runtime-release
```

For day-to-day iteration, prefer `bike_hire` over `bike_hire_many`. The chaos scenarios are slower and are better left for release validation or targeted exactly-once work.

## Pinning published runtime plugin versions

When testing a published version instead of a local staged manifest tree, use repeated `PLUGIN=VERSION` flags:

```bash
python3 .github/scripts/runtime_e2e_harness.py run bike_hire \
  --mode smoke \
  --runtime-plugin-version S3=8.1.0 \
  --runtime-plugin-version Athena=8.1.0 \
  --runtime-plugin-version Glue=8.1.0
```

Do not combine `--runtime-plugin-version` with `--local-runtime-manifest-dir`; the harness treats those as mutually exclusive sources of truth.

## Docs preview

The docs config lives at `docs/mkdocs.yml` and the source markdown lives at `docs/docs/`.

If MkDocs Material is not installed yet, create a small virtualenv and install it there:

```bash
python3 -m venv .venv-docs
source .venv-docs/bin/activate
python3 -m pip install mkdocs-material
```

Preview locally:

```bash
mkdocs serve -f docs/mkdocs.yml
```

Build the generated site:

```bash
mkdocs build -f docs/mkdocs.yml --strict
```
