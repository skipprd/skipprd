use std::sync::Arc;
use datafusion::prelude::{SessionConfig, SessionContext};
use datafusion::datasource::MemTable;
use arrow::record_batch::RecordBatch;

pub struct SessionFactory;

impl SessionFactory {
    pub async fn new_context() -> SessionContext {
        let mut session_config = SessionConfig::new();
        session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
        session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());
        SessionContext::new_with_config(session_config)
    }

    pub async fn register_s3_and_wal(ctx: &SessionContext, namespace: &str) {
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        let _ = crate::sql::tables::register_namespace_view(ctx, &pipeline, namespace).await;
    }
}

