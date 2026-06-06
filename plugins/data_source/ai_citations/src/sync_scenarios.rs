//! Happy- and unhappy-path sync scenarios for AI Citations.
//!
//! | Path | Scenario |
//! |------|----------|
//! | Happy | Full prompt × model enumeration with detail rows |
//! | Happy | Discover samples 1 prompt × 1 model, no checkpoints |
//! | Happy | max_prompts_per_run caps enumeration |
//! | Happy | Multiple models multiply job count |
//! | Happy | Brand mention + target domain checks pass |
//! | Happy | Second sync skips API when checkpoint exists |
//! | Happy | Site URL normalized to origin |
//! | Unhappy | Sync rejected without API key or fixture dir |
//! | Unhappy | Invalid config at plugin construction |
//! | Unhappy | Missing fixture file → error response row |
//! | Unhappy | No brand mention → BRAND_MENTIONED fails |
//! | Unhappy | No target domain link → TARGET_DOMAIN_LINKED fails |
//! | Unhappy | Error rows still emit checks + run summary |

use std::sync::{Arc, LazyLock, Mutex};

use crate::ai_citations::DataSourceAiCitationsPlugin;
use crate::client::FIXTURE_ENV;
use crate::config::{DataSourceAiCitationsPluginConfig, TrackedPrompt};
use crate::streams::{
    NAMESPACE_CHECK_DAILY, NAMESPACE_CITATION, NAMESPACE_LINK, NAMESPACE_MENTION,
    NAMESPACE_PROMPT_RESPONSE_DAILY, NAMESPACE_RUN_DAILY,
};
use crate::test_support::{checks_with_code, RecordingSyncContext};
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;

static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn fixture_dir() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures")
}

fn base_config() -> DataSourceAiCitationsPluginConfig {
    DataSourceAiCitationsPluginConfig {
        site: "https://example.com".into(),
        brand_names: vec!["Example".into()],
        prompt_list: vec![
            TrackedPrompt {
                id: "best_tools".into(),
                text: "What are the best project management tools?".into(),
                category: Some("discovery".into()),
                intent: None,
            },
            TrackedPrompt {
                id: "compare".into(),
                text: "Compare Example vs competitors.".into(),
                category: None,
                intent: Some("comparison".into()),
            },
        ],
        models: vec!["gpt-4.1-mini".into()],
        requests_per_minute: 600,
        max_prompts_per_run: 10,
        skip_unchanged_responses: true,
        openai_base_url: None,
    }
}

fn set_fixture_env(discover: bool) {
    std::env::set_var(FIXTURE_ENV, fixture_dir());
    std::env::remove_var("OPENAI_API_KEY");
    if discover {
        std::env::set_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV, "discover");
    } else {
        std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
    }
}

fn clear_fixture_env() {
    std::env::remove_var(FIXTURE_ENV);
    std::env::remove_var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV);
}

// --- Happy paths ---

#[tokio::test]
async fn happy_full_enumeration_emits_all_namespaces() {
    let _lock = env_lock();
    set_fixture_env(false);
    let mut plugin = DataSourceAiCitationsPlugin::new(base_config()).unwrap();
    let ctx = Arc::new(RecordingSyncContext::default());
    plugin.sync(ctx.clone()).await.expect("sync");
    clear_fixture_env();

    assert_eq!(ctx.row_count(NAMESPACE_PROMPT_RESPONSE_DAILY), 2);
    assert_eq!(ctx.row_count(NAMESPACE_CHECK_DAILY), 6);
    assert!(!ctx.rows(NAMESPACE_MENTION).is_empty());
    assert!(!ctx.rows(NAMESPACE_CITATION).is_empty());
    assert!(!ctx.rows(NAMESPACE_LINK).is_empty());
    assert_eq!(ctx.row_count(NAMESPACE_RUN_DAILY), 1);

    let run = &ctx.rows(NAMESPACE_RUN_DAILY)[0];
    assert_eq!(run["jobs_enumerated"], 2);
    assert_eq!(run["prompts_ok"], 2);
    assert_eq!(run["prompts_failed"], 0);
}

#[tokio::test]
async fn happy_discover_one_job_no_checkpoints() {
    let _lock = env_lock();
    set_fixture_env(true);
    let mut plugin = DataSourceAiCitationsPlugin::new(base_config()).unwrap();
    let ctx = Arc::new(RecordingSyncContext::default());
    plugin.sync(ctx.clone()).await.expect("discover sync");
    clear_fixture_env();

    assert_eq!(ctx.row_count(NAMESPACE_PROMPT_RESPONSE_DAILY), 1);
    assert_eq!(ctx.row_count(NAMESPACE_CHECK_DAILY), 3);
    assert!(ctx.checkpoint_stores.lock().unwrap().is_empty());
}

