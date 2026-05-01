pub mod catalog;
pub mod catalog_types;
pub mod dataset_catalog;
pub mod dbt;
pub mod query;
pub mod skippr;
pub mod stats;
pub mod type_parse;
pub mod warehouse;
pub mod warehouse_utils;

/// Default max in-flight concurrency for warehouse/query providers.
pub const DEFAULT_MAX_CONCURRENCY: usize = 15;

pub use catalog::{CatalogEnrichmentReport, CatalogProvider};
pub use catalog_types::{
    AccessDescriptor, CatalogField, DataCatalog, DatasetProfile, DatasetStats, EvidenceStatus,
    FieldProfile, FieldStatsLite, GlobalAssumptionGap, GlobalAudience, GlobalContextBullet,
    GlobalDatasetGroup, GlobalSemanticContext, KeyCandidateProfile, ProfileMetric,
    RelationshipCandidateProfile, SemanticClaimKind, SemanticClaimRef, SemanticField,
    SemanticFieldRole, SemanticModel, SemanticProfile, StructureKind, GLOBAL_SEMANTIC_DATASET_ID,
};
pub use dataset_catalog::{DatasetCatalogProvider, DatasetId};
pub use dbt::{DbtProvider, DbtValidateArgs, DbtValidateResult};
pub use query::{QueryProvider, QueryResult};
pub use skippr::{
    SchemaSinkResolvedConfig, SkipprDiscoverResult, SkipprFieldSchema, SkipprNamespaceStatus,
    SkipprOutputConfig, SkipprPipelineConfig, SkipprPipelineStatus, SkipprProvider,
    SkipprSyncResult,
};
pub use stats::DatasetFieldStats;
pub use warehouse::{WarehouseNaming, WarehouseProvider};
