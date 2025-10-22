# Local and Remote LLMs in Skippr

This guide explains how to run Skippr with a local llama.cpp model (GGUF) or an OpenAI-compatible HTTP provider, and how Skippr auto-tunes settings for your machine.

## Quick start (local, Mac/Linux)

1) Get a GGUF model (recommended 7B Instruct):
- Mistral 7B Instruct: `TheBloke/Mistral-7B-Instruct-v0.2-GGUF`
- Qwen2 7B Instruct: `Qwen/Qwen2-7B-Instruct-GGUF`
- Llama 3.1 8B Instruct: `bartowski/Meta-Llama-3.1-8B-Instruct-GGUF`

2) Place model in `./models/`, e.g.:
- `./models/mistral-7b-instruct-v0.2.Q4_K_M.gguf`

Download example with Hugging Face CLI:
```bash
huggingface-cli download TheBloke/Mistral-7B-Instruct-v0.2-GGUF \
  mistral-7b-instruct-v0.2.Q4_K_M.gguf --local-dir ./models
```

3) Run a chat:
```bash
# Build with local llama.cpp enabled
cargo run --features llama_cpp --quiet llm --chat "Say hello briefly"
```

If `LLM_CHAT_MODEL` is not set, Skippr will try to auto-locate a GGUF file in `./models/` and auto-tune GPU offload layers.

## Configuration

Environment variables (all optional; sensible defaults):
- `LLM_PROVIDER`:
  - Default: Local (llama.cpp). Set to `OPENAI` for OpenAI-compatible HTTP.
- `LLM_CHAT_MODEL`:
  - Absolute path to GGUF. If unset, Skippr attempts to find a GGUF under `./models/`.
- `LLM_EMBED_MODEL`:
  - For HTTP embeddings. Local embeddings are model-dependent.
- `LLM_BASE_URL`, `LLM_API_KEY`:
  - For OpenAI-compatible HTTP providers.
- `LLM_GPU_LAYERS`:
  - Hint for local offload (overridden by auto-tuning). Default auto-tunes.
- `LLM_CONTEXT_LENGTH`:
  - Default `4096`.

## Auto-tuning (local llama.cpp)

Skippr auto-tunes GPU offload layers (`n_gpu_layers`) on first run per model and saves the optimal value for next start. Behavior:
- Starts from an aggressive guess (platform-specific), attempts to load, and backs off (−8 layers each attempt) until successful.
- On Mac M1/M2 (Metal), starts around 32–40.
- On Linux CPU-only, starts at 0.
- On Linux GPU (e.g., L4/A10G), a mid value is tried and reduced as needed.
- If inference fails due to memory/initialization, Skippr reduces layers and retries; successful values are persisted.

Persisted tuning file:
- Stored under `$(DATA_DIR)/catalog_cache/llm_tuning.json`, keyed by absolute model path.
- If an error occurs in subsequent runs, Skippr reduces the saved value and updates the file.

You can override by setting `LLM_GPU_LAYERS` explicitly.

## OpenAI-compatible HTTP

Set `LLM_PROVIDER=OPENAI`, `LLM_BASE_URL`, `LLM_API_KEY`, and models:
```bash
LLM_PROVIDER=OPENAI \
LLM_BASE_URL=https://api.openai-compatible.local \
LLM_API_KEY=xxx \
LLM_CHAT_MODEL=gpt-4o-mini \
LLM_EMBED_MODEL=text-embedding-3-small \
cargo run --quiet llm --chat "Say hello"
```

## Model suggestions

- MacOS M1 Prod:
  - 16 GB RAM: 7B Q4_K_M (Mistral/Qwen2). Auto-tune will offload ~20–40 layers.
  - 32 GB RAM: 7B Q5_K_M or 13B Q4_K_M (slower). Auto-tune adjusts accordingly.
- Linux GitHub runner (CPU-only): 7B Q4_K_M, `LLM_GPU_LAYERS=0`.
- Linux EC2:
  - GPU recommended: g5.xlarge (A10G) or g6.xlarge (L4). 7B/13B Q4–Q5.
  - CPU-only: c7i/c7g (expect slower latency).

## CLI

- Chat: `skippr llm --chat "<prompt>"`
- Embeddings: `skippr llm --embed "text1" --embed "text2"`

## Notes

- Local embeddings via llama.cpp are supported. If the model lacks sequence pooling, Skippr falls back to averaging token embeddings for each input.
- If no local model is present or loading fails, embeddings gracefully return zero-vectors so tests/CI pass without a GGUF.
- Expected CLI output format for embeddings is one line per input: `index:dimension` (e.g., `0:4096`).
- Skippr suppresses llama.cpp/ggml logs by default to keep stdout clean during CLI runs.
- Context length >4096 increases memory; auto-tune currently targets GPU layers only.
