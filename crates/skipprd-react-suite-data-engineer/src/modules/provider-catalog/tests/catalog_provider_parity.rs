use std::sync::Arc;

use react_core::keyspace::{encode_key_component, DefaultKeyspace, Keyspace};
use react_core::llm::{LargeLanguageModel, NullModel};
use react_core::scope::RequestScope;
use react_core::storage::StorageAdapter;
use react_module_provider_catalog::DefaultCatalogProvider;
use react_module_storage_memory::InMemoryStorageAdapter;
use react_suite_data_engineer::providers::{
    CatalogProvider, DataCatalog, DatasetProfile, EvidenceStatus, GlobalSemanticContext,
    SemanticModel, SemanticProfile,
};

#[tokio::test]
async fn provider_write_semantic_uses_keyspace_key_and_roundtrips() {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
    let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("bucket".to_string()));
    let llm: Arc<dyn LargeLanguageModel> = Arc::new(NullModel::new());
    let provider = DefaultCatalogProvider::new(storage.clone(), keyspace.clone(), llm, 0, 8);

    let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
    let ns = "AwsDataCatalog.db.events";
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

    let key = keyspace.scoped_key(
        &scope,
        &["semantic", &format!("{}.yaml", encode_key_component(ns))],
    );
    let raw = storage.get_json(&key).await.expect("semantic stored");
    let sem2: SemanticModel = serde_json::from_value(raw).expect("semantic deserializable");
    assert_eq!(sem2.dataset_id, ns);
}

#[tokio::test]
async fn provider_write_catalog_uses_keyspace_key_and_roundtrips() {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
    let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("bucket".to_string()));
    let llm: Arc<dyn LargeLanguageModel> = Arc::new(NullModel::new());
    let provider = DefaultCatalogProvider::new(storage.clone(), keyspace.clone(), llm, 0, 8);

    let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
    let ns = "AwsDataCatalog.db.events";
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

    let key = keyspace.scoped_key(
        &scope,
        &["catalog", &format!("{}.yaml", encode_key_component(ns))],
    );
    let raw = storage.get_json(&key).await.expect("catalog stored");
    let cat2: DataCatalog = serde_json::from_value(raw).expect("catalog deserializable");
    assert_eq!(cat2.dataset_id, ns);
    assert_eq!(cat2.description.as_deref(), Some("desc"));
}

#[tokio::test]
async fn provider_write_semantic_profile_uses_keyspace_key_and_roundtrips() {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
    let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("bucket".to_string()));
    let llm: Arc<dyn LargeLanguageModel> = Arc::new(NullModel::new());
    let provider = DefaultCatalogProvider::new(storage.clone(), keyspace.clone(), llm, 0, 8);

    let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");
    let ns = "AwsDataCatalog.db.events";
    let profile = SemanticProfile {
        version: 1,
        built_at_epoch_secs: Some(1),
        dataset_profiles: vec![DatasetProfile {
            dataset_id: ns.to_string(),
            row_count: Some(10),
            fields: vec![],
            key_candidates: vec![],
            relationship_candidates: vec![],
        }],
        notes: vec!["aggregate-only".to_string()],
    };

    provider
        .write_semantic_profile(&scope, ns, &profile)
        .await
        .expect("write_semantic_profile");

    let key = keyspace.scoped_key(
        &scope,
        &[
            "semantic_profile",
            &format!("{}.yaml", encode_key_component(ns)),
        ],
    );
    let raw = storage
        .get_json(&key)
        .await
        .expect("semantic profile stored");
    let profile2: SemanticProfile = serde_json::from_value(raw).expect("deserializable");
    assert_eq!(profile2.dataset_profiles[0].dataset_id, ns);
    assert_eq!(
        provider
            .read_semantic_profile(&scope, ns)
            .await
            .expect("read_semantic_profile")
            .expect("profile")
            .dataset_profiles[0]
            .row_count,
        Some(10)
    );
    assert!(EvidenceStatus::Observed.authoring_safe());
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

    let scope = RequestScope::parse("t", "w", "p").expect("valid test scope");

    for ds in ["AwsDataCatalog.db.orders", "AwsDataCatalog.db.customers"] {
        let key = keyspace.scoped_key(
            &scope,
            &["catalog", &format!("{}.yaml", encode_key_component(ds))],
        );
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

    let ids: std::collections::HashSet<String> = [
        "AwsDataCatalog.db.orders".to_string(),
        "AwsDataCatalog.db.customers".to_string(),
    ]
    .into_iter()
    .collect();

    react_module_provider_catalog::enrich::run_llm_global_context_enrichment_all(
        storage.clone(),
        keyspace.clone(),
        llm,
        &scope,
        &ids,
        0,
    )
    .await
    .expect("global context enrichment");

    let gkey = keyspace.scoped_key(
        &scope,
        &[
            "semantic",
            &format!(
                "{}.yaml",
                encode_key_component(
                    react_suite_data_engineer::providers::GLOBAL_SEMANTIC_DATASET_ID
                )
            ),
        ],
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