#[tokio::test]
async fn happy_max_prompts_per_run_caps_sample() {
    let _lock = env_lock();
    set_fixture_env(false);
    let mut cfg = base_config();
    cfg.max_prompts_per_run = 1;
    let mut plugin = DataSourceAiCitationsPlugin::new(cfg).unwrap();
    let ctx = Arc::new(RecordingSyncContext::default());
    plugin.sync(ctx.clone()).await.expect("sync");
    clear_fixture_env();

    assert_eq!(ctx.row_count(NAMESPACE_PROMPT_RESPONSE_DAILY), 1);
    assert_eq!(
        ctx.rows(NAMESPACE_PROMPT_RESPONSE_DAILY)[0]["prompt_id"],
        "best_tools"
    );
}

#[tokio::test]
async fn happy_multiple_models_multiply_jobs() {
    let _lock = env_lock();
    set_fixture_env(false);
    let mut cfg = base_config();
    cfg.prompt_list.truncate(1);
    cfg.models = vec!["gpt-4.1-mini".into(), "gpt-4o".into()];
    let mut plugin = DataSourceAiCitationsPlugin::new(cfg).unwrap();
    let ctx = Arc::new(RecordingSyncContext::default());
    plugin.sync(ctx.clone()).await.expect("sync");
    clear_fixture_env();

    assert_eq!(ctx.row_count(NAMESPACE_PROMPT_RESPONSE_DAILY), 2);
    let run = &ctx.rows(NAMESPACE_RUN_DAILY)[0];
    assert_eq!(run["jobs_enumerated"], 2);
    assert_eq!(run["models_tracked"], 2);
}

#[tokio::test]
async fn happy_brand_and_domain_checks_pass() {
    let _lock = env_lock();
    set_fixture_env(false);
    let mut cfg = base_config();
    cfg.prompt_list.truncate(1);
    let mut plugin = DataSourceAiCitationsPlugin::new(cfg).unwrap();
    let ctx = Arc::new(RecordingSyncContext::default());
    plugin.sync(ctx.clone()).await.expect("sync");
    clear_fixture_env();

    let checks = ctx.rows(NAMESPACE_CHECK_DAILY);
    assert_eq!(
        checks_with_code(&checks, "RESPONSE_SUCCESS")[0]["status"],
        "pass"
    );
    assert_eq!(
        checks_with_code(&checks, "BRAND_MENTIONED")[0]["status"],
        "pass"
    );
    assert_eq!(
        checks_with_code(&checks, "TARGET_DOMAIN_LINKED")[0]["status"],
        "pass"
    );
}

#[tokio::test]
async fn happy_second_sync_skips_api_when_checkpoint_exists() {
    let _lock = env_lock();
    set_fixture_env(false);
    let mut plugin = DataSourceAiCitationsPlugin::new(base_config()).unwrap();
    let ctx = Arc::new(RecordingSyncContext::default());
    plugin.sync(ctx.clone()).await.expect("first sync");
    let first_stores = ctx.checkpoint_stores.lock().unwrap().len();
    assert_eq!(first_stores, 2);

    plugin.sync(ctx.clone()).await.expect("second sync");
    clear_fixture_env();

    assert_eq!(
        ctx.checkpoint_stores.lock().unwrap().len(),
        first_stores,
        "unchanged responses must not rewrite checkpoints"
    );
    let run_rows = ctx.rows(NAMESPACE_RUN_DAILY);
    let run = run_rows.last().expect("second run row");
    assert_eq!(run["prompts_skipped_unchanged"], 2);
    let skipped: Vec<_> = ctx
        .rows(NAMESPACE_PROMPT_RESPONSE_DAILY)
        .into_iter()
        .filter(|r| r["content_unchanged"] == true)
        .collect();
    assert_eq!(skipped.len(), 2);
}

// --- Unhappy paths ---

#[tokio::test]
async fn unhappy_sync_rejected_without_credentials() {
    let _lock = env_lock();
    std::env::remove_var(FIXTURE_ENV);
    std::env::remove_var("OPENAI_API_KEY");
    std::env::remove_var("SKIPPR_OPENAI_FIXTURE_DIR");

    let mut plugin = DataSourceAiCitationsPlugin::new(base_config()).unwrap();
    let ctx = Arc::new(RecordingSyncContext::default());
    let err = plugin.sync(ctx).await.unwrap_err();
    assert!(err.to_string().contains("OPENAI_API_KEY"));
}

#[test]
fn unhappy_invalid_config_rejected() {
    let cfg = DataSourceAiCitationsPluginConfig {
        site: "  ".into(),
        brand_names: vec![],
        prompt_list: vec![TrackedPrompt {
            id: "p".into(),
            text: "q".into(),
            category: None,
            intent: None,
        }],
        models: vec!["m".into()],
        requests_per_minute: 10,
        max_prompts_per_run: 10,
        skip_unchanged_responses: true,
        openai_base_url: None,
    };
    assert!(DataSourceAiCitationsPlugin::new(cfg).is_err());
}

