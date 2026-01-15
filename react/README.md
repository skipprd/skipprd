## ReAct (`react` crate)

This crate implements a **WebSocket-based ReAct agent runtime**. Clients send JSON frames, the server routes each request to a **suite**, and the suite runs a **ReAct loop** (LLM → tool calls → observations → final/interrupt), using injected providers (query/catalog/vector/dbt/storage).

### Run the server

From the workspace root:

```bash
cargo run -p react -- serve --port 8787 --log
```

The server speaks WebSocket on `ws://localhost:8787/` using schemas in `../docs/openapi/ask-ws.yaml`.

### Configuration notes (LLM output size)

The **effective output token limit** is controlled by the environment variable **`LLM_MAX_TOKENS`**.

- When you run `react serve` with a YAML config (e.g. `react/react.yaml`), the loader in `src/config.rs` will **set `LLM_MAX_TOKENS` from `llm.max_tokens` if it is not already set**.
- If you see errors like `parser_error invalid JSON twice` during large batch scaffolds, your model output is likely being **truncated**. Increase `llm.max_tokens` (or set `LLM_MAX_TOKENS` explicitly) so tool-call JSON can fit (for `gpt-5.x`, **8192** is a reasonable starting point).

### Core architecture

#### Transport: WebSocket server

- **File**: `src/ws/server.rs`
- Responsibilities:
  - Parse/validate inbound frames (`type: new/open/user/...`)
  - Manage thread lifecycle (create thread id, persist user steps, stream responses)
  - Delegate execution to suites (the WS server does not call the agent loop directly)

#### Suites: product surfaces

- **Files**: `src/suites/*`
- A **suite** owns:
  - Which tools exist (tool registry)
  - Which prompts are used (system prompt + tool card)
  - Which policy defines “final” and interrupts
  - Optional preflight behavior (dataset discovery/context injection)

The default registry is built in `src/suites/registry.rs`. The primary suite is:
- **`data_engineer`**: modes `ask` / `model` / `cleanse` (`src/suites/data_engineer_suite/`)

Suites receive a `SuiteCtx` (injected capabilities) and return `FlowFrame`s (`Final`, `AwaitUser`, `AwaitApproval`).

#### Agent loop: ReAct runtime

- **File**: `src/agent/mod.rs`
- Responsibilities:
  - Maintain transcript state
  - Call the LLM and parse strict JSON actions:
    - `{"action":"<tool_name>","args":{...}}`
    - `{"final":{...}}`
  - Execute tools via `ToolRegistry`
  - Apply suite policy (`AgentPolicy`) to accept/reject finals and convert tool actions into interrupts

#### Tools

- **File**: `src/tools/mod.rs`
- Tools are dynamic actions by name. Suites decide which tools are available for a mode.

#### Persistence: threads and artifacts

- **Threads**: `src/session/mod.rs` (`ThreadStore` persists steps as JSON via `StorageAdapter` + `Keyspace`)
- **Artifacts/catalog**: stored via `StorageAdapter` at keys derived from `Keyspace`

### Providers (capabilities injection)

Providers live under `src/providers/*` and are injected via `SuiteCtx`:
- `QueryProvider` (SQL execution/schema/sample)
- `DatasetCatalogProvider` (dataset discovery)
- `CatalogProvider` (catalog/semantic)
- `VectorStore` (embeddings upsert/query)
- `DbtProvider` (dbt project scaffolding/validation)
- `StorageAdapter` + `Keyspace` (persistence layout)

The concrete providers used by the CLI server (`src/main.rs`) determine whether the runtime uses Athena, etc. Suites and the agent loop remain provider-agnostic.

### Extending the system

- **Add a new suite**: create `src/suites/<your_suite>/` and register it in `src/suites/registry.rs`
- **Add a tool**: implement `Tool` and register it in the relevant suite mode’s tool registry
- **Change “final” semantics**: implement a new `AgentPolicy` and use it in the suite’s `AgentCtx`
- **Swap infra**: construct a different `SuiteCtx` (different providers/storage/keyspace) and pass it to `ws::server::start_with_ctx`

