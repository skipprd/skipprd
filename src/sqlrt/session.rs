use std::time::Duration;

use datafusion::prelude::{SessionConfig, SessionContext};

use crate::sqlrt::udfs;

/// Collect timeout for user-SQL sessions (B.19). Not an env knob.
pub const OTEL_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Isolated UDF-registration helper for tests that do not start QueryRuntime.
/// User SQL uses [`coordinating_query_context`].
pub fn build_query_context(config: SessionConfig) -> SessionContext {
    let ctx = SessionContext::new_with_config(config);
    udfs::register_observability_udfs(&ctx);
    ctx
}

/// User SQL SessionContext: QueryRuntime Worker + distributed planner.
pub async fn coordinating_query_context() -> datafusion::error::Result<SessionContext> {
    crate::query_flight::runtime::ensure_started()
        .await
        .map_err(|err| datafusion::error::DataFusionError::Execution(err.to_string()))?;
    crate::query_flight::runtime::query_context()
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
            query.contains("coordinating_query_context"),
            "query.rs must call coordinating_query_context"
        );
        assert!(
            !query.contains("SessionContext::new_with_config"),
            "query.rs must not construct a second SessionContext factory"
        );
        assert!(
            engine.contains("coordinating_query_context"),
            "engine.rs must call coordinating_query_context"
        );
        assert!(
            !engine.contains("SessionContext::new_with_config"),
            "engine.rs must not construct a second SessionContext factory"
        );
        let tables = include_str!("tables.rs");
        let tables = tables.split("#[cfg(test)]").next().unwrap();
        assert!(
            !tables.contains("plan_clustered_select"),
            "clustered query planning is deleted"
        );
        let api = include_str!("../api.rs");
        let collect = api
            .split("async fn collect_namespace")
            .nth(1)
            .unwrap()
            .split("/// `PIPELINE_NAME`")
            .next()
            .unwrap();
        assert!(
            collect.contains("coordinating_query_context"),
            "collect_namespace must call coordinating_query_context"
        );
        assert!(
            !collect.contains("build_query_context"),
            "collect_namespace must not use the test-only SessionContext factory"
        );
        let live = include_str!("../query_flight/service.rs")
            .split("async fn execute_live_wal_ticket")
            .nth(1)
            .unwrap()
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(
            !live.contains("query_context"),
            "LiveWalTicket DoGet must not use QueryRuntime"
        );
        assert!(
            !live.contains("ensure_started"),
            "LiveWalTicket DoGet must not start QueryRuntime"
        );
        assert!(
            live.contains("pinned_slice"),
            "owner DoGet must exact-get Index+Segment, not re-select live_slices"
        );
        assert!(
            !live.contains("live_slices"),
            "owner DoGet must not re-select W"
        );
    }

    #[test]
    fn query_timeout_is_thirty_seconds() {
        assert_eq!(OTEL_QUERY_TIMEOUT, Duration::from_secs(30));
        let runtime = include_str!("../query_flight/runtime.rs");
        assert!(
            runtime.contains("register_observability_udfs"),
            "QueryRuntime must register observability UDFs on the coordinating context"
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
