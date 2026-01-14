use crate::flow_frame::FlowFrame;

pub async fn run(sctx: &crate::suites::SuiteCtx, thread_id: &str, question: &str) -> Result<Vec<FlowFrame>, String> {
    super::super::SkipprModelSuite::run_model(thread_id, question, sctx).await
}

