use crate::react::flow_frame::FlowFrame;

pub async fn run(sctx: &crate::react::suites::SuiteCtx, thread_id: &str, question: &str) -> Result<Vec<FlowFrame>, String> {
    super::super::SkipprAskSuite::run_ask(thread_id, question, sctx).await
}

