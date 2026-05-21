//! Shared authentication + react resolution for headless agent runs (`skippr model`, `skippr chat`).

use std::path::PathBuf;
use std::sync::Arc;

use react::config::{LlmFile, ReactConfigFile, ScopeFile, StorageFile};
use react_core::resolved_config::ReactResolvedConfig;

use crate::api_client::ApiClient;
use crate::auth;
use crate::react_host;
use crate::translate;
use react_suite_data_engineer::PipelineName;

#[derive(Clone, Debug)]
pub enum ChatTarget {
    Pipeline(PipelineName),
    GenericIdeBootstrap,
}

pub struct HeadlessAuthContext {
    pub resolved: ReactResolvedConfig,
    pub client: ApiClient,
    pub pipeline: PipelineName,
}

/// Authenticate, overlay server credentials, resolve react config, and attach the refreshable S3
/// credentials provider — same spine as `skippr model`.
pub async fn authenticate_headless_for_pipeline(
    explicit_config: &Option<PathBuf>,
    pipeline: &str,
) -> Result<HeadlessAuthContext, String> {
    let pipeline = PipelineName::parse(pipeline)?;
    authenticate_headless(explicit_config, HeadlessTarget::Pipeline(pipeline)).await
}

/// Authenticate and resolve a headless chat config. When `pipeline` is absent,
/// build a minimal data-engineer runtime so chat can answer general/local-code
/// questions and create a new `skippr.yml` before a project config exists.
pub async fn authenticate_headless_for_chat(
    explicit_config: &Option<PathBuf>,
    target: ChatTarget,
) -> Result<HeadlessAuthContext, String> {
    match target {
        ChatTarget::Pipeline(pipeline) => {
            authenticate_headless(explicit_config, HeadlessTarget::Pipeline(pipeline)).await
        }
        ChatTarget::GenericIdeBootstrap => {
            authenticate_headless(&None, HeadlessTarget::GenericIdeBootstrap).await
        }
    }
}

#[derive(Clone, Debug)]
enum HeadlessTarget {
    Pipeline(PipelineName),
    GenericIdeBootstrap,
}

async fn authenticate_headless(
    explicit_config: &Option<PathBuf>,
    target: HeadlessTarget,
) -> Result<HeadlessAuthContext, String> {
    let engine_cfg = crate::load_cli_execution_config(explicit_config)
        .map_err(|e| format!("{e}\nRun 'skippr init <project>' first."))
        .ok();
    let pipeline = match &target {
        HeadlessTarget::Pipeline(pipeline) => Some(pipeline.clone()),
        HeadlessTarget::GenericIdeBootstrap => None,
    };
    let mut internal_file = match (engine_cfg.as_ref(), &target) {
        (Some(engine_cfg), HeadlessTarget::Pipeline(pipeline)) => {
            crate::react_config_from_pipeline_config(engine_cfg, pipeline.as_str())
                .map_err(|e| e)?
        }
        (_, HeadlessTarget::Pipeline(pipeline)) => {
            return Err(format!(
                "missing Skippr config for pipeline '{pipeline}'. Run 'skippr init <project>' first."
            ));
        }
        (_, HeadlessTarget::GenericIdeBootstrap) => generic_chat_config(),
    };

    let authenticated_with_api_key = std::env::var("SKIPPR_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let creds = if let Ok(api_key) = std::env::var("SKIPPR_API_KEY") {
        if api_key.trim().is_empty() {
            return Err("SKIPPR_API_KEY is set but empty.".into());
        }
        let base_url = auth::auth_base_url();
        let client = ApiClient::new(&base_url);
        client
            .exchange_api_key(api_key.trim())
            .await
            .map_err(|e| format!("API key authentication failed: {e}"))?
    } else if let Some(creds) = auth::load_credentials() {
        crate::refresh_user_credentials_or_exit(&ApiClient::new(&auth::auth_base_url()), creds)
            .await
    } else {
        return Err(
            "authentication required: run 'skippr user login' or set SKIPPR_API_KEY.".into(),
        );
    };

    let base_url = auth::auth_base_url();
    let tokens = crate::create_token_provider(&creds);
    let client = ApiClient::authenticated(&base_url, Arc::clone(&tokens));

    if let Err(e) = crate::ensure_eula_accepted(&client, !authenticated_with_api_key).await {
        return Err(e);
    }

    let initial_balance = match client.get_account().await {
        Ok(account) => {
            let bal = account.balance.balance;
            if bal <= 0.0 {
                return Err("balance is $0.00; add funds (skippr user buy-credits).".into());
            }
            if bal < crate::LOW_BALANCE_USD_THRESHOLD {
                eprintln!(
                    "[skippr] WARNING: Low balance (${:.2}). The run may exhaust your balance.",
                    bal
                );
            } else {
                eprintln!("[skippr] balance: ${:.2}", bal);
            }
            bal
        }
        Err(e) => {
            return Err(format!(
                "could not verify account balance ({e}); check login and connectivity."
            ));
        }
    };

    let srv_creds = client
        .get_credentials()
        .await
        .map_err(|e| format!("failed to fetch server credentials: {e}"))?;

    translate::apply_authenticated_overlay(
        &mut internal_file,
        &srv_creds,
        Arc::clone(&tokens),
        initial_balance,
    )
    .map_err(|e| e.to_string())?;

    let mut resolved =
        react_host::resolve_config(internal_file, react::config::ServeOverrides::default())?;
    crate::attach_s3_credentials_provider(&mut resolved, client.clone());

    Ok(HeadlessAuthContext {
        resolved,
        client,
        pipeline: pipeline.unwrap_or_else(|| {
            PipelineName::parse("ide-chat").expect("static generic IDE chat pipeline is valid")
        }),
    })
}

fn generic_chat_config() -> ReactConfigFile {
    let providers = serde_json::json!({
        "warehouse": {
            "kind": "postgres",
            "database": "_skippr_ide_chat_noop",
            "schema": "public"
        },
        "catalog": { "enabled": false },
        "dbt": { "enabled": false },
        "vector": { "enabled": false },
        "el": { "enabled": false },
    });
    ReactConfigFile {
        version: Some(1),
        storage: Some(StorageFile {
            mode: Some("local".into()),
            bucket: None,
            path: Some("./.skippr".into()),
            s3_credentials: None,
        }),
        scope: Some(ScopeFile {
            tenant: Some("_".to_string()),
            workspace: Some("ide".to_string()),
            project_id: Some("ide-chat".to_string()),
        }),
        llm: Some(LlmFile {
            provider: Some("OPENAI_COMPAT".into()),
            base_url: Some("https://api.openai.com".into()),
            reason_model: Some("gpt-5.4".into()),
            task_model: Some("gpt-5.4".into()),
            embed_model: Some("text-embedding-3-small".into()),
            context_length: Some(8192),
            http_timeout_secs: Some(120),
            max_tokens: Some(8192),
            temperature: Some(0.2),
            top_p: Some(1.0),
            ..Default::default()
        }),
        providers: Some(providers),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_chat_config_resolves_for_data_engineer_suite() {
        crate::react_host::resolve_config(
            generic_chat_config(),
            react::config::ServeOverrides::default(),
        )
        .expect("generic IDE chat config must satisfy data-engineer suite config resolution");
    }
}
