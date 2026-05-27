use std::collections::{BTreeMap, BTreeSet};

use react_core::storage::{retry_get_bytes, retry_list_prefix};
use react_core::suite::SuiteCtx;
use serde_json::Value;

use crate::ctx_ext::{
    sctx_catalog, sctx_datasets, sctx_skippr, sctx_skipprd_metadata, sctx_warehouse,
    ProvidersCfgCap, SkipprdMetadataLocation,
};
use crate::lineage_sql::{analyze_select_sql, SqlSelectedOutput};
use crate::lineage_store::{
    matching_field_node_ids, merge_lineage_node, slice_graph, strip_query_history_evidence,
    LineageStore,
};
use crate::lineage_types::{
    canonical_dataset_id, canonical_field_path, dataset_node_id, edge_id, field_node_id,
    LineageDiagnostic, LineageDiagnosticSeverity, LineageEdge, LineageEdgeKind,
    LineageEvidenceSource, LineageFieldRef, LineageGraphQuery, LineageGraphSnapshot, LineageNode,
    LineageNodeId, LineageNodeKind, LineageProvenance, LineageRefreshResult, LineageResourceKind,
    LineageResourceRef, QueryHistoryImportSummary, LINEAGE_META_RESOURCES,
};
use crate::providers::{
    DataCatalog, DatasetCatalogProvider, EvidenceStatus, QueryHistoryRecord, QueryHistoryRequest,
    SkipprFieldSchema, SkipprNamespaceStatus, SkipprPipelineStatus,
};
use crate::PipelineName;

#[derive(Clone, Debug)]
pub struct LineageBuildOptions {
    pub pipeline: PipelineName,
    pub include_query_history: bool,
    pub query_history_since: Option<String>,
    pub query_history_limit: usize,
}

#[derive(Default)]
struct GraphBuilder {
    nodes: BTreeMap<LineageNodeId, LineageNode>,
    edges: BTreeMap<String, LineageEdge>,
    diagnostics: Vec<LineageDiagnostic>,
}

impl GraphBuilder {
    fn add_node(&mut self, node: LineageNode) -> LineageNodeId {
        let id = node.id.clone();
        self.nodes
            .entry(id.clone())
            .and_modify(|existing| merge_lineage_node(existing, &node))
            .or_insert(node);
        id
    }

    fn add_edge(&mut self, edge: LineageEdge) {
        self.edges.insert(edge.id.0.clone(), edge);
    }

    fn warn(&mut self, message: impl Into<String>, source: Option<LineageEvidenceSource>) {
        self.add_diagnostic(LineageDiagnostic {
            severity: LineageDiagnosticSeverity::Warning,
            message: message.into(),
            source,
        });
    }

    fn info(&mut self, message: impl Into<String>, source: Option<LineageEvidenceSource>) {
        self.add_diagnostic(LineageDiagnostic {
            severity: LineageDiagnosticSeverity::Info,
            message: message.into(),
            source,
        });
    }

    fn add_diagnostic(&mut self, diagnostic: LineageDiagnostic) {
        if !self.diagnostics.contains(&diagnostic) {
            self.diagnostics.push(diagnostic);
        }
    }

    fn field_paths_for_dataset(&self, dataset_id: &str) -> Vec<String> {
        let dataset_id = canonical_dataset_id(dataset_id);
        self.nodes
            .values()
            .filter(|node| node.kind == LineageNodeKind::Field)
            .filter_map(|node| node.field.as_ref())
            .filter(|field| canonical_dataset_id(&field.dataset_id) == dataset_id)
            .map(|field| canonical_field_path(&field.field_path))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn finish(self) -> LineageGraphSnapshot {
        LineageGraphSnapshot {
            nodes: self.nodes.into_values().collect(),
            edges: self.edges.into_values().collect(),
            diagnostics: self.diagnostics,
            ..Default::default()
        }
    }

    fn from_snapshot(mut graph: LineageGraphSnapshot) -> Self {
        let diagnostics = std::mem::take(&mut graph.diagnostics);
        let mut builder = Self::default();
        for node in graph.nodes {
            builder.add_node(node);
        }
        for edge in graph.edges {
            builder.add_edge(edge);
        }
        builder.diagnostics = diagnostics;
        builder
    }

    fn has_edge_id(&self, edge_id: &str) -> bool {
        self.edges.contains_key(edge_id)
    }
}

fn add_field_node(
    builder: &mut GraphBuilder,
    dataset_id: &str,
    field_path: &str,
    label: impl Into<String>,
    path: Option<String>,
    metadata: BTreeMap<String, String>,
) -> LineageNodeId {
    let dataset_id = canonical_dataset_id(dataset_id);
    let field_path = canonical_field_path(field_path);
    let id = field_node_id(&dataset_id, &field_path);
    builder.add_node(LineageNode {
        id: id.clone(),
        label: label.into(),
        kind: LineageNodeKind::Field,
        dataset_id: Some(dataset_id.clone()),
        field: Some(LineageFieldRef {
            dataset_id,
            field_path,
            field_id: None,
        }),
        path,
        metadata,
    });
    id
}

fn add_contains_field_edge(
    builder: &mut GraphBuilder,
    entity_id: &LineageNodeId,
    field_id: &LineageNodeId,
    provenance: LineageProvenance,
) {
    builder.add_edge(LineageEdge {
        id: edge_id(LineageEdgeKind::ContainsField, entity_id, field_id),
        from_node_id: entity_id.clone(),
        to_node_id: field_id.clone(),
        kind: LineageEdgeKind::ContainsField,
        provenance,
        metadata: BTreeMap::new(),
    });
}

fn add_field_lineage_edge(
    builder: &mut GraphBuilder,
    kind: LineageEdgeKind,
    from_field_id: &LineageNodeId,
    to_field_id: &LineageNodeId,
    provenance: LineageProvenance,
    metadata: BTreeMap<String, String>,
) {
    builder.add_edge(LineageEdge {
        id: edge_id(kind.clone(), from_field_id, to_field_id),
        from_node_id: from_field_id.clone(),
        to_node_id: to_field_id.clone(),
        kind,
        provenance,
        metadata,
    });
}

#[derive(Clone, Debug, Default)]
struct RelationResolver {
    aliases: BTreeMap<String, String>,
    default_container: Option<String>,
    default_namespace: Option<String>,
    warehouse_metadata: BTreeMap<String, String>,
}

impl RelationResolver {
    fn new(cfg: Option<&crate::de_config::ProvidersResolved>, manifest: Option<&Value>) -> Self {
        let mut resolver = Self {
            aliases: BTreeMap::new(),
            default_container: cfg
                .map(|cfg| canonical_dataset_id(&cfg.warehouse.container))
                .filter(|value| !value.is_empty()),
            default_namespace: cfg
                .map(|cfg| canonical_dataset_id(&cfg.warehouse.namespace))
                .filter(|value| !value.is_empty()),
            warehouse_metadata: warehouse_node_metadata(cfg),
        };
        if let Some(manifest) = manifest {
            resolver.add_manifest(manifest);
        }
        resolver
    }

    fn add_manifest(&mut self, manifest: &Value) {
        for root in ["sources", "nodes"] {
            let Some(items) = manifest.get(root).and_then(Value::as_object) else {
                continue;
            };
            for (unique_id, value) in items {
                let Some(fqn) = manifest_relation_fqn(value).map(|fqn| canonical_dataset_id(&fqn))
                else {
                    continue;
                };
                self.add_alias(unique_id, &fqn);
                self.add_alias(&fqn, &fqn);
                if let Some(name) = value.get("name").and_then(Value::as_str) {
                    self.add_alias(name, &fqn);
                }
                if let Some(alias) = value.get("alias").and_then(Value::as_str) {
                    self.add_alias(alias, &fqn);
                }
                if let Some(identifier) = value.get("identifier").and_then(Value::as_str) {
                    self.add_alias(identifier, &fqn);
                }
                if let Some(short_name) = fqn.rsplit('.').next() {
                    self.add_alias(short_name, &fqn);
                }
            }
        }
    }

    fn seed_from_graph(&mut self, graph: &LineageGraphSnapshot) {
        for node in &graph.nodes {
            if node.kind == LineageNodeKind::Field {
                continue;
            }
            if let Some(dataset_id) = node.dataset_id.as_deref() {
                self.add_alias(dataset_id, dataset_id);
                if let Some(short_name) = canonical_dataset_id(dataset_id).rsplit('.').next() {
                    self.add_alias(short_name, dataset_id);
                }
            }
            if matches!(
                node.kind,
                LineageNodeKind::DbtModel | LineageNodeKind::DbtSource
            ) {
                self.add_alias(&node.label, dataset_id_from_node(node));
                if let Some(unique_id) = node.metadata.get("unique_id") {
                    self.add_alias(unique_id, dataset_id_from_node(node));
                }
                if let Some(name) = node.metadata.get("name") {
                    self.add_alias(name, dataset_id_from_node(node));
                }
            }
        }
    }

    fn add_alias(&mut self, raw: &str, fqn: &str) {
        let key = canonical_dataset_id(raw);
        let fqn = canonical_dataset_id(fqn);
        if key.is_empty() || fqn.is_empty() {
            return;
        }
        self.aliases
            .entry(key)
            .and_modify(|existing| {
                if existing != &fqn {
                    existing.clear();
                }
            })
            .or_insert(fqn);
    }

    fn resolve_relation(&self, raw: &str) -> Option<String> {
        let value = canonical_dataset_id(raw);
        if value.is_empty() {
            return None;
        }
        if let Some(alias) = self.aliases.get(&value) {
            return (!alias.is_empty()).then_some(alias.clone());
        }
        let parts = value.split('.').filter(|part| !part.is_empty()).count();
        if parts >= 3 {
            return Some(value);
        }
        match (
            parts,
            self.default_container.as_deref(),
            self.default_namespace.as_deref(),
        ) {
            (2, Some(container), _) => Some(format!("{container}.{value}")),
            (1, Some(container), Some(namespace)) => {
                Some(format!("{container}.{namespace}.{value}"))
            }
            _ => None,
        }
    }

    fn resolve_or_canonical(&self, raw: &str) -> String {
        self.resolve_relation(raw)
            .unwrap_or_else(|| canonical_dataset_id(raw))
    }

