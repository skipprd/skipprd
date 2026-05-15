//! Shared authentication + react resolution for headless agent runs (`skippr model`, `skippr chat`).

use std::path::PathBuf;
use std::sync::Arc;

use react_core::resolved_config::ReactResolvedConfig;

use crate::api_client::ApiClient;
use crate::auth;
use crate::react_host;
use crate::translate;

pub struct HeadlessAuthContext {
    pub resolved: ReactResolvedConfig,
    pub client: ApiClient,
    pub pipeline: String,
}

/// Authenticate, overlay server credentials, resolve react config, and attach the refreshable S3
/// credentials provider — same spine as `skippr model`.
pub async fn authenticate_headless_for_pipeline(
    explicit_config: &Option<PathBuf>,
    pipeline: &str,
) -> Result<HeadlessAuthContext, String> {
    let engine_cfg = crate::load_cli_execution_config(explicit_config)
        .map_err(|e| format!("{e}\nRun 'skippr init <project>' first."))?;
    let mut internal_file =
        crate::react_config_from_pipeline_config(&engine_cfg, pipeline).map_err(|e| e)?;

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

    let mut resolved = react_host::resolve_config(internal_file, react::config::ServeOverrides::default())?;
    crate::attach_s3_credentials_provider(&mut resolved, client.clone());

    Ok(HeadlessAuthContext {
        resolved,
        client,
        pipeline: pipeline.to_string(),
    })
}
