pub struct SemanticInfer;

impl SemanticInfer {
    pub async fn infer_and_write(namespace: &str) {
        let semantic = crate::semantics::infer::infer_semantic_model_async(namespace).await;
        // Debug-print semantic before write
        println!(
            "META: build semantic ns='{}' fields={} sample=[{}]",
            namespace,
            semantic.fields.len(),
            semantic.fields.iter().take(8).map(|f| format!("{}:{:?}", f.name, f.role)).collect::<Vec<_>>().join(",")
        );
        crate::helpers::configuration::Config::write_semantic_async(namespace, &semantic).await;
    }
}