    fn resolve_tables(&self, tables: &[String]) -> Vec<String> {
        tables
            .iter()
            .map(|table| self.resolve_or_canonical(table))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

fn dataset_id_from_node(node: &LineageNode) -> &str {
    node.dataset_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(node.label.as_str())
}

pub async fn refresh_lineage_graph_for_suite(
    sctx: &SuiteCtx,
    options: LineageBuildOptions,
) -> Result<LineageRefreshResult, String> {
    let mut builder = GraphBuilder::default();
    let cfg = sctx
        .capability::<ProvidersCfgCap>()
        .map(|cap| cap.0.clone());
    let manifest = load_manifest_value(sctx).await;
    let mut resolver = RelationResolver::new(cfg.as_ref(), manifest.as_ref());
    build_catalog_lineage(sctx, &mut builder, &mut resolver).await;
    build_skipprd_metadata_lineage(sctx, &options.pipeline, &mut builder, &mut resolver).await;
    build_dbt_manifest_lineage(manifest.as_ref(), &mut builder, &resolver);
    build_plan_contract_lineage(sctx, &mut builder, &resolver).await;
    let mut graph = builder.finish();
    if options.include_query_history {
        let records = fetch_query_history_records(
            sctx,
            options.query_history_since.as_deref(),
            options.query_history_limit,
            false,
            &mut graph.diagnostics,
        )
        .await;
        let deduped = dedupe_query_history_records(records);
        let mut evidence_resolver = RelationResolver::new(cfg.as_ref(), manifest.as_ref());
        evidence_resolver.seed_from_graph(&graph);
        apply_query_history_evidence(&mut graph, &deduped, &evidence_resolver);
    }

    graph.validate()?;
    let store = LineageStore::new(sctx.storage().clone(), sctx.keyspace().clone());
    store.write_graph(sctx.scope(), &graph).await?;
    Ok(refresh_result(graph))
}

pub async fn import_query_history_for_suite(
    sctx: &SuiteCtx,
    since: Option<String>,
    limit: usize,
    include_non_select: bool,
) -> Result<LineageRefreshResult, String> {
    let cfg = sctx
        .capability::<ProvidersCfgCap>()
        .map(|cap| cap.0.clone());
    let manifest = load_manifest_value(sctx).await;
    let store = LineageStore::new(sctx.storage().clone(), sctx.keyspace().clone());
    let mut graph = store.read_graph(sctx.scope()).await?.unwrap_or_default();
    graph = strip_query_history_evidence(graph);
    let mut resolver = RelationResolver::new(cfg.as_ref(), manifest.as_ref());
    resolver.seed_from_graph(&graph);
    let records = fetch_query_history_records(
        sctx,
        since.as_deref(),
        limit,
        include_non_select,
        &mut graph.diagnostics,
    )
    .await;
    let deduped = dedupe_query_history_records(records);
    apply_query_history_evidence(&mut graph, &deduped, &resolver);
    graph.validate()?;
    store.write_graph(sctx.scope(), &graph).await?;
    Ok(refresh_result(graph))
}

pub async fn load_lineage_graph_for_suite(
    sctx: &SuiteCtx,
    query: LineageGraphQuery,
) -> Result<LineageGraphSnapshot, String> {
    let store = LineageStore::new(sctx.storage().clone(), sctx.keyspace().clone());
    let graph = store.read_graph(sctx.scope()).await?.unwrap_or_default();
    let needs_field_focus = query
        .field_node_id
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    let mut sliced = slice_graph(&graph, &query);
    if needs_field_focus && matching_field_node_ids(&graph, &query).is_empty() {
        sliced.diagnostics.push(LineageDiagnostic {
            severity: LineageDiagnosticSeverity::Warning,
            message: "No field lineage matched this focus. Run `skippr lineage refresh --pipeline <pipeline>` to rebuild the persisted graph.".into(),
            source: Some(LineageEvidenceSource::SkipprdMetadata),
        });
    }
    Ok(sliced)
}

fn refresh_result(graph: LineageGraphSnapshot) -> LineageRefreshResult {
    let query_history = query_history_summary(&graph);
    let projected_graph = slice_graph(&graph, &LineageGraphQuery::default());
    LineageRefreshResult {
        ok: true,
        node_count: projected_graph.nodes.len(),
        edge_count: projected_graph.edges.len(),
        diagnostic_count: projected_graph.diagnostics.len(),
        query_history,
        graph: projected_graph,
    }
}

fn query_history_summary(graph: &LineageGraphSnapshot) -> QueryHistoryImportSummary {
    let mut query_ids = BTreeSet::new();
    for node in &graph.nodes {
        if let Some(raw) = node.metadata.get("query_history_ids") {
            if let Ok(ids) = serde_json::from_str::<Vec<String>>(raw) {
                query_ids.extend(ids);
            }
        }
    }
    let provider = graph
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.source == Some(LineageEvidenceSource::WarehouseQueryHistory))
        .and_then(|_| Some("warehouse".to_string()));
    let raw_error = graph
        .diagnostics
        .iter()
        .filter_map(|diagnostic| diagnostic.message.split_once("raw_error: "))
        .map(|(_, raw)| raw.to_string())
        .next();
    let parse_warnings = graph
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .to_ascii_lowercase()
                .contains("failed to parse sql")
        })
        .count();
    let queries_seen = graph
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.starts_with("query history:"))
        .count()
        + query_ids.len();
    QueryHistoryImportSummary {
        provider,
        capability: Some(if raw_error.is_some() {
            "provider_error".to_string()
        } else {
            "supported".to_string()
        }),
        raw_error,
        queries_seen,
        queries_imported: query_ids.len(),
        queries_skipped: queries_seen.saturating_sub(query_ids.len()),
        parse_warnings,
    }
}

async fn build_catalog_lineage(
    sctx: &SuiteCtx,
    builder: &mut GraphBuilder,
    resolver: &mut RelationResolver,
) {
    let Some(datasets) = sctx_datasets(sctx) else {
        builder.warn(
            "dataset catalog provider is unavailable; catalog lineage skipped",
            Some(LineageEvidenceSource::Catalog),
        );
        return;
    };
    let Some(catalog) = sctx_catalog(sctx) else {
        builder.warn(
            "catalog provider is unavailable; catalog lineage skipped",
            Some(LineageEvidenceSource::Catalog),
        );
        return;
    };
    let dataset_ids = match datasets.list_datasets().await {
        Ok(ids) => ids,
        Err(e) => {
            builder.warn(
                format!("dataset discovery failed during lineage build: {e}"),
                Some(LineageEvidenceSource::Catalog),
            );
            return;
        }
    };
    for dataset in dataset_ids {
        let fqn = dataset.fqn();
        match catalog.read_catalog(sctx.scope(), &fqn).await {
            Ok(Some(data_catalog)) => add_catalog_dataset(&data_catalog, builder, resolver),
            Ok(None) => {
                add_dataset_schema_fallback(datasets.as_ref(), &fqn, builder, resolver).await
            }
            Err(e) => builder.warn(
                format!("catalog read failed for {fqn}: {e}"),
                Some(LineageEvidenceSource::Catalog),
            ),
        }
    }
}

fn add_catalog_dataset(
    catalog: &DataCatalog,
    builder: &mut GraphBuilder,
    resolver: &mut RelationResolver,
) {
    let dataset_id = resolver.resolve_or_canonical(&catalog.dataset_id);
    resolver.add_alias(&catalog.dataset_id, &dataset_id);
    if let Some(short_name) = dataset_id.rsplit('.').next() {
        resolver.add_alias(short_name, &dataset_id);
    }
    let table_id = ensure_warehouse_node(
        builder,
        &dataset_id,
        merge_metadata(
            resolver.warehouse_metadata.clone(),
            BTreeMap::from([
                ("catalog".to_string(), catalog.catalog.clone()),
                ("database".to_string(), catalog.database.clone()),
                ("table".to_string(), catalog.table.clone()),
            ]),
        ),
    );
    for field in &catalog.fields {
        let field_path = field
            .field_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(field.name.as_str());
        let field_id = add_field_node(
            builder,
            &dataset_id,
            field_path,
            field_path.to_string(),
            None,
            BTreeMap::from([(
                "type".to_string(),
                field
                    .data_type
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            )]),
        );
        add_contains_field_edge(
            builder,
            &table_id,
            &field_id,
            LineageProvenance::observed(
                LineageEvidenceSource::Catalog,
                Some(catalog.dataset_id.clone()),
            ),
        );
    }
}

async fn add_dataset_schema_fallback(
    datasets: &dyn DatasetCatalogProvider,
    dataset_id: &str,
    builder: &mut GraphBuilder,
    resolver: &mut RelationResolver,
) {
    let Ok(parsed) = crate::providers::DatasetId::parse_fqn_strict(dataset_id) else {
        return;
    };
    let dataset_id = resolver.resolve_or_canonical(dataset_id);
    resolver.add_alias(dataset_id.as_str(), dataset_id.as_str());
    if let Some(short_name) = dataset_id.rsplit('.').next() {
        resolver.add_alias(short_name, &dataset_id);
    }
    let table_id = ensure_warehouse_node(builder, &dataset_id, resolver.warehouse_metadata.clone());
    let Ok(cols) = datasets.get_dataset_schema(&parsed).await else {
        return;
    };
    for (name, ty) in cols {
        let field_id = add_field_node(
            builder,
            &dataset_id,
            &name,
            name.clone(),
            None,
            BTreeMap::from([("type".to_string(), ty)]),
        );
        add_contains_field_edge(
            builder,
            &table_id,
            &field_id,
            LineageProvenance::unverified(
                LineageEvidenceSource::Catalog,
                Some(dataset_id.to_string()),
                80,
            ),
        );
    }
}

#[derive(Clone, Debug)]
struct SourceDescriptor {
    node_id: LineageNodeId,
    label: String,
    dataset_id: String,
    path: Option<String>,
    metadata: BTreeMap<String, String>,
}

impl SourceDescriptor {
    fn namespace(namespace: &str) -> Self {
        let label = namespace.trim().to_string();
        Self {
            node_id: LineageNodeId::generated(format!("raw:{label}")),
            label: label.clone(),
            dataset_id: label,
            path: None,
            metadata: BTreeMap::new(),
        }
    }
}

fn source_descriptor_from_providers_cfg(
    cfg: &crate::de_config::ProvidersResolved,
) -> Option<SourceDescriptor> {
    let input = cfg.el.skippr_input.as_object()?;
    let kind = input
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("source");
    let brand = provider_brand_for_source_kind(kind);
    let label = match brand {
        Some("s3") => {
            let bucket = input.get("s3_bucket").and_then(Value::as_str)?.trim();
            let prefix = input
                .get("s3_prefix")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .trim_start_matches('/');
            if prefix.is_empty() {
                format!("s3://{bucket}")
            } else {
                format!("s3://{bucket}/{prefix}")
            }
        }
        _ => input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(kind)
            .trim()
            .to_string(),
    };
    if label.is_empty() {
        return None;
    }
    let mut metadata = BTreeMap::new();
    metadata.insert("source_kind".to_string(), kind.to_string());
    if let Some(brand) = brand {
        metadata.insert("provider_brand".to_string(), brand.to_string());
        metadata.insert(
            "provider_label".to_string(),
            provider_label_for_brand(brand).to_string(),
        );
    }
    Some(SourceDescriptor {
        node_id: dataset_node_id(&label, LineageNodeKind::RawSource),
        label: label.clone(),
        dataset_id: label.clone(),
        path: label.starts_with("s3://").then_some(label),
        metadata,
    })
}

fn provider_brand_for_source_kind(kind: &str) -> Option<&'static str> {
    match kind.trim().to_ascii_lowercase().as_str() {
        "s3" => Some("s3"),
        "file" | "files" | "csv" | "local" | "local_file" => Some("file"),
        _ => None,
    }
}

fn provider_brand_for_warehouse_kind(kind: crate::de_config::WarehouseKind) -> &'static str {
    match kind {
        crate::de_config::WarehouseKind::Athena => "athena",
        crate::de_config::WarehouseKind::Postgres => "postgres",
        crate::de_config::WarehouseKind::Mssql => "mssql",
        crate::de_config::WarehouseKind::Snowflake => "snowflake",
        crate::de_config::WarehouseKind::Bigquery => "bigquery",
        crate::de_config::WarehouseKind::Databricks => "databricks",
        crate::de_config::WarehouseKind::Synapse => "synapse",
        crate::de_config::WarehouseKind::Redshift => "redshift",
        crate::de_config::WarehouseKind::Clickhouse => "clickhouse",
        crate::de_config::WarehouseKind::Motherduck => "motherduck",
    }
}

fn provider_label_for_brand(brand: &str) -> &'static str {
    match brand {
        "s3" => "Amazon S3",
        "file" => "File",
        "athena" => "Amazon Athena",
        "postgres" => "PostgreSQL",
        "mssql" => "Microsoft SQL Server",
        "snowflake" => "Snowflake",
        "bigquery" => "BigQuery",
        "databricks" => "Databricks",
        "synapse" => "Azure Synapse",
        "redshift" => "Amazon Redshift",
        "clickhouse" => "ClickHouse",
        "motherduck" => "MotherDuck",
        _ => "Provider",
    }
}

fn warehouse_node_metadata(
    cfg: Option<&crate::de_config::ProvidersResolved>,
) -> BTreeMap<String, String> {
    let Some(cfg) = cfg else {
        return BTreeMap::new();
    };
    let brand = provider_brand_for_warehouse_kind(cfg.warehouse.kind);
    BTreeMap::from([
        ("provider_brand".to_string(), brand.to_string()),
        (
            "provider_label".to_string(),
            provider_label_for_brand(brand).to_string(),
        ),
    ])
}

fn insert_lineage_resources(
    metadata: &mut BTreeMap<String, String>,
    resources: Vec<LineageResourceRef>,
) {
    if resources.is_empty() {
        return;
    }
    if let Ok(json) = serde_json::to_string(&resources) {
        metadata.insert(LINEAGE_META_RESOURCES.to_string(), json);
    }
}

fn resources_for_raw_source(
    node_id: &LineageNodeId,
    dataset_id: &str,
    metadata_location: Option<&str>,
) -> Vec<LineageResourceRef> {
    let mut resources = vec![
        LineageResourceRef {
            kind: LineageResourceKind::NodeId,
            label: "Copy node id".to_string(),
            target: Some(node_id.as_str().to_string()),
        },
        LineageResourceRef {
            kind: LineageResourceKind::DatasetId,
            label: "Copy dataset id".to_string(),
            target: Some(dataset_id.to_string()),
        },
    ];
    if let Some(target) = metadata_location.filter(|value| !value.trim().is_empty()) {
        resources.insert(
            0,
            LineageResourceRef {
                kind: LineageResourceKind::Metadata,
                label: "Open metadata".to_string(),
                target: Some(target.to_string()),
            },
        );
    }
    resources
}

