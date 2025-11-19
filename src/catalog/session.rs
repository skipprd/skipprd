use datafusion::prelude::{SessionConfig, SessionContext};

pub struct SessionFactory;

impl SessionFactory {
    pub async fn new_context() -> SessionContext {
        let session_config = SessionConfig::new();
        SessionContext::new_with_config(session_config)
    }

    #[allow(dead_code)]
    pub async fn register_s3_and_wal(ctx: &SessionContext, namespace: &str) {
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        let pairs = vec![(pipeline.clone(), namespace.to_string())];
        crate::ws::agent_runner::pre_register_selected_namespaces(ctx, &pairs).await;
    }
}

