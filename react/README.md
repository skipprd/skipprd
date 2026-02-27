## ReAct (`react` crate)

This crate implements a **WebSocket-based ReAct agent runtime**. Clients send JSON frames, the server routes each request to a **suite**, and the suite runs a **ReAct loop** (LLM → tool calls → observations → final/interrupt), using injected providers (query/catalog/vector/dbt/storage).

### Getting started

- Windows + BigQuery (local storage): see `GETTING_STARTED_WINDOWS_BIGQUERY.md`

### Run the server

From the workspace root:

```bash
cargo run -p react -- serve --config react/runtime/config.example.yml --port 8787 --terminal
```

The server speaks WebSocket on `ws://localhost:8787/` using schemas in `runtime/openapi/ws-core.yaml` plus suite overlays.

Notes:
- By default, storage is **local** (`storage.mode: local`) and persists under `storage.path` (default `./.react`).
- To use S3 storage, pass `--storage-mode s3 --bucket <BUCKET>` (or set `storage.mode: s3` + `storage.bucket` in YAML / env `SKIPPR_S3_BUCKET`).

### Configuration notes (LLM output size)

The **effective output token limit** is controlled by the environment variable **`LLM_MAX_TOKENS`**.

- When you run `react serve` with a YAML config (e.g. `react/runtime/config.example.yml`), the loader in `src/config.rs` will **set `LLM_MAX_TOKENS` from `llm.max_tokens` if it is not already set**.
- If you see errors like `parser_error invalid JSON twice` during large batch scaffolds, your model output is likely being **truncated**. Increase `llm.max_tokens` (or set `LLM_MAX_TOKENS` explicitly) so tool-call JSON can fit (for `gpt-5.x`, **8192** is a reasonable starting point).

### Core architecture

#### Transport: WebSocket server

- **File**: `runtime/src/ws/server.rs`
- Responsibilities:
  - Parse/validate inbound frames (`type: new/open/user/...`)
  - Manage thread lifecycle (create thread id, persist user steps, stream responses)
  - Delegate execution to suites (the WS server does not call the agent loop directly)

#### Suites: product surfaces

- **Files**: `suites/react-suites/src/*`
- A **suite** owns:
  - Which tools exist (tool registry)
  - Which prompts are used (system prompt + tool card)
  - Which policy defines “final” and interrupts
  - Optional preflight behavior (dataset discovery/context injection)

The default registry is built in `suites/react-suites/src/registry.rs`. Registered suites include:
- **`data_engineer`**: analytics + DBT workflow (`suites/react-suites/src/data_engineer/`)
- **`kb`**: local knowledge-base workflow (`suites/react-suites/src/kb/`)

Suites receive a `SuiteCtx` (injected capabilities) and return `FlowFrame`s (`Final`, `AwaitUser`, `AwaitApproval`).

#### Agent loop: ReAct runtime

- **File**: `../core/src/agent/mod.rs`
- Responsibilities:
  - Maintain transcript state
  - Call the LLM and parse strict JSON actions:
    - `{"type":"tool","name":"<tool_name>","args":{...}}`
    - `{"type":"final","final":{...}}`
  - Execute tools via `ToolRegistry`
  - Apply suite policy (`AgentPolicy`) to accept/reject finals and convert tool actions into interrupts

#### Tools

Tools are dynamic actions by name. Suites decide which tools are available for each suite-defined flow.

#### Persistence: threads and artifacts

- **Threads**: `../core/src/session/mod.rs` (`ThreadStore` persists steps as JSON via `StorageAdapter` + `Keyspace`)
- **Artifacts/catalog**: stored via `StorageAdapter` at keys derived from `Keyspace`

### Providers (capabilities injection)

Providers live under `runtime/src/providers/*` and are injected via `SuiteCtx`:
- `QueryProvider` (SQL execution/schema/sample)
- `DatasetCatalogProvider` (dataset discovery)
- `CatalogProvider` (catalog/semantic)
- `VectorStore` (embeddings upsert/query)
- `DbtProvider` (dbt project scaffolding/validation)
- `StorageAdapter` + `Keyspace` (persistence layout)

The concrete providers used by the CLI server (`runtime/src/main.rs`) determine whether the runtime uses Athena, etc. Suites and the agent loop remain provider-agnostic.

### Extending the system

- **Add a new suite**: create `suites/react-suites/src/<your_suite>/` and register it in `suites/react-suites/src/registry.rs`
- **Add a tool**: implement `Tool` and register it in the relevant suite flow’s tool registry
- **Change “final” semantics**: implement a new `AgentPolicy` and use it in the suite’s `AgentCtx`
- **Swap infra**: construct a different `SuiteCtx` (different providers/storage/keyspace) and pass it to `ws::server::start_with_ctx`