fn resources_for_pipeline(
    node_id: &LineageNodeId,
    pipeline: &str,
    metadata_location: Option<&str>,
) -> Vec<LineageResourceRef> {
    let mut resources = vec![
        LineageResourceRef {
            kind: LineageResourceKind::NodeId,
            label: "Copy node id".to_string(),
            target: Some(node_id.as_str().to_string()),
        },
        LineageResourceRef {
            kind: LineageResourceKind::DatasetId,
            label: "Copy dataset id".to_string(),
            target: Some(pipeline.to_string()),
        },
        LineageResourceRef {
            kind: LineageResourceKind::Config,
            label: "Open skippr.yml".to_string(),
            target: None,
        },
    ];
    if let Some(target) = metadata_location.filter(|value| !value.trim().is_empty()) {
        resources.insert(
            0,
            LineageResourceRef {
                kind: LineageResourceKind::Metadata,
                label: "Open pipeline metadata".to_string(),
                target: Some(target.to_string()),
            },
        );
    }
    resources
}

fn resources_for_dataset_node(
    node_id: &LineageNodeId,
    dataset_id: &str,
) -> Vec<LineageResourceRef> {
    vec![
        LineageResourceRef {
            kind: LineageResourceKind::NodeId,
            label: "Copy node id".to_string(),
            target: Some(node_id.as_str().to_string()),
        },
        LineageResourceRef {
            kind: LineageResourceKind::DatasetId,
            label: "Copy dataset id".to_string(),
            target: Some(dataset_id.to_string()),
        },
    ]
}

struct SkipprdNamespaceGraph {
    raw_id: LineageNodeId,
    pipeline_id: LineageNodeId,
    ingest_id: LineageNodeId,
    _warehouse_id: LineageNodeId,
    dataset_id: String,
}

async fn load_skipprd_pipeline_metadata(
    sctx: &SuiteCtx,
    pipeline: &str,
    builder: &mut GraphBuilder,
) -> (Vec<crate::providers::SkipprNamespaceStatus>, Option<String>) {
    let uses_configured_metadata = sctx_skipprd_metadata(sctx).is_some();
    let mut status = if uses_configured_metadata {
        load_configured_skipprd_metadata_status(sctx, pipeline, builder).await
    } else if let Some(skippr) = sctx_skippr(sctx) {
        match skippr.show_pipeline(sctx.scope(), pipeline).await {
            Ok(status) => Some(status),
            Err(e) => {
                builder.warn(
                    format!("failed to read skipprd metadata for pipeline '{pipeline}': {e}"),
                    Some(LineageEvidenceSource::SkipprdMetadata),
                );
                None
            }
        }
    } else {
        builder.info(
            "skippr provider is unavailable; using config-derived source lineage only",
            Some(LineageEvidenceSource::SkipprdMetadata),
        );
        None
    };
    if !uses_configured_metadata && skippr_status_missing_fields(status.as_ref()) {
        if let Some(persisted) =
            load_persisted_skipprd_metadata_status(sctx, pipeline, status.as_ref()).await
        {
            status = Some(persisted);
        }
    }
    let source_ref = status
        .as_ref()
        .and_then(|status| status.metadata_location.clone());
    let namespaces = status
        .map(|status| status.namespaces)
        .filter(|namespaces| !namespaces.is_empty())
        .unwrap_or_else(|| {
            vec![crate::providers::SkipprNamespaceStatus {
                namespace: pipeline.to_string(),
                ..Default::default()
            }]
        });
    (namespaces, source_ref)
}

async fn load_configured_skipprd_metadata_status(
    sctx: &SuiteCtx,
    pipeline: &str,
    builder: &mut GraphBuilder,
) -> Option<SkipprPipelineStatus> {
    let Some(cap) = sctx_skipprd_metadata(sctx) else {
        return None;
    };
    let mut inspected = Vec::new();
    for location in cap.locations {
        match read_skipprd_metadata_location(sctx, &location).await {
            Ok(Some(value)) => {
                let source_ref = skipprd_metadata_location_label(&location);
                if let Some(status) =
                    skippr_status_from_metadata_value(pipeline, &source_ref, &value)
                {
                    return Some(status);
                }
                inspected.push(source_ref);
            }
            Ok(None) => inspected.push(skipprd_metadata_location_label(&location)),
            Err(error) => builder.warn(
                format!(
                    "failed to read configured skipprd metadata '{}': {error}",
                    skipprd_metadata_location_label(&location)
                ),
                Some(LineageEvidenceSource::SkipprdMetadata),
            ),
        }
    }
    if !inspected.is_empty() {
        builder.warn(
            format!(
                "no skipprd metadata fields found for pipeline '{pipeline}' in configured metadata locations: {}",
                inspected.join(", ")
            ),
            Some(LineageEvidenceSource::SkipprdMetadata),
        );
    }
    None
}

async fn read_skipprd_metadata_location(
    sctx: &SuiteCtx,
    location: &SkipprdMetadataLocation,
) -> Result<Option<Value>, String> {
    match location {
        SkipprdMetadataLocation::LocalPath(path) => {
            if !path.exists() {
                return Ok(None);
            }
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|error| error.to_string())?;
            serde_json::from_slice::<Value>(&bytes)
                .map(Some)
                .map_err(|error| error.to_string())
        }
        SkipprdMetadataLocation::StorageKey(key) => {
            match retry_get_bytes(sctx.storage().as_ref(), key).await {
                Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
                    .map(Some)
                    .map_err(|error| error.to_string()),
                Err(_) => Ok(None),
            }
        }
    }
}

fn skipprd_metadata_location_label(location: &SkipprdMetadataLocation) -> String {
    match location {
        SkipprdMetadataLocation::LocalPath(path) => path.display().to_string(),
        SkipprdMetadataLocation::StorageKey(key) => key.clone(),
    }
}

fn build_skipprd_entity_nodes(
    builder: &mut GraphBuilder,
    resolver: &mut RelationResolver,
    pipeline: &str,
    source: &SourceDescriptor,
    source_ref: Option<&str>,
    namespace: &crate::providers::SkipprNamespaceStatus,
    default_catalog: &str,
    default_schema: &str,
    cfg: Option<&crate::de_config::ProvidersResolved>,
) -> SkipprdNamespaceGraph {
    let raw_id = source.node_id.clone();
    let pipeline_id = LineageNodeId::generated(format!("pipeline:{pipeline}"));
    let mut source_metadata = source.metadata.clone();
    source_metadata.insert("pipeline".to_string(), pipeline.to_string());
    if let Some(location) = source_ref {
        source_metadata.insert("metadata_location".to_string(), location.to_string());
    }
    insert_lineage_resources(
        &mut source_metadata,
        resources_for_raw_source(&raw_id, &source.dataset_id, source_ref),
    );
    builder.add_node(LineageNode {
        id: raw_id.clone(),
        label: source.label.clone(),
        kind: LineageNodeKind::RawSource,
        dataset_id: Some(source.dataset_id.clone()),
        field: None,
        path: source
            .path
            .clone()
            .or_else(|| source_ref.map(str::to_string)),
        metadata: source_metadata,
    });
    let dataset_id = if default_catalog.is_empty() || default_schema.is_empty() {
        canonical_dataset_id(&namespace.namespace)
    } else {
        canonical_dataset_id(&format!(
            "{default_catalog}.{default_schema}.{}",
            namespace.namespace
        ))
    };
    resolver.add_alias(&dataset_id, &dataset_id);
    resolver.add_alias(&namespace.namespace, &dataset_id);
    let mut pipeline_metadata = BTreeMap::from([
        ("provider_brand".to_string(), "skippr".to_string()),
        ("provider_label".to_string(), "Skippr".to_string()),
        (
            "transform_key".to_string(),
            format!("warehouse:{dataset_id}"),
        ),
        (
            "transform_source".to_string(),
            "skipprd_metadata".to_string(),
        ),
        ("pipeline".to_string(), pipeline.to_string()),
    ]);
    if let Some(location) = source_ref {
        pipeline_metadata.insert("metadata_location".to_string(), location.to_string());
    }
    insert_lineage_resources(
        &mut pipeline_metadata,
        resources_for_pipeline(&pipeline_id, pipeline, source_ref),
    );
    builder.add_node(LineageNode {
        id: pipeline_id.clone(),
        label: pipeline.to_string(),
        kind: LineageNodeKind::Pipeline,
        dataset_id: Some(pipeline.to_string()),
        field: None,
        path: source_ref.map(str::to_string),
        metadata: pipeline_metadata,
    });
    let ingest_id = dataset_node_id(&dataset_id, LineageNodeKind::IngestTable);
    let mut ingest_metadata = warehouse_node_metadata(cfg);
    insert_lineage_resources(
        &mut ingest_metadata,
        resources_for_dataset_node(&ingest_id, &dataset_id),
    );
    builder.add_node(LineageNode {
        id: ingest_id.clone(),
        label: dataset_id.clone(),
        kind: LineageNodeKind::IngestTable,
        dataset_id: Some(dataset_id.clone()),
        field: None,
        path: None,
        metadata: ingest_metadata,
    });
    let warehouse_id = dataset_node_id(&dataset_id, LineageNodeKind::WarehouseTable);
    let mut warehouse_metadata = warehouse_node_metadata(cfg);
    insert_lineage_resources(
        &mut warehouse_metadata,
        resources_for_dataset_node(&warehouse_id, &dataset_id),
    );
    builder.add_node(LineageNode {
        id: warehouse_id.clone(),
        label: dataset_id.clone(),
        kind: LineageNodeKind::WarehouseTable,
        dataset_id: Some(dataset_id.clone()),
        field: None,
        path: None,
        metadata: warehouse_metadata,
    });
    let provenance = || {
        LineageProvenance::observed(
            LineageEvidenceSource::SkipprdMetadata,
            source_ref.map(str::to_string),
        )
    };
    builder.add_edge(LineageEdge {
        id: edge_id(LineageEdgeKind::Ingests, &raw_id, &pipeline_id),
        from_node_id: raw_id.clone(),
        to_node_id: pipeline_id.clone(),
        kind: LineageEdgeKind::Ingests,
        provenance: provenance(),
        metadata: BTreeMap::new(),
    });
    builder.add_edge(LineageEdge {
        id: edge_id(LineageEdgeKind::Ingests, &pipeline_id, &ingest_id),
        from_node_id: pipeline_id.clone(),
        to_node_id: ingest_id.clone(),
        kind: LineageEdgeKind::Ingests,
        provenance: provenance(),
        metadata: BTreeMap::new(),
    });
    builder.add_edge(LineageEdge {
        id: edge_id(LineageEdgeKind::Materializes, &ingest_id, &warehouse_id),
        from_node_id: ingest_id.clone(),
        to_node_id: warehouse_id.clone(),
        kind: LineageEdgeKind::Materializes,
        provenance: provenance(),
        metadata: BTreeMap::new(),
    });
    SkipprdNamespaceGraph {
        raw_id,
        pipeline_id,
        ingest_id,
        _warehouse_id: warehouse_id,
        dataset_id,
    }
}

fn build_skipprd_field_nodes(
    builder: &mut GraphBuilder,
    pipeline: &str,
    source: &SourceDescriptor,
    source_ref: Option<&str>,
    graph: &SkipprdNamespaceGraph,
    fields: &[crate::providers::SkipprFieldSchema],
) {
    let provenance = || {
        LineageProvenance::observed(
            LineageEvidenceSource::SkipprdMetadata,
            source_ref.map(str::to_string),
        )
    };
    for field in fields {
        let output_field_name = field
            .out_field_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(field.name.as_str());
        let source_field_name = field
            .source_field_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(output_field_name);
        let mut field_metadata = BTreeMap::from([
            ("type".to_string(), field.field_type.clone()),
            ("nullable".to_string(), field.nullable.to_string()),
            (
                "source_field_name".to_string(),
                source_field_name.to_string(),
            ),
            ("out_field_name".to_string(), output_field_name.to_string()),
        ]);
        field_metadata.insert("pipeline".to_string(), pipeline.to_string());
        if let Some(field_id) = field.field_id {
            field_metadata.insert("field_id".to_string(), field_id.to_string());
        }
        if let Some(lineage_id) = field.lineage_id.as_deref() {
            if !lineage_id.trim().is_empty() {
                field_metadata.insert("lineage_id".to_string(), lineage_id.to_string());
            }
        }
        let source_field_id = add_field_node(
            builder,
            &source.dataset_id,
            source_field_name,
            source_field_name.to_string(),
            None,
            field_metadata.clone(),
        );
        let pipeline_field_id = add_field_node(
            builder,
            pipeline,
            output_field_name,
            output_field_name.to_string(),
            None,
            field_metadata,
        );
        let output_field_id = add_field_node(
            builder,
            &graph.dataset_id,
            output_field_name,
            output_field_name.to_string(),
            None,
            BTreeMap::from([
                ("type".to_string(), field.field_type.clone()),
                ("nullable".to_string(), field.nullable.to_string()),
                (
                    "source_field_name".to_string(),
                    source_field_name.to_string(),
                ),
                ("out_field_name".to_string(), output_field_name.to_string()),
            ]),
        );
        add_contains_field_edge(builder, &graph.raw_id, &source_field_id, provenance());
        add_contains_field_edge(
            builder,
            &graph.pipeline_id,
            &pipeline_field_id,
            provenance(),
        );
        add_contains_field_edge(builder, &graph.ingest_id, &output_field_id, provenance());
        add_field_lineage_edge(
            builder,
            LineageEdgeKind::FieldDerivesFrom,
            &source_field_id,
            &pipeline_field_id,
            provenance(),
            BTreeMap::new(),
        );
        add_field_lineage_edge(
            builder,
            LineageEdgeKind::FieldDerivesFrom,
            &pipeline_field_id,
            &output_field_id,
            provenance(),
            BTreeMap::new(),
        );
    }
}

