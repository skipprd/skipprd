use std::sync::Arc;

use react::adapters::storage::{InMemoryStorageAdapter, StorageAdapter};
use react::providers::{DefaultKeyspace, Keyspace, RequestScope};
use react::providers::catalog::{CatalogProvider, SkipprCatalogProvider};
use react::providers::catalog::types::{SemanticModel, DataCatalog};

#[tokio::test]
async fn provider_write_semantic_uses_keyspace_key_and_roundtrips() {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
    let keyspace: Arc<dyn Keyspace> = Arc::new(DefaultKeyspace::new("bucket".to_string()));
    let llm: Arc<dyn react::llm::LargeLanguageModel> = Arc::new(react::llm::NullModel::new());
    let provider = SkipprCatalogProvider::new(storage.clone(), keyspace.clone(), llm, 0, 8);

    let scope = RequestScope { tenant: "t".into(), workspace: "w".into(), project_id: "p".into() };
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

    provider.write_semantic(&scope, ns, &sem).await.expect("write_semantic");

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
    let llm: Arc<dyn react::llm::LargeLanguageModel> = Arc::new(react::llm::NullModel::new());
    let provider = SkipprCatalogProvider::new(storage.clone(), keyspace.clone(), llm, 0, 8);

    let scope = RequestScope { tenant: "t".into(), workspace: "w".into(), project_id: "p".into() };
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
    };

    provider.write_catalog(&scope, ns, &cat).await.expect("write_catalog");

    let key = keyspace.catalog_key(&scope, ns);
    let raw = storage.get_json(&key).await.expect("catalog stored");
    let cat2: DataCatalog = serde_json::from_value(raw).expect("catalog deserializable");
    assert_eq!(cat2.dataset_id, ns);
    assert_eq!(cat2.description.as_deref(), Some("desc"));
}

