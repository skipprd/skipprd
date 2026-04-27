use std::sync::Arc;

use react_core::keyspace::encode_key_component;
use react_core::keyspace::Keyspace;
use react_core::scope::RequestScope;
use react_core::storage::StorageAdapter;

use crate::types::SemanticModel;

pub async fn infer_and_write_semantic(
    storage: Arc<dyn StorageAdapter>,
    keyspace: Arc<dyn Keyspace>,
    scope: &RequestScope,
    namespace: &str,
) -> Result<SemanticModel, String> {
    let semantic = crate::infer::infer_semantic_model_async(
        storage.clone(),
        keyspace.clone(),
        scope,
        namespace,
    )
    .await;
    write_semantic(storage, keyspace, scope, namespace, &semantic).await?;
    Ok(semantic)
}

pub async fn write_semantic(
    storage: Arc<dyn StorageAdapter>,
    keyspace: Arc<dyn Keyspace>,
    scope: &RequestScope,
    namespace: &str,
    semantic: &SemanticModel,
) -> Result<(), String> {
    let key = keyspace.scoped_key(
        scope,
        &[
            "semantic",
            &format!("{}.yaml", encode_key_component(namespace)),
        ],
    );
    let json_equiv = crate::utils::yaml_to_json_value(semantic)?;
    storage
        .put_json(&key, &json_equiv)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