async fn build_skipprd_metadata_lineage(
    sctx: &SuiteCtx,
    pipeline: &PipelineName,
    builder: &mut GraphBuilder,
    resolver: &mut RelationResolver,
) {
    let pipeline = pipeline.as_str();
    let cfg = sctx
        .capability::<ProvidersCfgCap>()
        .map(|cap| cap.0.clone());
    let default_catalog = cfg
        .as_ref()
        .map(|cfg| cfg.warehouse.container.trim())
        .unwrap_or("");
    let default_schema = cfg
        .as_ref()
        .map(|cfg| cfg.warehouse.namespace.trim())
        .unwrap_or("");
    let source = cfg
        .as_ref()
        .and_then(source_descriptor_from_providers_cfg)
        .unwrap_or_else(|| SourceDescriptor::namespace(pipeline));

    let (namespaces, source_ref) = load_skipprd_pipeline_metadata(sctx, pipeline, builder).await;
    let source_ref = source_ref.as_deref();

    for namespace in namespaces {
        let graph = build_skipprd_entity_nodes(
            builder,
            resolver,
            pipeline,
            &source,
            source_ref,
            &namespace,
            default_catalog,
            default_schema,
            cfg.as_ref(),
        );
        build_skipprd_field_nodes(
            builder,
            pipeline,
            &source,
            source_ref,
            &graph,
            &namespace.fields,
        );
    }
}

fn skippr_status_missing_fields(status: Option<&SkipprPipelineStatus>) -> bool {
    status
        .map(|status| {
            status.namespaces.is_empty()
                || status
                    .namespaces
                    .iter()
                    .all(|namespace| namespace.fields.is_empty())
        })
        .unwrap_or(true)
}

async fn load_persisted_skipprd_metadata_status(
    sctx: &SuiteCtx,
    pipeline: &str,
    current: Option<&SkipprPipelineStatus>,
) -> Option<SkipprPipelineStatus> {
    let mut keys = BTreeSet::new();
    if let Some(key) = current
        .and_then(|status| status.metadata_location.as_deref())
        .and_then(metadata_location_storage_key)
    {
        keys.insert(key);
    }
    keys.insert(
        sctx.keyspace()
            .scoped_key(sctx.scope(), &["metadata", "metadata.json"]),
    );
    keys.insert(format!(
        "{}/{}/{}/metadata/metadata.json",
        sctx.scope().tenant,
        sctx.scope().workspace,
        pipeline
    ));

    for key in keys {
        let Ok(bytes) = retry_get_bytes(sctx.storage().as_ref(), &key).await else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if let Some(status) = skippr_status_from_metadata_value(pipeline, &key, &value) {
            return Some(status);
        }
    }
    None
}

fn metadata_location_storage_key(location: &str) -> Option<String> {
    let location = location.trim();
    if location.is_empty() {
        return None;
    }
    if let Some(rest) = location.strip_prefix("s3://") {
        return rest
            .split_once('/')
            .map(|(_, key)| key.to_string())
            .filter(|key| !key.trim().is_empty());
    }
    Some(location.to_string())
}

fn skippr_status_from_metadata_value(
    pipeline: &str,
    source_ref: &str,
    value: &Value,
) -> Option<SkipprPipelineStatus> {
    let metadata = value.get("metadata")?.as_object()?;
    let namespaces = metadata
        .iter()
        .filter_map(|(namespace, value)| skippr_namespace_from_metadata_value(namespace, value))
        .collect::<Vec<_>>();
    (!namespaces.is_empty()).then(|| SkipprPipelineStatus {
        pipeline: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(pipeline)
            .to_string(),
        status: if value
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            "active".to_string()
        } else {
            "disabled".to_string()
        },
        namespaces,
        metadata_location: Some(source_ref.to_string()),
    })
}

fn skippr_namespace_from_metadata_value(
    namespace: &str,
    value: &Value,
) -> Option<SkipprNamespaceStatus> {
    let fields = value
        .get("fields")
        .and_then(Value::as_object)?
        .iter()
        .filter_map(|(key, value)| skippr_field_from_metadata_value(key, value))
        .collect::<Vec<_>>();
    (!fields.is_empty()).then(|| SkipprNamespaceStatus {
        namespace: namespace.to_string(),
        fields,
        offset: None,
        cdc_enabled: false,
        last_checkpoint: None,
    })
}