#[tokio::test]
async fn unhappy_missing_fixture_emits_error_response() {
    let _lock = env_lock();
    set_fixture_env(false);
    let cfg = DataSourceAiCitationsPluginConfig {
        site: "https://example.com".into(),
        brand_names: vec!["Example".into()],
        prompt_list: vec![TrackedPrompt {
            id: "does_not_exist".into(),
            text: "missing fixture".into(),
            category: None,
            intent: None,
        }],
        models: vec!["gpt-4.1-mini".into()],
        requests_per_minute: 600,
        max_prompts_per_run: 10,
        skip_unchanged_responses: true,
        openai_base_url: None,
    };
    let mut plugin = DataSourceAiCitationsPlugin::new(cfg).unwrap();
    let ctx = Arc::new(RecordingSyncContext::default());
    plugin.sync(ctx.clone()).await.expect("sync continues");
    clear_fixture_env();

    let response = &ctx.rows(NAMESPACE_PROMPT_RESPONSE_DAILY)[0];
    assert_eq!(response["status"], "error");
    assert_eq!(response["error_code"], "fixture_missing");
    assert_eq!(ctx.row_count(NAMESPACE_MENTION), 0);
    assert_eq!(
        checks_with_code(&ctx.rows(NAMESPACE_CHECK_DAILY), "RESPONSE_SUCCESS")[0]["status"],
        "fail"
    );
    assert_eq!(ctx.rows(NAMESPACE_RUN_DAILY)[0]["prompts_failed"], 1);
}

#[tokio::test]
async fn unhappy_no_brand_mention_check_fails() {
    let _lock = env_lock();
    set_fixture_env(false);
    let cfg = DataSourceAiCitationsPluginConfig {
        site: "https://example.com".into(),
        brand_names: vec!["Example".into()],
        prompt_list: vec![TrackedPrompt {
            id: "no_brand".into(),
            text: "List tools".into(),
            category: None,
            intent: None,
        }],
        models: vec!["gpt-4.1-mini".into()],
        requests_per_minute: 600,
        max_prompts_per_run: 10,
        skip_unchanged_responses: true,
        openai_base_url: None,
    };
    let mut plugin = DataSourceAiCitationsPlugin::new(cfg).unwrap();
    let ctx = Arc::new(RecordingSyncContext::default());
    plugin.sync(ctx.clone()).await.expect("sync");
    clear_fixture_env();

    assert_eq!(
        checks_with_code(&ctx.rows(NAMESPACE_CHECK_DAILY), "BRAND_MENTIONED")[0]["status"],
        "fail"
    );
    assert_eq!(ctx.row_count(NAMESPACE_MENTION), 0);
}

#[tokio::test]
async fn unhappy_no_target_domain_link_check_fails() {
    let _lock = env_lock();
    set_fixture_env(false);
    let cfg = DataSourceAiCitationsPluginConfig {
        site: "https://example.com".into(),
        brand_names: vec!["Example".into()],
        prompt_list: vec![TrackedPrompt {
            id: "no_brand".into(),
            text: "List tools".into(),
            category: None,
            intent: None,
        }],
        models: vec!["gpt-4.1-mini".into()],
        requests_per_minute: 600,
        max_prompts_per_run: 10,
        skip_unchanged_responses: true,
        openai_base_url: None,
    };
    let mut plugin = DataSourceAiCitationsPlugin::new(cfg).unwrap();
    let ctx = Arc::new(RecordingSyncContext::default());
    plugin.sync(ctx.clone()).await.expect("sync");
    clear_fixture_env();

    assert_eq!(
        checks_with_code(&ctx.rows(NAMESPACE_CHECK_DAILY), "TARGET_DOMAIN_LINKED")[0]["status"],
        "fail"
    );
}

#[tokio::test]
async fn unhappy_partial_failure_still_enumerates_all_prompts() {
    let _lock = env_lock();
    set_fixture_env(false);
    let mut cfg = base_config();
    cfg.prompt_list.push(TrackedPrompt {
        id: "does_not_exist".into(),
        text: "broken".into(),
        category: None,
        intent: None,
    });
    let mut plugin = DataSourceAiCitationsPlugin::new(cfg).unwrap();
    let ctx = Arc::new(RecordingSyncContext::default());
    plugin.sync(ctx.clone()).await.expect("sync");
    clear_fixture_env();

    assert_eq!(ctx.row_count(NAMESPACE_PROMPT_RESPONSE_DAILY), 3);
    let run = &ctx.rows(NAMESPACE_RUN_DAILY)[0];
    assert_eq!(run["prompts_ok"], 2);
    assert_eq!(run["prompts_failed"], 1);
}
