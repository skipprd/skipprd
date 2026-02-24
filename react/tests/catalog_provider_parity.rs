use std::sync::Arc;

use react::providers::catalog::DefaultCatalogProvider;
use react_core::keyspace::{DefaultKeyspace, Keyspace};
use react_core::llm::{LargeLanguageModel, NullModel};
use react_core::providers::catalog::types::GlobalSemanticContext;
use react_core::providers::{CatalogProvider, DataCatalog, SemanticModel};
use react_core::scope::RequestScope;
use react_core::storage::{InMemoryStorageAdapter, StorageAdapter};

#[tokio::test]
async fn provider_write_semantic_uses_keyspace_key_and_roundtrips() {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
    let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("bucket".to_string()));
    let llm: Arc<dyn LargeLanguageModel> = Arc::new(NullModel::new());
    let provider = DefaultCatalogProvider::new(storage.clone(), keyspace.clone(), llm, 0, 8);

    let scope = RequestScope {
        tenant: "t".into(),
        workspace: "w".into(),
        project_id: "p".into(),
    };
    let ns = "events";
    let sem = SemanticModel {
        dataset_id: ns.into(),
        catalog: String::new(),
        database: String::new(),
        table: String::new(),
        fields: vec![],
        dimensions: vec![],
        metrics: vec![],
    };

    provider
        .write_semantic(&scope, ns, &sem)
        .await
        .expect("write_semantic");

    let key = keyspace.semantic_key(&scope, ns);
    let raw = storage.get_json(&key).await.expect("semantic stored");
    // Stored as YAML-equivalent JSON; should still deserialize back into SemanticModel.
    let sem2: SemanticModel = serde_json::from_value(raw).expect("semantic deserializable");
    assert_eq!(sem2.dataset_id, ns);
}

#[tokio::test]
async fn provider_write_catalog_uses_keyspace_key_and_roundtrips() {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
    let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("bucket".to_string()));
    let llm: Arc<dyn LargeLanguageModel> = Arc::new(NullModel::new());
    let provider = DefaultCatalogProvider::new(storage.clone(), keyspace.clone(), llm, 0, 8);

    let scope = RequestScope {
        tenant: "t".into(),
        workspace: "w".into(),
        project_id: "p".into(),
    };
    let ns = "events";
    let cat = DataCatalog {
        dataset_id: ns.into(),
        catalog: String::new(),
        database: String::new(),
        table: String::new(),
        description: Some("desc".into()),
        dimensions: vec![],
        metrics: vec![],
        fields: vec![],
        structure_index: Default::default(),
        dataset_stats: None,
        built_at_epoch_secs: None,
    };

    provider
        .write_catalog(&scope, ns, &cat)
        .await
        .expect("write_catalog");

    let key = keyspace.catalog_key(&scope, ns);
    let raw = storage.get_json(&key).await.expect("catalog stored");
    let cat2: DataCatalog = serde_json::from_value(raw).expect("catalog deserializable");
    assert_eq!(cat2.dataset_id, ns);
    assert_eq!(cat2.description.as_deref(), Some("desc"));
}

#[tokio::test]
async fn global_semantic_context_is_written_under_semantic_global_key() {
    #[derive(Default)]
    struct ScriptedLlm {
        replies: std::sync::Mutex<Vec<String>>,
    }
    impl LargeLanguageModel for ScriptedLlm {
        fn chat(
            &self,
            _messages: &[react_core::llm::ChatMessage],
            _options: &react_core::llm::LlmCallOptions,
        ) -> Result<String, String> {
            let mut g = self
                .replies
                .lock()
                .map_err(|_| "mutex poisoned".to_string())?;
            if g.is_empty() {
                return Err("no more replies".to_string());
            }
            Ok(g.remove(0))
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
    }

    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
    let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("bucket".to_string()));
    let llm: Arc<dyn LargeLanguageModel> = Arc::new(ScriptedLlm {
        replies: std::sync::Mutex::new(vec![
            "Likely audience: finance analysts; evidence comes from orders amount/status fields."
                .to_string(),
            serde_json::json!({
                "version": 1,
                "audiences": [
                  {"audience":"finance","confidence":0.9,"evidence":["AwsDataCatalog.db.orders has amount/status fields"]},
                  {"audience":"low_conf_should_drop","confidence":0.5,"evidence":["x"]}
                ],
                "context_bullets": [
                  {"text":"This looks like transactional order/customer data.","confidence":0.92,"evidence":["orders, customers tables"]}
                ],
                "dataset_groups": [],
                "assumptions_and_gaps": []
            }).to_string(),
        ]),
    });

    let scope = RequestScope {
        tenant: "t".into(),
        workspace: "w".into(),
        project_id: "p".into(),
    };

    // Seed two dataset catalogs (minimal fields/description) so the global pass has inputs.
    for ds in ["AwsDataCatalog.db.orders", "AwsDataCatalog.db.customers"] {
        let key = keyspace.catalog_key(&scope, ds);
        storage
            .put_json(
                &key,
                &serde_json::json!({
                    "dataset_id": ds,
                    "catalog": "AwsDataCatalog",
                    "database": "db",
                    "table": ds.rsplit('.').next().unwrap_or(ds),
                    "description": format!("desc for {}", ds),
                    "fields": [
                        {"name":"order_id","type":"string","role":"Id","stats":{"total":100,"nulls":0,"approx_distinct":100}},
                        {"name":"amount","type":"double","role":"Metric","stats":{"total":100,"nulls":0,"min_numeric":1.0,"max_numeric":10.0}}
                    ]
                }),
            )
            .await
            .expect("seed catalog");
    }

    let mut ids: std::collections::HashMap<String, react_core::discover::Metadata> =
        std::collections::HashMap::new();
    ids.insert(
        "AwsDataCatalog.db.orders".to_string(),
        react_core::discover::Metadata::default(),
    );
    ids.insert(
        "AwsDataCatalog.db.customers".to_string(),
        react_core::discover::Metadata::default(),
    );

    react::providers::catalog::enrich::run_llm_global_context_enrichment_all(
        storage.clone(),
        keyspace.clone(),
        llm,
        &scope,
        &ids,
        0,
    )
    .await
    .expect("global context enrichment");

    let gkey = keyspace.semantic_key(
        &scope,
        react_core::providers::catalog::types::GLOBAL_SEMANTIC_DATASET_ID,
    );
    let raw = storage
        .get_json(&gkey)
        .await
        .expect("global semantic stored");
    let ctx: GlobalSemanticContext = serde_json::from_value(raw).expect("deserializable");
    assert!(ctx.audiences.iter().any(|a| a.audience == "finance"));
    assert!(!ctx
        .audiences
        .iter()
        .any(|a| a.audience == "low_conf_should_drop"));
}
