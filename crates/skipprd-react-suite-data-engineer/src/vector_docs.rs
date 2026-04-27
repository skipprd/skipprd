use react_core::provider_traits::{TypedVectorDocument, VectorCollection};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub struct ManualVectorCollection;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ManualVectorMetadata {
    pub kind: String,
    #[serde(default)]
    pub dataset_id: Option<String>,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub extra: Value,
}

impl VectorCollection for ManualVectorCollection {
    type Metadata = ManualVectorMetadata;

    const NAMESPACE: &'static str = "manual_vector_item";
}

pub type ManualVectorDocument = TypedVectorDocument<ManualVectorCollection>;

pub struct CatalogNoteCollection;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CatalogNoteMetadata {
    pub dataset_id: String,
    #[serde(default)]
    pub field: Option<String>,
    pub level: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl VectorCollection for CatalogNoteCollection {
    type Metadata = CatalogNoteMetadata;

    const NAMESPACE: &'static str = "catalog_note";
}

pub type CatalogNoteDocument = TypedVectorDocument<CatalogNoteCollection>;

pub struct RepairMemoryCollection;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RepairMemoryMetadata {
    pub outcome: String,
    #[serde(default)]
    pub files_changed: Vec<String>,
}

impl VectorCollection for RepairMemoryCollection {
    type Metadata = RepairMemoryMetadata;

    const NAMESPACE: &'static str = "repair_memory";
}

pub type RepairMemoryDocument = TypedVectorDocument<RepairMemoryCollection>;
