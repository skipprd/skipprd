pub struct SemanticInfer;

impl SemanticInfer {
    pub async fn infer_and_write(namespace: &str) {
        let semantic = crate::semantics::infer::infer_semantic_model(namespace);
        crate::helpers::configuration::Config::write_semantic_async(namespace, &semantic).await;
    }
}


