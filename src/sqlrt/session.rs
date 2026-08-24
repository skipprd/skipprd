use std::time::Duration;

use datafusion::prelude::{SessionConfig, SessionContext};

use crate::sqlrt::udfs;

/// Collect timeout for user-SQL sessions (B.19). Not an env knob.
pub const OTEL_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Construct a user-SQL `SessionContext` with observability UDFs registered.
///
/// Callers pass pipeline names into `register_namespace_view`. This factory
/// MUST NOT read `PIPELINE_NAME`.
pub fn build_query_context(config: SessionConfig) -> SessionContext {
    let ctx = SessionContext::new_with_config(config);
    udfs::register_observability_udfs(&ctx);
    ctx
}

/// Collect a user-SQL dataframe under the session timeout (B.19).
pub async fn collect_user_sql(
    df: datafusion::dataframe::DataFrame,
) -> datafusion::error::Result<Vec<datafusion::arrow::array::RecordBatch>> {
    collect_with_timeout(df.collect(), OTEL_QUERY_TIMEOUT).await
}

/// Timeout wrapper with injected duration for tests (B.19).
pub async fn collect_with_timeout<T, F>(fut: F, timeout: Duration) -> datafusion::error::Result<T>
where
    F: std::future::Future<Output = datafusion::error::Result<T>>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(result) => result,
        Err(_) => Err(datafusion::error::DataFusionError::Execution(format!(
            "query exceeded {}s timeout",
            timeout.as_secs().max(1)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::prelude::SessionConfig;

    #[tokio::test]
    async fn factory_context_selects_one() {
        let ctx = build_query_context(SessionConfig::new());
        let batches = ctx.sql("SELECT 1").await.unwrap().collect().await.unwrap();
        assert_eq!(batches[0].num_rows(), 1);
    }

    #[test]
    fn query_and_engine_use_factory() {
        let query = include_str!("query.rs");
        let engine = include_str!("../engine.rs");
        assert!(
            !query.contains("SessionContext::new_with_config"),
            "query.rs must call build_query_context"
        );
        assert!(
            !engine.contains("SessionContext::new_with_config"),
            "engine.rs must call build_query_context"
        );
    }

    #[test]
    fn query_timeout_is_thirty_seconds() {
        assert_eq!(OTEL_QUERY_TIMEOUT, Duration::from_secs(30));
        let ballista = include_str!("../query_flight/ballista.rs");
        assert!(
            ballista.contains("register_observability_udfs"),
            "Ballista must register observability UDFs before BallistaFunctionRegistry"
        );
        let query = include_str!("query.rs");
        assert!(
            query.contains("collect_user_sql"),
            "query.rs user SQL must use collect_user_sql"
        );
    }

    #[tokio::test]
    async fn elapsed_timeout_is_error_without_partial_batches() {
        let err = collect_with_timeout(
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok::<Vec<datafusion::arrow::array::RecordBatch>, _>(vec![])
            },
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err}");
    }
}
