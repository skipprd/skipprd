//! Read-only semantic search against Skippr-hosted public documentation vectors (S3 + LanceDB).

use react_core::resolved_config::{ReactResolvedConfig, S3Credentials};
use react_module_provider_vector_lance::lance_store::LanceDbStore;

use crate::api_client::{CredentialsResponse, StsCreds};
use crate::react_host::vector::lance_storage_options_from_credentials;

const DEFAULT_PUBLIC_VECTORS_BUCKET: &str = "skippr-public-vectors-prod";
/// Key under the public vectors bucket (see product docs index).
pub const PUBLIC_DOCS_LANCE_SUFFIX: &str = "skippr-docs/lancedb/embeddings_v2.lance";

fn sts_to_s3_credentials(sts: &StsCreds, region: &str) -> S3Credentials {
    let expires_at = chrono::DateTime::parse_from_rfc3339(&sts.expiration)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc));
    S3Credentials {
        access_key_id: sts.access_key_id.clone(),
        secret_access_key: sts.secret_access_key.clone(),
        session_token: Some(sts.session_token.clone()),
        region: region.to_string(),
        expires_at,
        provider: None,
    }
}

/// Embed `query_text` using the suite LLM and run a vector search on the public docs Lance table.
pub async fn search_public_skippr_docs(
    resolved: &ReactResolvedConfig,
    creds_response: &CredentialsResponse,
    query_text: &str,
    k: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let sts = creds_response.knowledge_credentials.as_ref().ok_or_else(|| {
        "server credentials did not include knowledge_credentials (needed for public docs Lance)"
            .to_string()
    })?;
    let bucket = creds_response
        .public_vectors_bucket
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(DEFAULT_PUBLIC_VECTORS_BUCKET);
    let region = "us-east-1";
    let s3creds = sts_to_s3_credentials(sts, region);
    let opts = lance_storage_options_from_credentials(&s3creds);
    let uri = format!(
        "s3://{}/{}",
        bucket.trim_end_matches('/'),
        PUBLIC_DOCS_LANCE_SUFFIX
    );
    let store = LanceDbStore::new(&uri).with_storage_options(opts);

    let sctx = react::bootstrap::build_base_suite_ctx(resolved)
        .await
        .map_err(|e| format!("bootstrap suite ctx: {e}"))?;
    let embeddings = sctx
        .llm_embed(&[query_text.to_string()])
        .map_err(|e| format!("embed failed: {e}"))?;
    let qvec = embeddings
        .into_iter()
        .next()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "empty query embedding".to_string())?;

    let chunks = store.query(&qvec, k, None).await?;
    Ok(chunks
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "id": c.item.id,
                "namespace": c.item.namespace,
                "text": c.item.text,
                "metadata_json": c.item.metadata_json,
                "score": c.score,
            })
        })
        .collect())
}