fn skippr_field_from_metadata_value(key: &str, value: &Value) -> Option<SkipprFieldSchema> {
    let out_field_name = value
        .get("out_field_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(key);
    let source_field_name = value
        .get("source_field_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(key);
    let field_type = value
        .get("determined_type")
        .or_else(|| value.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "Unknown".to_string());
    Some(SkipprFieldSchema {
        name: out_field_name.to_string(),
        field_type,
        nullable: value
            .get("nullable")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        source_field_name: Some(source_field_name.to_string()),
        out_field_name: Some(out_field_name.to_string()),
        field_id: value
            .get("field_id")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok()),
        lineage_id: value
            .get("lineage_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn build_dbt_manifest_lineage(
    manifest: Option<&Value>,
    builder: &mut GraphBuilder,
    resolver: &RelationResolver,
) {
    let Some(manifest) = manifest else {
        builder.info(
            "target/manifest.json was not available; DBT lineage skipped",
            Some(LineageEvidenceSource::DbtManifest),
        );
        return;
    };
    add_manifest_sources(manifest, builder, resolver);
    add_manifest_models(manifest, builder, resolver);
    add_manifest_exposures_and_metrics(manifest, builder);
}

async fn load_manifest_value(sctx: &SuiteCtx) -> Option<Value> {
    let base = sctx
        .keyspace()
        .scoped_prefix(sctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string()
        + "/";
    let key = format!("{base}target/manifest.json");
    let bytes = retry_get_bytes(sctx.storage().as_ref(), &key).await.ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

fn add_manifest_sources(manifest: &Value, builder: &mut GraphBuilder, resolver: &RelationResolver) {
    let Some(sources) = manifest.get("sources").and_then(|value| value.as_object()) else {
        return;
    };
    for (unique_id, source) in sources {
        let fqn =
            resolver.resolve_or_canonical(&manifest_relation_fqn(source).unwrap_or_else(|| {
                source
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(unique_id)
                    .to_string()
            }));
        let id = LineageNodeId::generated(format!("dbt_source:{unique_id}"));
        builder.add_node(LineageNode {
            id: id.clone(),
            label: fqn.clone(),
            kind: LineageNodeKind::DbtSource,
            dataset_id: Some(fqn.clone()),
            field: None,
            path: None,
            metadata: manifest_common_metadata(unique_id, source),
        });
        let table_id = ensure_warehouse_node(builder, &fqn, resolver.warehouse_metadata.clone());
        builder.add_edge(LineageEdge {
            id: edge_id(LineageEdgeKind::SelectsFrom, &table_id, &id),
            from_node_id: table_id,
            to_node_id: id,
            kind: LineageEdgeKind::SelectsFrom,
            provenance: LineageProvenance::observed(
                LineageEvidenceSource::DbtManifest,
                Some("target/manifest.json".to_string()),
            ),
            metadata: BTreeMap::new(),
        });
    }
}

fn manifest_relation_map(manifest: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for root in ["sources", "nodes"] {
        let Some(items) = manifest.get(root).and_then(Value::as_object) else {
            continue;
        };
        for (unique_id, value) in items {
            if let Some(fqn) = manifest_relation_fqn(value).map(|fqn| canonical_dataset_id(&fqn)) {
                out.insert(unique_id.clone(), fqn);
            }
        }
    }
    out
}

fn manifest_column_paths(node: &Value) -> Vec<String> {
    node.get("columns")
        .and_then(Value::as_object)
        .map(|columns| {
            columns
                .iter()
                .map(|(key, value)| {
                    value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(key.as_str())
                })
                .map(canonical_field_path)
                .filter(|field| !field.is_empty())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        })
        .unwrap_or_default()
}

fn manifest_sql(node: &Value) -> Option<&str> {
    ["compiled_sql", "compiled_code", "raw_sql", "raw_code"]
        .into_iter()
        .find_map(|key| {
            node.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
}

fn merge_metadata(
    mut base: BTreeMap<String, String>,
    incoming: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    for (key, value) in incoming {
        base.entry(key).or_insert(value);
    }
    base
}

fn ensure_warehouse_node(
    builder: &mut GraphBuilder,
    dataset_id: &str,
    metadata: BTreeMap<String, String>,
) -> LineageNodeId {
    let dataset_id = canonical_dataset_id(dataset_id);
    let table_id = dataset_node_id(&dataset_id, LineageNodeKind::WarehouseTable);
    builder.add_node(LineageNode {
        id: table_id.clone(),
        label: dataset_id.clone(),
        kind: LineageNodeKind::WarehouseTable,
        dataset_id: Some(dataset_id),
        field: None,
        path: None,
        metadata,
    });
    table_id
}

fn field_leaf(field_path: &str) -> String {
    canonical_field_path(field_path)
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_string()
}

fn wildcard_source_dataset(output: &SqlSelectedOutput, input_tables: &[String]) -> Option<String> {
    if !output.wildcard {
        return None;
    }
    output
        .source_fields
        .iter()
        .find_map(|source| {
            source
                .strip_suffix(".*")
                .map(canonical_dataset_id)
                .filter(|dataset| !dataset.is_empty())
        })
        .or_else(|| (input_tables.len() == 1).then(|| input_tables[0].clone()))
}

struct SqlFieldLineageTarget<'a> {
    transform_entity_id: &'a LineageNodeId,
    output_entity_id: Option<&'a LineageNodeId>,
    output_dataset: &'a str,
    input_tables: &'a [String],
    fallback_output_fields: &'a [String],
    evidence_source: LineageEvidenceSource,
    source_ref: Option<String>,
    warehouse_metadata: BTreeMap<String, String>,
}

fn add_sql_selected_output_lineage(
    builder: &mut GraphBuilder,
    target: SqlFieldLineageTarget<'_>,
    selected_outputs: &[SqlSelectedOutput],
) {
    let mut added = false;
    for output in selected_outputs {
        if output.wildcard {
            let Some(source_dataset) = wildcard_source_dataset(output, target.input_tables) else {
                continue;
            };
            let output_fields = if target.fallback_output_fields.is_empty() {
                builder.field_paths_for_dataset(&source_dataset)
            } else {
                target.fallback_output_fields.to_vec()
            };
            for field in output_fields {
                add_sql_field_lineage(
                    builder,
                    &target,
                    &source_dataset,
                    &field,
                    &field,
                    LineageEdgeKind::FieldDerivesFrom,
                );
                added = true;
            }
            continue;
        }

        let Some(output_field) = output
            .output_field
            .as_deref()
            .map(canonical_field_path)
            .filter(|field| !field.is_empty())
            .or_else(|| {
                (output.source_fields.len() == 1).then(|| field_leaf(&output.source_fields[0]))
            })
        else {
            continue;
        };
        for source_field in &output.source_fields {
            let Some((source_dataset, source_field)) =
                resolve_query_field_source(source_field, target.input_tables)
            else {
                continue;
            };
            add_sql_field_lineage(
                builder,
                &target,
                &source_dataset,
                &source_field,
                &output_field,
                if output.aggregate {
                    LineageEdgeKind::AggregatesFrom
                } else {
                    LineageEdgeKind::FieldDerivesFrom
                },
            );
            added = true;
        }
    }

    if !added && target.input_tables.len() == 1 {
        for output_field in target.fallback_output_fields {
            add_sql_field_lineage(
                builder,
                &target,
                &target.input_tables[0],
                output_field,
                output_field,
                LineageEdgeKind::FieldDerivesFrom,
            );
        }
    }
}

fn add_sql_field_lineage(
    builder: &mut GraphBuilder,
    target: &SqlFieldLineageTarget<'_>,
    source_dataset: &str,
    source_field: &str,
    output_field: &str,
    edge_kind: LineageEdgeKind,
) {
    let source_dataset = canonical_dataset_id(source_dataset);
    let output_dataset = canonical_dataset_id(target.output_dataset);
    let source_table_id =
        ensure_warehouse_node(builder, &source_dataset, target.warehouse_metadata.clone());
    let source_field = canonical_field_path(source_field);
    let output_field = canonical_field_path(output_field);
    if source_field.is_empty() || output_field.is_empty() {
        return;
    }
    let source_field_id = add_field_node(
        builder,
        &source_dataset,
        &source_field,
        source_field.clone(),
        None,
        BTreeMap::new(),
    );
    add_contains_field_edge(
        builder,
        &source_table_id,
        &source_field_id,
        LineageProvenance::unverified(
            target.evidence_source.clone(),
            target.source_ref.clone(),
            70,
        ),
    );
    let output_field_id = add_field_node(
        builder,
        &output_dataset,
        &output_field,
        output_field.clone(),
        None,
        BTreeMap::new(),
    );
    add_contains_field_edge(
        builder,
        target.transform_entity_id,
        &output_field_id,
        LineageProvenance::unverified(
            target.evidence_source.clone(),
            target.source_ref.clone(),
            75,
        ),
    );
    if let Some(output_entity_id) = target.output_entity_id {
        add_contains_field_edge(
            builder,
            output_entity_id,
            &output_field_id,
            LineageProvenance::unverified(
                target.evidence_source.clone(),
                target.source_ref.clone(),
                75,
            ),
        );
    }
    add_field_lineage_edge(
        builder,
        edge_kind,
        &source_field_id,
        &output_field_id,
        LineageProvenance::unverified(
            target.evidence_source.clone(),
            target.source_ref.clone(),
            70,
        ),
        BTreeMap::new(),
    );
}

fn add_manifest_models(manifest: &Value, builder: &mut GraphBuilder, resolver: &RelationResolver) {
    let Some(nodes) = manifest.get("nodes").and_then(|value| value.as_object()) else {
        return;
    };
    let relation_by_unique_id = manifest_relation_map(manifest);
    for (unique_id, node) in nodes {
        if node.get("resource_type").and_then(Value::as_str) != Some("model") {
            continue;
        }
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(unique_id);
        let id = LineageNodeId::generated(format!("dbt_model:{unique_id}"));
        let relation_fqn =
            manifest_relation_fqn(node).and_then(|fqn| resolver.resolve_relation(&fqn));
        let metadata = manifest_model_metadata(unique_id, node, relation_fqn.as_deref());
        builder.add_node(LineageNode {
            id: id.clone(),
            label: name.to_string(),
            kind: LineageNodeKind::DbtModel,
            dataset_id: relation_fqn.clone(),
            field: None,
            path: manifest_path(node),
            metadata,
        });

        let relation_id = if let Some(fqn) = relation_fqn.as_ref() {
            let relation_id =
                ensure_warehouse_node(builder, fqn, resolver.warehouse_metadata.clone());
            builder.add_edge(LineageEdge {
                id: edge_id(LineageEdgeKind::Materializes, &id, &relation_id),
                from_node_id: id.clone(),
                to_node_id: relation_id.clone(),
                kind: LineageEdgeKind::Materializes,
                provenance: LineageProvenance::observed(
                    LineageEvidenceSource::DbtManifest,
                    Some("target/manifest.json".to_string()),
                ),
                metadata: BTreeMap::new(),
            });
            Some(relation_id)
        } else {
            None
        };

        for dep in manifest_depends_on(node) {
            let dep_id = manifest_dep_node_id(&dep);
            let dep_dataset_id = relation_by_unique_id.get(&dep).cloned();
            builder.add_node(LineageNode {
                id: dep_id.clone(),
                label: dep.clone(),
                kind: manifest_dep_node_kind(&dep),
                dataset_id: dep_dataset_id,
                field: None,
                path: None,
                metadata: BTreeMap::new(),
            });
            builder.add_edge(LineageEdge {
                id: edge_id(LineageEdgeKind::SelectsFrom, &dep_id, &id),
                from_node_id: dep_id,
                to_node_id: id.clone(),
                kind: LineageEdgeKind::SelectsFrom,
                provenance: LineageProvenance::observed(
                    LineageEvidenceSource::DbtManifest,
                    Some("target/manifest.json".to_string()),
                ),
                metadata: BTreeMap::new(),
            });
        }
        if let (Some(output_dataset), Some(output_entity_id), Some(sql)) = (
            relation_fqn.as_deref(),
            relation_id.as_ref(),
            manifest_sql(node),
        ) {
            let analysis = analyze_select_sql(sql);
            let input_tables = resolver.resolve_tables(&analysis.tables);
            let fallback_output_fields = manifest_column_paths(node);
            add_sql_selected_output_lineage(
                builder,
                SqlFieldLineageTarget {
                    transform_entity_id: &id,
                    output_entity_id: Some(output_entity_id),
                    output_dataset,
                    input_tables: &input_tables,
                    fallback_output_fields: &fallback_output_fields,
                    evidence_source: LineageEvidenceSource::DbtManifest,
                    source_ref: Some("target/manifest.json".to_string()),
                    warehouse_metadata: resolver.warehouse_metadata.clone(),
                },
                &analysis.selected_outputs,
            );
        }
    }
}

fn add_manifest_exposures_and_metrics(manifest: &Value, builder: &mut GraphBuilder) {
    for (root_key, kind) in [
        ("exposures", LineageNodeKind::Dashboard),
        ("metrics", LineageNodeKind::Metric),
    ] {
        let Some(items) = manifest.get(root_key).and_then(Value::as_object) else {
            continue;
        };
        for (unique_id, item) in items {
            let id = LineageNodeId::generated(format!("{root_key}:{unique_id}"));
            let label = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(unique_id);
            builder.add_node(LineageNode {
                id: id.clone(),
                label: label.to_string(),
                kind: kind.clone(),
                dataset_id: None,
                field: None,
                path: manifest_path(item),
                metadata: manifest_common_metadata(unique_id, item),
            });
            for dep in manifest_depends_on(item) {
                let dep_id = manifest_dep_node_id(&dep);
                builder.add_node(LineageNode {
                    id: dep_id.clone(),
                    label: dep.clone(),
                    kind: manifest_dep_node_kind(&dep),
                    dataset_id: None,
                    field: None,
                    path: None,
                    metadata: BTreeMap::new(),
                });
                builder.add_edge(LineageEdge {
                    id: edge_id(LineageEdgeKind::Feeds, &dep_id, &id),
                    from_node_id: dep_id,
                    to_node_id: id.clone(),
                    kind: LineageEdgeKind::Feeds,
                    provenance: LineageProvenance::observed(
                        LineageEvidenceSource::DbtManifest,
                        Some("target/manifest.json".to_string()),
                    ),
                    metadata: BTreeMap::new(),
                });
            }
        }
    }
}

async fn build_plan_contract_lineage(
    sctx: &SuiteCtx,
    builder: &mut GraphBuilder,
    resolver: &RelationResolver,
) {
    let root = sctx
        .keyspace()
        .threads_prefix(sctx.scope())
        .trim_end_matches("/threads")
        .trim_end_matches('/')
        .to_string();
    let prefix = format!("{root}/plans/");
    let keys = match retry_list_prefix(sctx.storage().as_ref(), &prefix).await {
        Ok(keys) => keys,
        Err(e) => {
            builder.info(
                format!("plan lineage skipped; no readable plan prefix: {e}"),
                Some(LineageEvidenceSource::DbtPlanContract),
            );
            return;
        }
    };
    for key in keys
        .into_iter()
        .filter(|key| key.ends_with("_cleanse.json") || key.ends_with("_model.json"))
    {
        let Ok(bytes) = retry_get_bytes(sctx.storage().as_ref(), &key).await else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        add_plan_value_lineage(&key, &value, builder, resolver);
    }
}

fn add_plan_value_lineage(
    plan_key: &str,
    value: &Value,
    builder: &mut GraphBuilder,
    resolver: &RelationResolver,
) {
    let Some(tasks) = value.get("tasks").and_then(Value::as_array) else {
        return;
    };
    for task in tasks {
        let task_name = task
            .get("name")
            .or_else(|| task.get("dataset_id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown_task");
        let output_dataset = plan_task_output_dataset(task)
            .and_then(|dataset| resolver.resolve_relation(dataset))
            .or_else(|| resolver.resolve_relation(task_name));
        let Some(output_dataset) = output_dataset else {
            builder.warn(
                format!("plan lineage skipped unresolved output relation for task '{task_name}'"),
                Some(LineageEvidenceSource::DbtPlanContract),
            );
            continue;
        };
        let output_table_id = ensure_warehouse_node(
            builder,
            &output_dataset,
            resolver.warehouse_metadata.clone(),
        );
        let Some(output_fields) = task
            .get("implementation_spec")
            .and_then(|spec| spec.get("output_fields"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for output in output_fields {
            let Some(output_name) = output.get("name").and_then(Value::as_str) else {
                continue;
            };
            let output_node = add_field_node(
                builder,
                &output_dataset,
                output_name,
                output_name.to_string(),
                task.get("expected_model_path")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                BTreeMap::from([("plan_task".to_string(), task_name.to_string())]),
            );
            add_contains_field_edge(
                builder,
                &output_table_id,
                &output_node,
                LineageProvenance::unverified(
                    LineageEvidenceSource::DbtPlanContract,
                    Some(plan_key.to_string()),
                    95,
                ),
            );
            let Some(lineage) = output.get("lineage").and_then(Value::as_array) else {
                continue;
            };
            for item in lineage {
                let kind = item
                    .get("lineage_kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if kind != "column" {
                    continue;
                }
                let Some(source) = item.get("source") else {
                    continue;
                };
                let Some(source_name) = source.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let source_relation = source
                    .get("relation")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .or_else(|| plan_task_input_dataset(task))
                    .unwrap_or(output_dataset.as_str());
                let Some(source_relation) = resolver.resolve_relation(source_relation) else {
                    builder.warn(
                        format!(
                            "plan lineage skipped unresolved source relation '{source_relation}' for task '{task_name}'"
                        ),
                        Some(LineageEvidenceSource::DbtPlanContract),
                    );
                    continue;
                };
                let source_table_id = ensure_warehouse_node(
                    builder,
                    &source_relation,
                    resolver.warehouse_metadata.clone(),
                );
                let source_node = add_field_node(
                    builder,
                    &source_relation,
                    source_name,
                    source_name.to_string(),
                    None,
                    BTreeMap::new(),
                );
                let provenance = LineageProvenance {
                    source: LineageEvidenceSource::DbtPlanContract,
                    status: EvidenceStatus::UserProvided,
                    confidence: 95,
                    source_ref: Some(plan_key.to_string()),
                    run_id: None,
                    thread_id: None,
                    observed_at_epoch_secs: crate::lineage_types::now_epoch_secs(),
                };
                add_contains_field_edge(
                    builder,
                    &source_table_id,
                    &source_node,
                    provenance.clone(),
                );
                add_field_lineage_edge(
                    builder,
                    LineageEdgeKind::FieldDerivesFrom,
                    &source_node,
                    &output_node,
                    provenance,
                    item.get("role")
                        .and_then(Value::as_str)
                        .map(|role| BTreeMap::from([("role".to_string(), role.to_string())]))
                        .unwrap_or_default(),
                );
            }
        }
    }
}

fn plan_task_output_dataset(task: &Value) -> Option<&str> {
    task.get("output_relation_fqn")
        .or_else(|| task.get("relation_fqn"))
        .or_else(|| task.get("expected_relation_fqn"))
        .or_else(|| task.get("dataset_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            task.get("grounded_output")
                .and_then(|value| value.get("relation_fqn"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            task.get("output")
                .and_then(|value| value.get("relation_fqn"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            task.get("grounded_outputs")
                .and_then(Value::as_array)
                .and_then(|outputs| outputs.first())
                .and_then(|output| output.get("relation_fqn"))
                .and_then(Value::as_str)
        })
}

fn plan_task_input_dataset(task: &Value) -> Option<&str> {
    task.get("grounded_inputs")
        .and_then(Value::as_array)
        .and_then(|inputs| inputs.first())
        .and_then(|input| input.get("relation_fqn"))
        .and_then(Value::as_str)
}

async fn fetch_query_history_records(
    sctx: &SuiteCtx,
    since: Option<&str>,
    limit: usize,
    include_non_select: bool,
    diagnostics: &mut Vec<LineageDiagnostic>,
) -> Vec<QueryHistoryRecord> {
    let Some(warehouse) = sctx_warehouse(sctx) else {
        push_query_history_diagnostic(
            diagnostics,
            "warehouse provider unavailable; warehouse query-history lineage skipped",
        );
        return Vec::new();
    };
    let capability = warehouse.query_history_capability();
    if let Some(message) = capability.diagnostic_message() {
        push_query_history_diagnostic(diagnostics, message);
    }
    if !capability.supported() {
        return Vec::new();
    }
    let request = QueryHistoryRequest {
        since: since.map(ToString::to_string),
        limit,
        include_non_select,
        ..QueryHistoryRequest::default()
    };
    let result = match warehouse.list_query_history(&request).await {
        Ok(result) => result,
        Err(e) => {
            push_query_history_diagnostic(
                diagnostics,
                format!(
                    "warehouse query history lookup failed: {}",
                    e.diagnostic_message()
                ),
            );
            return Vec::new();
        }
    };
    for diagnostic in result.diagnostics {
        push_query_history_diagnostic(diagnostics, diagnostic);
    }
    result.records
}

fn push_query_history_diagnostic(
    diagnostics: &mut Vec<LineageDiagnostic>,
    message: impl Into<String>,
) {
    let diagnostic = LineageDiagnostic {
        severity: LineageDiagnosticSeverity::Warning,
        message: message.into(),
        source: Some(LineageEvidenceSource::WarehouseQueryHistory),
    };
    if !diagnostics.contains(&diagnostic) {
        diagnostics.push(diagnostic);
    }
}

fn dedupe_query_history_records(records: Vec<QueryHistoryRecord>) -> Vec<QueryHistoryRecord> {
    let mut grouped: BTreeMap<String, QueryHistoryRecord> = BTreeMap::new();
    for record in records {
        let key = if !record.sql_hash.trim().is_empty() {
            record.sql_hash.clone()
        } else if !record.normalized_sql.trim().is_empty() {
            record.normalized_sql.clone()
        } else {
            record.query_id.clone()
        };
        grouped
            .entry(key)
            .and_modify(|existing| {
                if query_history_record_is_newer(&record, existing) {
                    *existing = record.clone();
                }
            })
            .or_insert(record);
    }
    grouped.into_values().collect()
}

fn query_history_record_is_newer(
    candidate: &QueryHistoryRecord,
    existing: &QueryHistoryRecord,
) -> bool {
    match (candidate.ended_at_epoch_ms, existing.ended_at_epoch_ms) {
        (Some(candidate), Some(existing)) => candidate > existing,
        (Some(_), None) => true,
        _ => false,
    }
}

fn apply_query_history_evidence(
    graph: &mut LineageGraphSnapshot,
    records: &[QueryHistoryRecord],
    resolver: &RelationResolver,
) {
    let mut builder = GraphBuilder::from_snapshot(std::mem::take(graph));
    for record in records {
        apply_query_record_evidence(record, &mut builder, resolver);
    }
    *graph = builder.finish();
}

fn apply_query_record_evidence(
    record: &QueryHistoryRecord,
    builder: &mut GraphBuilder,
    resolver: &RelationResolver,
) {
    let analysis = analyze_select_sql(&record.sql);
    if analysis.tables.is_empty()
        && analysis.output_tables.is_empty()
        && analysis.aggregate_fields.is_empty()
        && analysis
            .diagnostics
            .iter()
            .all(|diagnostic| is_low_value_query_history_diagnostic(diagnostic))
    {
        return;
    }
    if analysis.output_tables.is_empty()
        && !analysis.tables.is_empty()
        && analysis
            .tables
            .iter()
            .all(|table| is_metadata_relation(table))
    {
        return;
    }
    if analysis.output_tables.is_empty() {
        return;
    }
    let input_tables = resolver.resolve_tables(&analysis.tables);
    let output_tables = resolver.resolve_tables(&analysis.output_tables);
    let source_ref = record
        .source_ref
        .clone()
        .or_else(|| Some(record.query_id.clone()));
    let warehouse_metadata = query_warehouse_node_metadata(record);
    for table in output_tables {
        let warehouse_id = ensure_warehouse_node(builder, &table, warehouse_metadata.clone());
        let transform_id = canonical_transform_for_builder(builder, &table)
            .unwrap_or_else(|| warehouse_id.clone());
        for input in &input_tables {
            let input_id = ensure_warehouse_node(builder, input, warehouse_metadata.clone());
            if input_id == transform_id {
                continue;
            }
            let edge = edge_id(LineageEdgeKind::SelectsFrom, &input_id, &transform_id);
            if !builder.has_edge_id(&edge.0) {
                builder.add_edge(LineageEdge {
                    id: edge,
                    from_node_id: input_id,
                    to_node_id: transform_id.clone(),
                    kind: LineageEdgeKind::SelectsFrom,
                    provenance: LineageProvenance::unverified(
                        LineageEvidenceSource::WarehouseQueryHistory,
                        source_ref.clone(),
                        75,
                    ),
                    metadata: BTreeMap::new(),
                });
            }
        }
        if transform_id != warehouse_id {
            let edge = edge_id(LineageEdgeKind::Materializes, &transform_id, &warehouse_id);
            if !builder.has_edge_id(&edge.0) {
                builder.add_edge(LineageEdge {
                    id: edge,
                    from_node_id: transform_id.clone(),
                    to_node_id: warehouse_id.clone(),
                    kind: LineageEdgeKind::Materializes,
                    provenance: LineageProvenance::unverified(
                        LineageEvidenceSource::WarehouseQueryHistory,
                        source_ref.clone(),
                        70,
                    ),
                    metadata: BTreeMap::new(),
                });
            }
        }
        append_query_history_refs(builder, &transform_id, record);
        add_sql_selected_output_lineage(
            builder,
            SqlFieldLineageTarget {
                transform_entity_id: &transform_id,
                output_entity_id: Some(&warehouse_id),
                output_dataset: &table,
                input_tables: &input_tables,
                fallback_output_fields: &[],
                evidence_source: LineageEvidenceSource::WarehouseQueryHistory,
                source_ref: source_ref.clone(),
                warehouse_metadata: warehouse_metadata.clone(),
            },
            &analysis.selected_outputs,
        );
    }
    for diagnostic in analysis
        .diagnostics
        .into_iter()
        .filter(|diagnostic| !is_low_value_query_history_diagnostic(diagnostic))
    {
        builder.warn(
            format!("query {}: {diagnostic}", record.query_id),
            Some(LineageEvidenceSource::WarehouseQueryHistory),
        );
    }
}

fn canonical_transform_for_builder(
    builder: &GraphBuilder,
    dataset_id: &str,
) -> Option<LineageNodeId> {
    let dataset_id = canonical_dataset_id(dataset_id);
    let warehouse_id = dataset_node_id(&dataset_id, LineageNodeKind::WarehouseTable);
    for edge in builder.edges.values() {
        if edge.kind != LineageEdgeKind::Materializes || edge.to_node_id != warehouse_id {
            continue;
        }
        let Some(node) = builder.nodes.get(&edge.from_node_id) else {
            continue;
        };
        if node.kind == LineageNodeKind::DbtModel {
            return Some(edge.from_node_id.clone());
        }
    }
    let transform_key = format!("warehouse:{dataset_id}");
    for node in builder.nodes.values() {
        if node.kind != LineageNodeKind::Pipeline {
            continue;
        }
        if node
            .metadata
            .get("transform_key")
            .is_some_and(|value| value == &transform_key)
        {
            return Some(node.id.clone());
        }
    }
    None
}

fn append_query_history_refs(
    builder: &mut GraphBuilder,
    entity_id: &LineageNodeId,
    record: &QueryHistoryRecord,
) {
    let Some(node) = builder.nodes.get_mut(entity_id) else {
        return;
    };
    const MAX_QUERY_HISTORY_IDS: usize = 32;
    let mut ids = node
        .metadata
        .get("query_history_ids")
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default();
    if !ids.iter().any(|id| id == &record.query_id) {
        ids.push(record.query_id.clone());
    }
    if !record.sql_hash.trim().is_empty() && !ids.iter().any(|id| id == &record.sql_hash) {
        ids.push(record.sql_hash.clone());
    }
    ids.sort();
    ids.dedup();
    ids.truncate(MAX_QUERY_HISTORY_IDS);
    if let Ok(json) = serde_json::to_string(&ids) {
        node.metadata.insert("query_history_ids".to_string(), json);
    }
    if node.metadata.get("provider").is_none() && !record.provider.to_string().is_empty() {
        node.metadata
            .insert("provider".to_string(), record.provider.to_string());
    }
}

fn resolve_query_field_source(field: &str, input_tables: &[String]) -> Option<(String, String)> {
    let field = field.trim();
    if field.is_empty() || field == "*" {
        return None;
    }
    let parts = field
        .split('.')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let field_name = parts.last()?;
    if parts.len() == 1 {
        return (input_tables.len() == 1)
            .then(|| (input_tables[0].clone(), canonical_field_path(field_name)));
    }
    let relation = canonical_dataset_id(&parts[..parts.len() - 1].join("."));
    input_tables
        .iter()
        .find(|table| **table == relation || table.ends_with(&format!(".{relation}")))
        .cloned()
        .map(|table| (table, canonical_field_path(field_name)))
}

fn is_low_value_query_history_diagnostic(diagnostic: &str) -> bool {
    diagnostic
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("non-query statement ignored for lineage:")
}

fn is_metadata_relation(relation: &str) -> bool {
    relation.to_ascii_lowercase().split('.').any(|part| {
        matches!(
            part,
            "information_schema"
                | "pg_catalog"
                | "sys"
                | "system"
                | "performance_schema"
                | "mysql"
                | "sqlite_master"
                | "sqlite_schema"
        )
    })
}

fn query_warehouse_node_metadata(record: &QueryHistoryRecord) -> BTreeMap<String, String> {
    let brand = provider_brand_for_warehouse_kind(record.provider);
    BTreeMap::from([
        ("provider_brand".to_string(), brand.to_string()),
        (
            "provider_label".to_string(),
            provider_label_for_brand(brand).to_string(),
        ),
    ])
}

fn manifest_relation_fqn(node: &Value) -> Option<String> {
    let database = node.get("database").and_then(Value::as_str)?.trim();
    let schema = node.get("schema").and_then(Value::as_str)?.trim();
    let alias = node
        .get("alias")
        .or_else(|| node.get("identifier"))
        .or_else(|| node.get("name"))
        .and_then(Value::as_str)?
        .trim();
    if database.is_empty() || schema.is_empty() || alias.is_empty() {
        return None;
    }
    Some(format!("{database}.{schema}.{alias}"))
}

fn manifest_path(node: &Value) -> Option<String> {
    node.get("original_file_path")
        .or_else(|| node.get("path"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn manifest_common_metadata(unique_id: &str, node: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::from([
        ("unique_id".to_string(), unique_id.to_string()),
        ("provider_brand".to_string(), "dbt".to_string()),
        ("provider_label".to_string(), "dbt".to_string()),
    ]);
    if let Some(resource_type) = node.get("resource_type").and_then(Value::as_str) {
        out.insert("resource_type".to_string(), resource_type.to_string());
    }
    out
}

fn manifest_model_metadata(
    unique_id: &str,
    node: &Value,
    relation_fqn: Option<&str>,
) -> BTreeMap<String, String> {
    let mut out = manifest_common_metadata(unique_id, node);
    out.insert("transform_source".to_string(), "dbt_manifest".to_string());
    out.insert(
        "transform_key".to_string(),
        relation_fqn
            .map(|fqn| format!("warehouse:{fqn}"))
            .unwrap_or_else(|| format!("dbt_model:{unique_id}")),
    );
    for (metadata_key, manifest_key) in [
        ("compiled_sql", "compiled_sql"),
        ("raw_sql", "raw_sql"),
        ("compiled_sql", "compiled_code"),
        ("raw_sql", "raw_code"),
    ] {
        if out.contains_key(metadata_key) {
            continue;
        }
        if let Some(sql) = node
            .get(manifest_key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            out.insert(metadata_key.to_string(), sql.chars().take(4000).collect());
        }
    }
    out
}

fn manifest_depends_on(node: &Value) -> Vec<String> {
    node.get("depends_on")
        .and_then(|value| value.get("nodes"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn manifest_dep_node_id(dep: &str) -> LineageNodeId {
    if dep.starts_with("model.") {
        LineageNodeId::generated(format!("dbt_model:{dep}"))
    } else if dep.starts_with("source.") {
        LineageNodeId::generated(format!("dbt_source:{dep}"))
    } else if dep.starts_with("metric.") {
        LineageNodeId::generated(format!("metrics:{dep}"))
    } else {
        LineageNodeId::generated(format!("external:{dep}"))
    }
}

fn manifest_dep_node_kind(dep: &str) -> LineageNodeKind {
    if dep.starts_with("model.") {
        LineageNodeKind::DbtModel
    } else if dep.starts_with("source.") {
        LineageNodeKind::DbtSource
    } else if dep.starts_with("metric.") {
        LineageNodeKind::Metric
    } else {
        LineageNodeKind::ExternalSystem
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_relation_fqn_uses_identifier_for_sources() {
        let node = serde_json::json!({
            "database": "db",
            "schema": "raw",
            "identifier": "orders",
            "name": "orders_src"
        });
        assert_eq!(manifest_relation_fqn(&node).unwrap(), "db.raw.orders");
    }

    #[test]
    fn query_history_summary_reports_query_counts() {
        let mut graph = LineageGraphSnapshot::default();
        graph.nodes.push(LineageNode {
            id: LineageNodeId::generated("warehouse:analytics.raw.bike_hire"),
            label: "analytics.raw.bike_hire".to_string(),
            kind: LineageNodeKind::WarehouseTable,
            dataset_id: Some("analytics.raw.bike_hire".to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::from([(
                "query_history_ids".to_string(),
                r#"["q-1","q-2"]"#.to_string(),
            )]),
        });
        let summary = query_history_summary(&graph);
        assert_eq!(summary.queries_imported, 2);
    }

    #[test]
    fn dedupe_query_history_records_keeps_latest_sql_hash() {
        let records = vec![
            QueryHistoryRecord {
                query_id: "old".to_string(),
                sql_hash: "same-hash".to_string(),
                ended_at_epoch_ms: Some(1),
                ..query_history_test_record("select 1")
            },
            QueryHistoryRecord {
                query_id: "new".to_string(),
                sql_hash: "same-hash".to_string(),
                ended_at_epoch_ms: Some(2),
                ..query_history_test_record("select 1")
            },
        ];
        let deduped = dedupe_query_history_records(records);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].query_id, "new");
    }

    #[test]
    fn source_descriptor_from_s3_config_includes_branding() {
        let cfg = crate::de_config::ProvidersResolved {
            el: crate::de_config::ElToolResolved {
                skippr_input: serde_json::json!({
                    "kind": "s3",
                    "s3_bucket": "skippr-e2e-sample-data",
                    "s3_prefix": "bike-hire/"
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let source = source_descriptor_from_providers_cfg(&cfg).expect("s3 source descriptor");

        assert_eq!(source.label, "s3://skippr-e2e-sample-data/bike-hire/");
        assert_eq!(
            source.metadata.get("provider_brand").map(String::as_str),
            Some("s3")
        );
        assert_eq!(
            source.metadata.get("provider_label").map(String::as_str),
            Some("Amazon S3")
        );
    }

    #[test]
    fn source_descriptor_from_file_config_includes_file_branding() {
        let cfg = crate::de_config::ProvidersResolved {
            el: crate::de_config::ElToolResolved {
                skippr_input: serde_json::json!({
                    "kind": "file",
                    "name": "local/customers.csv"
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let source = source_descriptor_from_providers_cfg(&cfg).expect("file source descriptor");

        assert_eq!(source.label, "local/customers.csv");
        assert_eq!(
            source.metadata.get("provider_brand").map(String::as_str),
            Some("file")
        );
        assert_eq!(
            source.metadata.get("provider_label").map(String::as_str),
            Some("File")
        );
    }

    #[test]
    fn graph_builder_deduplicates_matching_diagnostics() {
        let mut builder = GraphBuilder::default();
        builder.warn(
            "plan lineage skipped unresolved source relation",
            Some(LineageEvidenceSource::DbtPlanContract),
        );
        builder.warn(
            "plan lineage skipped unresolved source relation",
            Some(LineageEvidenceSource::DbtPlanContract),
        );
        builder.info(
            "plan lineage skipped unresolved source relation",
            Some(LineageEvidenceSource::DbtPlanContract),
        );

        let graph = builder.finish();
        assert_eq!(graph.diagnostics.len(), 2);
    }

    #[test]
    fn persisted_skipprd_metadata_preserves_original_source_field_name() {
        let value = serde_json::json!({
            "name": "bike_hire",
            "enabled": true,
            "metadata": {
                "bike_hire": {
                    "fields": {
                        "bike_id": {
                            "source_field_name": "BIKE_ID",
                            "out_field_name": "bike_id",
                            "determined_type": "Long",
                            "nullable": false,
                            "field_id": 7,
                            "lineage_id": "bike_hire:bike_id"
                        }
                    }
                }
            }
        });

        let status =
            skippr_status_from_metadata_value("bike_hire", "metadata/metadata.json", &value)
                .expect("metadata status parses");
        let field = &status.namespaces[0].fields[0];

        assert_eq!(field.name, "bike_id");
        assert_eq!(field.out_field_name.as_deref(), Some("bike_id"));
        assert_eq!(field.source_field_name.as_deref(), Some("BIKE_ID"));
        assert_eq!(field.field_id, Some(7));
        assert_eq!(field.lineage_id.as_deref(), Some("bike_hire:bike_id"));
        assert!(!field.nullable);
    }

    #[test]
    fn graph_builder_merges_duplicate_case_warehouse_nodes_without_losing_metadata() {
        let mut builder = GraphBuilder::default();
        let upper_id = dataset_node_id("ANALYTICS.RAW.BIKE_HIRE", LineageNodeKind::WarehouseTable);
        let lower_id = dataset_node_id("analytics.raw.bike_hire", LineageNodeKind::WarehouseTable);
        assert_eq!(upper_id, lower_id);

        builder.add_node(LineageNode {
            id: upper_id.clone(),
            label: "analytics.raw.bike_hire".to_string(),
            kind: LineageNodeKind::WarehouseTable,
            dataset_id: Some("analytics.raw.bike_hire".to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::from([("provider_brand".to_string(), "snowflake".to_string())]),
        });
        builder.add_node(LineageNode {
            id: lower_id,
            label: "ANALYTICS.RAW.BIKE_HIRE".to_string(),
            kind: LineageNodeKind::WarehouseTable,
            dataset_id: Some("ANALYTICS.RAW.BIKE_HIRE".to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        });

        let graph = builder.finish();
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(
            graph.nodes[0]
                .metadata
                .get("provider_brand")
                .map(String::as_str),
            Some("snowflake")
        );
    }

    #[test]
    fn manifest_model_metadata_includes_bounded_sql_and_transform_key() {
        let node = serde_json::json!({
            "resource_type": "model",
            "database": "bike_hire_gold",
            "schema": "bike_hire",
            "alias": "fct_bike_hire_events",
            "compiled_sql": "select * from analytics.raw.bike_hire",
            "raw_sql": "{{ ref('stg_raw_bike_hire') }}"
        });

        let metadata = manifest_model_metadata(
            "model.project.fct_bike_hire_events",
            &node,
            Some("bike_hire_gold.bike_hire.fct_bike_hire_events"),
        );

        assert_eq!(
            metadata.get("transform_key").map(String::as_str),
            Some("warehouse:bike_hire_gold.bike_hire.fct_bike_hire_events")
        );
        assert_eq!(
            metadata.get("transform_source").map(String::as_str),
            Some("dbt_manifest")
        );
        assert_eq!(
            metadata.get("compiled_sql").map(String::as_str),
            Some("select * from analytics.raw.bike_hire")
        );
        assert_eq!(
            metadata.get("raw_sql").map(String::as_str),
            Some("{{ ref('stg_raw_bike_hire') }}")
        );
    }

    #[test]
    fn manifest_compiled_sql_adds_field_lineage_edges() {
        let manifest = serde_json::json!({
            "sources": {
                "source.project.raw_bike_hire": {
                    "resource_type": "source",
                    "database": "analytics",
                    "schema": "raw",
                    "identifier": "bike_hire",
                    "name": "bike_hire"
                }
            },
            "nodes": {
                "model.project.stg_raw_bike_hire": {
                    "resource_type": "model",
                    "name": "stg_raw_bike_hire",
                    "database": "bike_hire_silver",
                    "schema": "bike_hire",
                    "alias": "stg_raw_bike_hire",
                    "compiled_sql": "select EVENT_DATE as RIDE_DATE from analytics.raw.bike_hire",
                    "columns": {
                        "RIDE_DATE": { "name": "RIDE_DATE", "data_type": "date" }
                    },
                    "depends_on": {
                        "nodes": ["source.project.raw_bike_hire"]
                    }
                }
            }
        });
        let mut builder = GraphBuilder::default();
        let resolver = RelationResolver::new(None, Some(&manifest));

        add_manifest_sources(&manifest, &mut builder, &resolver);
        add_manifest_models(&manifest, &mut builder, &resolver);

        let graph = builder.finish();
        graph.validate().expect("manifest field lineage validates");
        let source_field = field_node_id("analytics.raw.bike_hire", "event_date");
        let output_field =
            field_node_id("bike_hire_silver.bike_hire.stg_raw_bike_hire", "ride_date");
        let model = LineageNodeId::generated("dbt_model:model.project.stg_raw_bike_hire");
        let output_table = dataset_node_id(
            "bike_hire_silver.bike_hire.stg_raw_bike_hire",
            LineageNodeKind::WarehouseTable,
        );

        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::FieldDerivesFrom
                && edge.from_node_id == source_field
                && edge.to_node_id == output_field
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::ContainsField
                && edge.from_node_id == model
                && edge.to_node_id == output_field
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::ContainsField
                && edge.from_node_id == output_table
                && edge.to_node_id == output_field
        }));
    }

    #[test]
    fn manifest_resolver_prevents_short_name_orphan_plan_nodes() {
        let manifest = serde_json::json!({
            "nodes": {
                "model.project.dim_rider": {
                    "resource_type": "model",
                    "name": "dim_rider",
                    "database": "bike_hire_gold",
                    "schema": "bike_hire",
                    "alias": "dim_rider"
                },
                "model.project.fct_events": {
                    "resource_type": "model",
                    "name": "fct_events",
                    "database": "bike_hire_gold",
                    "schema": "bike_hire",
                    "alias": "fct_events"
                }
            }
        });
        let plan = serde_json::json!({
            "tasks": [{
                "name": "fct_events",
                "implementation_spec": {
                    "output_fields": [{
                        "name": "rider_id",
                        "lineage": [{
                            "lineage_kind": "column",
                            "source": {
                                "relation": "dim_rider",
                                "name": "rider_id"
                            }
                        }]
                    }]
                }
            }]
        });
        let resolver = RelationResolver::new(None, Some(&manifest));
        let mut builder = GraphBuilder::default();

        add_plan_value_lineage("plans/model.json", &plan, &mut builder, &resolver);

        let graph = builder.finish();
        graph.validate().expect("resolved plan graph validates");
        assert!(!graph.nodes.iter().any(|node| {
            node.id == dataset_node_id("dim_rider", LineageNodeKind::WarehouseTable)
        }));
        assert!(graph.nodes.iter().any(|node| {
            node.id
                == dataset_node_id(
                    "bike_hire_gold.bike_hire.dim_rider",
                    LineageNodeKind::WarehouseTable,
                )
        }));
        assert!(graph.nodes.iter().any(|node| {
            node.id
                == dataset_node_id(
                    "bike_hire_gold.bike_hire.fct_events",
                    LineageNodeKind::WarehouseTable,
                )
        }));
    }

    #[test]
    fn query_history_short_refs_resolve_through_manifest_aliases() {
        let manifest = serde_json::json!({
            "nodes": {
                "model.project.fct_bike_hire_events": {
                    "resource_type": "model",
                    "name": "fct_bike_hire_events",
                    "database": "bike_hire_gold",
                    "schema": "bike_hire",
                    "alias": "fct_bike_hire_events"
                }
            }
        });
        let record = QueryHistoryRecord {
            provider: crate::de_config::WarehouseKind::Snowflake,
            query_id: "aggregate-query".to_string(),
            sql: "create table bike_hire_gold.bike_hire.fct_bike_hire_events as select count(bike_id) from fct_bike_hire_events".to_string(),
            normalized_sql: "create table bike_hire_gold.bike_hire.fct_bike_hire_events as select count(bike_id) from fct_bike_hire_events".to_string(),
            sql_hash: "hash".to_string(),
            started_at_epoch_ms: None,
            ended_at_epoch_ms: None,
            user: None,
            application: None,
            warehouse: None,
            database: None,
            schema: None,
            status: Default::default(),
            error: None,
            source_ref: None,
            raw_metadata: BTreeMap::new(),
        };
        let resolver = RelationResolver::new(None, Some(&manifest));
        let mut builder = GraphBuilder::default();

        apply_query_record_evidence(&record, &mut builder, &resolver);

        let graph = builder.finish();
        graph.validate().expect("resolved query graph validates");
        let short_table = dataset_node_id("fct_bike_hire_events", LineageNodeKind::WarehouseTable);
        let canonical_table = dataset_node_id(
            "bike_hire_gold.bike_hire.fct_bike_hire_events",
            LineageNodeKind::WarehouseTable,
        );
        assert!(!graph.nodes.iter().any(|node| node.id == short_table));
        assert!(graph.nodes.iter().any(|node| node.id == canonical_table));
        assert!(!graph
            .nodes
            .iter()
            .any(|node| node.kind == LineageNodeKind::Query));
    }

    #[test]
    fn query_history_ctas_connects_source_fields_to_output_table_fields() {
        let record = QueryHistoryRecord {
            provider: crate::de_config::WarehouseKind::Snowflake,
            query_id: "ctas-query".to_string(),
            sql: "create table bike_hire_gold.bike_hire.events as select EVENT_DATE as RIDE_DATE from analytics.raw.bike_hire".to_string(),
            normalized_sql: "create table bike_hire_gold.bike_hire.events as select event_date as ride_date from analytics.raw.bike_hire".to_string(),
            sql_hash: "hash".to_string(),
            started_at_epoch_ms: None,
            ended_at_epoch_ms: None,
            user: None,
            application: None,
            warehouse: None,
            database: None,
            schema: None,
            status: Default::default(),
            error: None,
            source_ref: None,
            raw_metadata: BTreeMap::new(),
        };
        let mut builder = GraphBuilder::default();
        let resolver = RelationResolver::default();

        apply_query_record_evidence(&record, &mut builder, &resolver);

        let graph = builder.finish();
        graph
            .validate()
            .expect("query CTAS field lineage validates");
        assert!(!graph
            .nodes
            .iter()
            .any(|node| node.kind == LineageNodeKind::Query));
        let source_field = field_node_id("analytics.raw.bike_hire", "event_date");
        let output_field = field_node_id("bike_hire_gold.bike_hire.events", "ride_date");
        let output_table = dataset_node_id(
            "bike_hire_gold.bike_hire.events",
            LineageNodeKind::WarehouseTable,
        );
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::FieldDerivesFrom
                && edge.from_node_id == source_field
                && edge.to_node_id == output_field
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::ContainsField
                && edge.from_node_id == output_table
                && edge.to_node_id == output_field
        }));
    }

    #[test]
    fn plan_contract_field_lineage_uses_output_relation_and_contains_fields() {
        let plan = serde_json::json!({
            "tasks": [{
                "name": "model_project_events",
                "relation_fqn": "bike_hire_gold.bike_hire.events",
                "implementation_spec": {
                    "output_fields": [{
                        "name": "EVENT_DATE",
                        "lineage": [{
                            "lineage_kind": "column",
                            "source": {
                                "relation": "analytics.raw.bike_hire",
                                "name": "EVENT_DATE"
                            },
                            "role": "projection"
                        }]
                    }]
                }
            }]
        });
        let mut builder = GraphBuilder::default();
        let resolver = RelationResolver::default();

        add_plan_value_lineage("plans/model.json", &plan, &mut builder, &resolver);

        let graph = builder.finish();
        graph.validate().expect("plan field lineage validates");
        let source_field = field_node_id("analytics.raw.bike_hire", "event_date");
        let output_field = field_node_id("bike_hire_gold.bike_hire.events", "event_date");
        let output_table = dataset_node_id(
            "bike_hire_gold.bike_hire.events",
            LineageNodeKind::WarehouseTable,
        );
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::FieldDerivesFrom
                && edge.from_node_id == source_field
                && edge.to_node_id == output_field
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::ContainsField
                && edge.from_node_id == output_table
                && edge.to_node_id == output_field
        }));
    }

    #[test]
    fn query_history_skips_metadata_introspection_queries() {
        let record = QueryHistoryRecord {
            provider: crate::de_config::WarehouseKind::Snowflake,
            query_id: "metadata-query".to_string(),
            sql: r#"SELECT COLUMN_NAME, DATA_TYPE FROM "ANALYTICS".INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA = 'RAW' AND TABLE_NAME = 'BIKE_HIRE' ORDER BY ORDINAL_POSITION"#.to_string(),
            normalized_sql: r#"select column_name, data_type from "analytics".information_schema.columns where table_schema = 'raw' and table_name = 'bike_hire' order by ordinal_position"#.to_string(),
            sql_hash: "hash".to_string(),
            started_at_epoch_ms: None,
            ended_at_epoch_ms: None,
            user: None,
            application: None,
            warehouse: None,
            database: None,
            schema: None,
            status: Default::default(),
            error: None,
            source_ref: None,
            raw_metadata: BTreeMap::new(),
        };
        let mut builder = GraphBuilder::default();
        let resolver = RelationResolver::default();

        apply_query_record_evidence(&record, &mut builder, &resolver);

        let graph = builder.finish();
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
    }

    #[test]
    fn query_history_skips_low_value_non_query_diagnostics() {
        let record = QueryHistoryRecord {
            provider: crate::de_config::WarehouseKind::Snowflake,
            query_id: "show-query".to_string(),
            sql: "SHOW objects in bike_hire_gold bike_hire limit".to_string(),
            normalized_sql: "show objects in bike_hire_gold bike_hire limit".to_string(),
            sql_hash: "hash".to_string(),
            started_at_epoch_ms: None,
            ended_at_epoch_ms: None,
            user: None,
            application: None,
            warehouse: None,
            database: None,
            schema: None,
            status: Default::default(),
            error: None,
            source_ref: None,
            raw_metadata: BTreeMap::new(),
        };
        let mut builder = GraphBuilder::default();
        let resolver = RelationResolver::default();

        apply_query_record_evidence(&record, &mut builder, &resolver);

        let graph = builder.finish();
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
        assert!(graph.diagnostics.is_empty());
    }

    #[test]
    fn query_history_field_lineage_graph_validates_independently() {
        let record = QueryHistoryRecord {
            provider: crate::de_config::WarehouseKind::Snowflake,
            query_id: "01c47dc1-0003-542f-0003-40a600225d1e".to_string(),
            sql: "create table bike_hire_gold.bike_hire.fct_bike_hire_events_agg as select count(bike_id) as bike_id from bike_hire_gold.bike_hire.fct_bike_hire_events"
                .to_string(),
            normalized_sql:
                "create table bike_hire_gold.bike_hire.fct_bike_hire_events_agg as select count(bike_id) as bike_id from bike_hire_gold.bike_hire.fct_bike_hire_events"
                    .to_string(),
            sql_hash: "hash".to_string(),
            started_at_epoch_ms: None,
            ended_at_epoch_ms: None,
            user: None,
            application: None,
            warehouse: None,
            database: None,
            schema: None,
            status: Default::default(),
            error: None,
            source_ref: None,
            raw_metadata: BTreeMap::new(),
        };
        let mut builder = GraphBuilder::default();
        let resolver = RelationResolver::default();

        apply_query_record_evidence(&record, &mut builder, &resolver);

        let graph = builder.finish();
        graph.validate().expect("query lineage graph validates");
        let source_field_id =
            field_node_id("bike_hire_gold.bike_hire.fct_bike_hire_events", "bike_id");
        let output_field_id = field_node_id(
            "bike_hire_gold.bike_hire.fct_bike_hire_events_agg",
            "bike_id",
        );
        let output_table = dataset_node_id(
            "bike_hire_gold.bike_hire.fct_bike_hire_events_agg",
            LineageNodeKind::WarehouseTable,
        );
        assert!(!graph
            .nodes
            .iter()
            .any(|node| node.kind == LineageNodeKind::Query));
        assert!(graph.nodes.iter().any(|node| node.id == source_field_id));
        assert!(graph.nodes.iter().any(|node| node.id == output_field_id));
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::ContainsField
                && edge.from_node_id == output_table
                && edge.to_node_id == output_field_id
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::AggregatesFrom
                && edge.from_node_id == source_field_id
                && edge.to_node_id == output_field_id
        }));
    }

    #[test]
    fn query_history_attaches_to_dbt_model_when_present() {
        let model_id = LineageNodeId::generated("dbt_model:model.project.events");
        let warehouse_id = dataset_node_id(
            "bike_hire_gold.bike_hire.events",
            LineageNodeKind::WarehouseTable,
        );
        let mut builder = GraphBuilder::default();
        builder.add_node(LineageNode {
            id: model_id.clone(),
            label: "events".to_string(),
            kind: LineageNodeKind::DbtModel,
            dataset_id: Some("bike_hire_gold.bike_hire.events".to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        });
        builder.add_node(LineageNode {
            id: warehouse_id.clone(),
            label: "bike_hire_gold.bike_hire.events".to_string(),
            kind: LineageNodeKind::WarehouseTable,
            dataset_id: Some("bike_hire_gold.bike_hire.events".to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        });
        builder.add_edge(LineageEdge {
            id: edge_id(LineageEdgeKind::Materializes, &model_id, &warehouse_id),
            from_node_id: model_id.clone(),
            to_node_id: warehouse_id.clone(),
            kind: LineageEdgeKind::Materializes,
            provenance: LineageProvenance::observed(
                LineageEvidenceSource::DbtManifest,
                Some("manifest.json".to_string()),
            ),
            metadata: BTreeMap::new(),
        });
        let record = QueryHistoryRecord {
            provider: crate::de_config::WarehouseKind::Snowflake,
            query_id: "ctas-query".to_string(),
            sql: "create table bike_hire_gold.bike_hire.events as select event_date from analytics.raw.bike_hire".to_string(),
            normalized_sql: "create table bike_hire_gold.bike_hire.events as select event_date from analytics.raw.bike_hire".to_string(),
            sql_hash: "hash".to_string(),
            started_at_epoch_ms: None,
            ended_at_epoch_ms: None,
            user: None,
            application: None,
            warehouse: None,
            database: None,
            schema: None,
            status: Default::default(),
            error: None,
            source_ref: None,
            raw_metadata: BTreeMap::new(),
        };
        let resolver = RelationResolver::default();
        apply_query_record_evidence(&record, &mut builder, &resolver);
        let graph = builder.finish();
        assert!(!graph
            .nodes
            .iter()
            .any(|node| node.kind == LineageNodeKind::Query));
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::SelectsFrom
                && edge.from_node_id
                    == dataset_node_id("analytics.raw.bike_hire", LineageNodeKind::WarehouseTable)
                && edge.to_node_id == model_id
        }));
    }

    fn query_history_test_record(sql: &str) -> QueryHistoryRecord {
        QueryHistoryRecord {
            provider: crate::de_config::WarehouseKind::Snowflake,
            query_id: "test-query".to_string(),
            sql: sql.to_string(),
            normalized_sql: sql.to_string(),
            sql_hash: String::new(),
            started_at_epoch_ms: None,
            ended_at_epoch_ms: None,
            user: None,
            application: None,
            warehouse: None,
            database: None,
            schema: None,
            status: Default::default(),
            error: None,
            source_ref: None,
            raw_metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn metadata_relation_detection_covers_common_system_schemas() {
        assert!(is_metadata_relation("analytics.information_schema.columns"));
        assert!(is_metadata_relation("pg_catalog.pg_class"));
        assert!(is_metadata_relation("sys.dm_exec_query_stats"));
        assert!(is_metadata_relation("system.query_log"));
        assert!(!is_metadata_relation("analytics.raw.bike_hire"));
    }
}
