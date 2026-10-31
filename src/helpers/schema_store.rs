//! Shredded `schema` family. Replaces whole-document `metadata.json` authority.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use skippr_wal_catalog::{
    CatalogFence, KvOp, SkipprStore, SkipprStoreRead, StoreFamily, STORE_FORMAT_VERSION,
};

use crate::discover::{Metadata, PipelineMetadata};

const IMPORT_CURSOR: &[u8] = b"schema_import/cursor";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Eq, PartialEq)]
struct SchemaHead {
    name: String,
    enabled: bool,
    flattened: bool,
    metadata_version: u32,
    schema_id: u64,
    namespaces: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct SchemaNode {
    node: Metadata,
    children: Vec<(String, i32)>,
}

fn head_rest(pipeline: &str) -> Vec<u8> {
    format!("{pipeline}/head").into_bytes()
}

fn contract_rest(pipeline: &str, namespace: &str) -> Vec<u8> {
    format!("{pipeline}/ns/{namespace}/contract").into_bytes()
}

fn node_rest(pipeline: &str, namespace: &str, field_id: i32) -> Vec<u8> {
    format!("{pipeline}/ns/{namespace}/node/{field_id}").into_bytes()
}

fn root_rest(pipeline: &str, namespace: &str) -> Vec<u8> {
    format!("{pipeline}/ns/{namespace}/root").into_bytes()
}

fn pack_node(meta: &Metadata) -> SchemaNode {
    let children: Vec<(String, i32)> = meta
        .fields
        .iter()
        .map(|(name, child)| (name.clone(), child.field_id))
        .collect();
    let mut node = meta.clone();
    node.fields = Box::new(HashMap::new());
    SchemaNode { node, children }
}

fn collect_nodes(meta: &Metadata, out: &mut Vec<(i32, SchemaNode)>) {
    out.push((meta.field_id, pack_node(meta)));
    for child in meta.fields.values() {
        collect_nodes(child, out);
    }
}

pub fn persist_pipeline_schema(
    store: &dyn SkipprStore,
    fence: CatalogFence,
    metadata: &PipelineMetadata,
) -> Result<(), String> {
    let pipeline = &metadata.name;
    let mut desired: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let head = SchemaHead {
        name: metadata.name.clone(),
        enabled: metadata.enabled,
        flattened: metadata.flattened,
        metadata_version: metadata.metadata_version,
        schema_id: metadata.schema_id,
        namespaces: metadata.metadata.keys().cloned().collect(),
    };
    desired.insert(
        StoreFamily::Schema.key(&head_rest(pipeline)),
        serde_json::to_vec(&head).map_err(|err| err.to_string())?,
    );
    for (namespace, root) in &metadata.metadata {
        desired.insert(
            StoreFamily::Schema.key(&root_rest(pipeline, namespace)),
            serde_json::to_vec(&pack_node(root)).map_err(|err| err.to_string())?,
        );
        let mut nodes = Vec::new();
        collect_nodes(root, &mut nodes);
        for (field_id, node) in nodes {
            desired.insert(
                StoreFamily::Schema.key(&node_rest(pipeline, namespace, field_id)),
                serde_json::to_vec(&node).map_err(|err| err.to_string())?,
            );
        }
        if let Some(contract) = metadata.source_contracts.get(namespace) {
            desired.insert(
                StoreFamily::Schema.key(&contract_rest(pipeline, namespace)),
                serde_json::to_vec(contract).map_err(|err| err.to_string())?,
            );
        }
    }
    let prefix = StoreFamily::Schema.key(format!("{pipeline}/").as_bytes());
    let existing = store
        .query_prefix(&prefix, None, usize::MAX)
        .map_err(|err| err.to_string())?;
    let mut ops = Vec::new();
    for (key, value) in &desired {
        let unchanged = existing.iter().any(|(k, v)| k == key && v == value);
        if !unchanged {
            ops.push(KvOp::Put {
                key: key.clone(),
                value: value.clone(),
            });
        }
    }
    for (key, _) in existing {
        if !desired.contains_key(&key) {
            ops.push(KvOp::Delete { key });
        }
    }
    if !ops.is_empty() {
        store.commit(&ops, fence).map_err(|err| err.to_string())?;
        store.flush_durable().map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn load_node(
    store: &dyn SkipprStoreRead,
    pipeline: &str,
    namespace: &str,
    field_id: i32,
) -> Result<Metadata, String> {
    let bytes = store
        .get(&StoreFamily::Schema.key(&node_rest(pipeline, namespace, field_id)))
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("missing schema node {pipeline}/{namespace}/{field_id}"))?;
    let packed: SchemaNode = serde_json::from_slice(&bytes).map_err(|err| err.to_string())?;
    let mut meta = packed.node;
    for (name, child_id) in packed.children {
        meta.fields
            .insert(name, load_node(store, pipeline, namespace, child_id)?);
    }
    Ok(meta)
}

pub fn load_pipeline_schema(
    store: &dyn SkipprStoreRead,
    pipeline: &str,
) -> Result<Option<PipelineMetadata>, String> {
    let Some(head_bytes) = store
        .get(&StoreFamily::Schema.key(&head_rest(pipeline)))
        .map_err(|err| err.to_string())?
    else {
        return Ok(None);
    };
    let head: SchemaHead = serde_json::from_slice(&head_bytes).map_err(|err| err.to_string())?;
    let mut metadata = HashMap::new();
    let mut source_contracts = HashMap::new();
    for namespace in &head.namespaces {
        let root_bytes = store
            .get(&StoreFamily::Schema.key(&root_rest(pipeline, namespace)))
            .map_err(|err| err.to_string())?
            .ok_or_else(|| format!("missing schema root {pipeline}/{namespace}"))?;
        let packed: SchemaNode =
            serde_json::from_slice(&root_bytes).map_err(|err| err.to_string())?;
        let mut root = packed.node;
        for (name, child_id) in packed.children {
            root.fields
                .insert(name, load_node(store, pipeline, namespace, child_id)?);
        }
        metadata.insert(namespace.clone(), root);
        if let Some(bytes) = store
            .get(&StoreFamily::Schema.key(&contract_rest(pipeline, namespace)))
            .map_err(|err| err.to_string())?
        {
            let contract = serde_json::from_slice(&bytes).map_err(|err| err.to_string())?;
            source_contracts.insert(namespace.clone(), contract);
        }
    }
    Ok(Some(PipelineMetadata {
        name: head.name,
        metadata,
        enabled: head.enabled,
        flattened: head.flattened,
        metadata_version: head.metadata_version,
        schema_id: head.schema_id,
        source_contracts,
    }))
}

pub fn import_metadata_json_if_needed(
    store: &dyn SkipprStore,
    fence: CatalogFence,
    pipeline: &str,
    local: Option<&[u8]>,
    remote: Option<&[u8]>,
) -> Result<Option<PipelineMetadata>, String> {
    if load_pipeline_schema(store, pipeline)?.is_some() {
        return Ok(None);
    }
    let chosen = match (local, remote) {
        (Some(a), Some(b)) if Sha256::digest(a) != Sha256::digest(b) => {
            return Err(
                "local and S3 metadata.json checksums differ; fail closed (no winner)".into(),
            );
        }
        (Some(a), Some(_)) | (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return Ok(None),
    };
    let metadata: PipelineMetadata =
        serde_json::from_slice(chosen).map_err(|err| format!("metadata.json parse: {err}"))?;
    persist_pipeline_schema(store, fence, &metadata)?;
    store
        .commit(
            &[KvOp::Put {
                key: StoreFamily::StoreMeta.key(IMPORT_CURSOR),
                value: STORE_FORMAT_VERSION.to_le_bytes().to_vec(),
            }],
            fence,
        )
        .map_err(|err| err.to_string())?;
    tracing::info!(pipeline, "imported metadata.json into schema family");
    Ok(Some(metadata))
}

pub fn checksum_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::configuration::Config;

    #[test]
    fn shredded_schema_roundtrips_and_skips_unchanged_puts() {
        let fence = CatalogFence::local();
        let store = skippr_wal_catalog::MemorySkipprStore::open(fence);
        let mut md = PipelineMetadata::new(&Config::new());
        md.name = "orders".into();
        let mut root = Metadata::new().unwrap();
        root.field_id = 1;
        root.out_field_name = "id".into();
        let mut child = Metadata::new().unwrap();
        child.field_id = 2;
        child.out_field_name = "amount".into();
        root.fields.insert("amount".into(), child);
        md.metadata.insert("trips".into(), root);
        persist_pipeline_schema(&store, fence, &md).unwrap();
        let loaded = load_pipeline_schema(&store, "orders").unwrap().unwrap();
        assert_eq!(loaded.schema_id, md.schema_id);
        assert_eq!(
            loaded.metadata["trips"].fields["amount"].out_field_name,
            "amount"
        );
        persist_pipeline_schema(&store, fence, &md).unwrap();
        let page =
            SkipprStoreRead::query_prefix(&store, StoreFamily::Schema.prefix(), None, usize::MAX)
                .unwrap();
        assert!(page.iter().any(|(k, _)| k.ends_with(b"/head")));
        assert!(page.len() >= 3);
    }

    #[test]
    fn checksum_mismatch_fail_closed() {
        let fence = CatalogFence::local();
        let store = skippr_wal_catalog::MemorySkipprStore::open(fence);
        let err = import_metadata_json_if_needed(
            &store,
            fence,
            "p",
            Some(br#"{"name":"a","metadata":{},"enabled":true}"#),
            Some(br#"{"name":"b","metadata":{},"enabled":true}"#),
        )
        .unwrap_err();
        assert!(err.contains("checksums differ"), "{err}");
    }
}
