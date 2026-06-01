# AI Citations source plugin

Runtime source plugin: `plugins/data_source/ai_citations/` (`AiCitations` / `ai_citations`).

Queries OpenAI-compatible chat APIs for every configured **prompt × model** pair and emits daily bronze tables for responses, brand mentions, citations, links, and visibility checks.

## Bronze catalog

| Namespace | Grain |
| --- | --- |
| `ai_citations.run_daily` | 1 / site / day |
| `ai_citations.prompt_response_daily` | prompt × model / day |
| `ai_citations.mention` | mention span / day |
| `ai_citations.citation` | cited URL / day |
| `ai_citations.link` | link URL / day |
| `ai_citations.check_daily` | check outcome / day |

All namespaces use `replace_partition` on `run_date`.

## Discover sampling (automatic)

When `SKIPPR_RUNTIME_EXECUTION_MODE=discover`:

- One prompt and one model only
- No checkpoint load/advance
- Full namespace contracts still emitted

## Configuration (engine `skippr.yml`)

```yaml
pipelines:
  brand_ai_visibility:
    data_source: data_sources.brand_ai
    data_sink: data_sinks.warehouse
    transform:
      batch_time_fields: run_date
      batch_time_unit: day

data_sources:
  brand_ai:
    AiCitations:
      site: "https://example.com"
      brand_names:
        - "Example"
        - "Example App"
      prompt_list:
        - id: best_project_tools
          text: "What are the best project management tools?"
          category: discovery
        - id: compare_example
          text: "How does Example compare to competitors?"
          intent: comparison
      models:
        - gpt-4.1-mini
      requests_per_minute: 10
      max_prompts_per_run: 50
      skip_unchanged_responses: true
```

Credentials: `OPENAI_API_KEY` and optional `OPENAI_BASE_URL` for OpenAI-compatible endpoints.

## Test coverage

Scenario tests live in `plugins/data_source/ai_citations/src/sync_scenarios.rs`.

| Path | Test |
| --- | --- |
| Happy | `happy_full_enumeration_emits_all_namespaces` |
| Happy | `happy_discover_one_job_no_checkpoints` |
| Happy | `happy_max_prompts_per_run_caps_sample` |
| Happy | `happy_multiple_models_multiply_jobs` |
| Happy | `happy_brand_and_domain_checks_pass` |
| Happy | `happy_second_sync_skips_api_when_checkpoint_exists` |
| Unhappy | `unhappy_sync_rejected_without_credentials` |
| Unhappy | `unhappy_invalid_config_rejected` |
| Unhappy | `unhappy_missing_fixture_emits_error_response` |
| Unhappy | `unhappy_no_brand_mention_check_fails` |
| Unhappy | `unhappy_no_target_domain_link_check_fails` |
| Unhappy | `unhappy_partial_failure_still_enumerates_all_prompts` |

Unit tests cover config validation, client fixture/skip behavior, extraction, checks, checkpoints, and namespace contracts.

## Local verification

```bash
cargo build -p skippr-plugin-data-source-ai-citations
cargo test -p skippr-plugin-data-source-ai-citations
cargo test -p skippr-cli translate_ai_citations_source
```

Fixture-only sync (no live API):

```bash
export SKIPPR_AI_CITATIONS_FIXTURE_DIR=plugins/data_source/ai_citations/fixtures
export USE_LOCAL_PLUGIN_CODE=1
manifest_dir="$(python3 .github/scripts/local_runtime_plugins.py \
  --config path/to/skippr.yml --pipeline brand_ai_visibility)"
export SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR="$manifest_dir"
skippr discover --pipeline brand_ai_visibility
skippr sync --once --pipeline brand_ai_visibility
```

## Checkpointing

Per prompt + model key: `ai_citations:prompt:{prompt_id}:{model}`.

When `skip_unchanged_responses` is true (default) and a checkpoint exists, the plugin skips the API call but still emits `prompt_response_daily` and `check_daily` rows for partition completeness.

## Checks

| Code | Meaning |
| --- | --- |
| `RESPONSE_SUCCESS` | API call succeeded |
| `BRAND_MENTIONED` | Brand/alias found in answer |
| `TARGET_DOMAIN_LINKED` | Target site domain appears in citations or links |
