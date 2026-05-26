use std::collections::{BTreeMap, BTreeSet};

use react_core::storage::{retry_get_bytes, retry_list_prefix};
use react_core::suite::SuiteCtx;
use serde::Serialize;
use serde_json::Value;

use crate::ctx_ext::{sctx_catalog, sctx_datasets, sctx_skippr, sctx_warehouse, ProvidersCfgCap};
use crate::lineage_sql::{analyze_select_sql, SqlSelectedOutput};
use crate::lineage_store::{merge_lineage_node, slice_graph, LineageStore};
use crate::lineage_types::{
    canonical_dataset_id, canonical_field_path, dataset_node_id, edge_id, field_node_id,
    LineageDiagnostic, LineageDiagnosticSeverity, LineageDirection, LineageEdge, LineageEdgeKind,
    LineageEvidenceSource, LineageFieldRef, LineageGraphQuery, LineageGraphSnapshot, LineageNode,
    LineageNodeId, LineageNodeKind, LineageProvenance, LineageRefreshResult,
    QueryHistoryImportSummary,
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

    fn add_graph(&mut self, graph: &LineageGraphSnapshot) {
        for node in &graph.nodes {
            if !matches!(
                node.kind,
                LineageNodeKind::WarehouseTable | LineageNodeKind::IngestTable
            ) {
                continue;
            }
            if let Some(dataset_id) = node.dataset_id.as_deref() {
                self.add_alias(dataset_id, dataset_id);
                if let Some(short_name) = canonical_dataset_id(dataset_id).rsplit('.').next() {
                    self.add_alias(short_name, dataset_id);
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
    if options.include_query_history {
        build_query_history_lineage(
            sctx,
            options.query_history_since.as_deref(),
            options.query_history_limit,
            &mut builder,
            &resolver,
        )
        .await;
    }

    let graph = builder.finish();
    graph.validate()?;
    let store = LineageStore::new(sctx.storage().clone(), sctx.keyspace().clone());
    store.write_graph(sctx.scope(), &graph).await?;
    Ok(refresh_result(graph))
}

pub async fn import_query_history_for_suite(
    sctx: &SuiteCtx,
    since: Option<String>,
    limit: usize,
) -> Result<LineageRefreshResult, String> {
    let mut builder = GraphBuilder::default();
    let cfg = sctx
        .capability::<ProvidersCfgCap>()
        .map(|cap| cap.0.clone());
    let manifest = load_manifest_value(sctx).await;
    let store = LineageStore::new(sctx.storage().clone(), sctx.keyspace().clone());
    let current_graph = store.read_graph(sctx.scope()).await?.unwrap_or_default();
    let mut resolver = RelationResolver::new(cfg.as_ref(), manifest.as_ref());
    resolver.add_graph(&current_graph);
    build_query_history_lineage(sctx, since.as_deref(), limit, &mut builder, &resolver).await;
    let next = builder.finish();
    next.validate()?;
    let mut current = current_graph;
    current.diagnostics.retain(|diagnostic| {
        diagnostic.source != Some(LineageEvidenceSource::WarehouseQueryHistory)
    });
    let merged = crate::lineage_store::merge_graphs(current, next)?;
    store.write_graph(sctx.scope(), &merged).await?;
    Ok(refresh_result(merged))
}

pub async fn load_lineage_graph_for_suite(
    sctx: &SuiteCtx,
    query: LineageGraphQuery,
) -> Result<LineageGraphSnapshot, String> {
    let store = LineageStore::new(sctx.storage().clone(), sctx.keyspace().clone());
    let graph = store.read_graph(sctx.scope()).await?.unwrap_or_default();
    Ok(slice_graph(&graph, &query))
}

fn refresh_result(graph: LineageGraphSnapshot) -> LineageRefreshResult {
    let query_history = query_history_summary(&graph);
    let projected_graph = slice_graph(
        &graph,
        &LineageGraphQuery {
            asset: None,
            field: None,
            direction: LineageDirection::Both,
        },
    );
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
    let query_nodes = graph
        .nodes
        .iter()
        .filter(|node| node.kind == LineageNodeKind::Query)
        .collect::<Vec<_>>();
    let provider = query_nodes
        .iter()
        .find_map(|node| node.metadata.get("provider").cloned());
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
    QueryHistoryImportSummary {
        provider,
        capability: Some(if raw_error.is_some() {
            "provider_error".to_string()
        } else {
            "supported".to_string()
        }),
        raw_error,
        queries_seen: query_nodes.len(),
        queries_imported: query_nodes.len(),
        queries_skipped: 0,
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

fn schema_fields_for_source_side(fields: &[SkipprFieldSchema]) -> Vec<SkipprFieldSchema> {
    fields
        .iter()
        .map(|field| {
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
            SkipprFieldSchema {
                name: source_field_name.to_string(),
                field_type: field.field_type.clone(),
                nullable: field.nullable,
                source_field_name: field.source_field_name.clone(),
                out_field_name: field.out_field_name.clone(),
                field_id: field.field_id,
                lineage_id: field.lineage_id.clone(),
            }
        })
        .collect()
}

fn schema_fields_for_pipeline_side(fields: &[SkipprFieldSchema]) -> Vec<SkipprFieldSchema> {
    fields
        .iter()
        .map(|field| {
            let output_field_name = field
                .out_field_name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(field.name.as_str());
            SkipprFieldSchema {
                name: output_field_name.to_string(),
                field_type: field.field_type.clone(),
                nullable: field.nullable,
                source_field_name: field.source_field_name.clone(),
                out_field_name: field.out_field_name.clone(),
                field_id: field.field_id,
                lineage_id: field.lineage_id.clone(),
            }
        })
        .collect()
}

fn encode_schema_fields_metadata(fields: &[SkipprFieldSchema]) -> Option<String> {
    if fields.is_empty() {
        return None;
    }
    serde_json::to_string(fields).ok()
}

fn parse_schema_fields_metadata(metadata: &BTreeMap<String, String>) -> Vec<SkipprFieldSchema> {
    metadata
        .get("schema_fields")
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default()
}

fn schema_fields_from_graph_field_nodes(
    graph: &LineageGraphSnapshot,
    node: &LineageNode,
) -> Vec<SkipprFieldSchema> {
    let dataset_id = node.dataset_id.as_deref().unwrap_or("");
    if dataset_id.is_empty() {
        return Vec::new();
    }
    let mut fields = graph
        .nodes
        .iter()
        .filter(|candidate| candidate.kind == LineageNodeKind::Field)
        .filter(|candidate| {
            candidate
                .metadata
                .get("_lineage_schema_only")
                .map(String::as_str)
                != Some("true")
        })
        .filter(|candidate| {
            candidate
                .field
                .as_ref()
                .map(|field| field.dataset_id.as_str())
                == Some(dataset_id)
        })
        .map(|candidate| {
            let field_path = candidate
                .field
                .as_ref()
                .map(|field| field.field_path.as_str())
                .unwrap_or(candidate.label.as_str());
            SkipprFieldSchema {
                name: field_path.to_string(),
                field_type: candidate.metadata.get("type").cloned().unwrap_or_default(),
                nullable: candidate
                    .metadata
                    .get("nullable")
                    .map(|value| value == "true")
                    .unwrap_or(true),
                source_field_name: candidate.metadata.get("source_field_name").cloned(),
                out_field_name: candidate.metadata.get("out_field_name").cloned(),
                field_id: candidate
                    .metadata
                    .get("field_id")
                    .and_then(|value| value.parse().ok()),
                lineage_id: candidate.metadata.get("lineage_id").cloned(),
            }
        })
        .collect::<Vec<_>>();
    fields.sort_by(|left, right| left.name.cmp(&right.name));
    fields
}

async fn skippr_schema_fields_for_node(
    sctx: &SuiteCtx,
    pipeline: &str,
    node: &LineageNode,
) -> Vec<SkipprFieldSchema> {
    let status = if let Some(skippr) = sctx_skippr(sctx) {
        skippr.show_pipeline(sctx.scope(), pipeline).await.ok()
    } else {
        None
    };
    let status = if skippr_status_missing_fields(status.as_ref()) {
        load_persisted_skipprd_metadata_status(sctx, pipeline, status.as_ref()).await
    } else {
        status
    };
    let Some(status) = status else {
        return Vec::new();
    };
    match node.kind {
        LineageNodeKind::RawSource => status
            .namespaces
            .iter()
            .flat_map(|namespace| schema_fields_for_source_side(&namespace.fields))
            .collect(),
        LineageNodeKind::Pipeline => status
            .namespaces
            .iter()
            .find(|namespace| namespace.namespace == pipeline)
            .or_else(|| status.namespaces.first())
            .map(|namespace| schema_fields_for_pipeline_side(&namespace.fields))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LineageNodeSchemaResponse {
    pub ok: bool,
    pub node_id: String,
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<SkipprFieldSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn resolve_lineage_node_schema(
    sctx: &SuiteCtx,
    pipeline: &str,
    node_id: &str,
) -> LineageNodeSchemaResponse {
    let store = LineageStore::new(sctx.storage().clone(), sctx.keyspace().clone());
    let graph = match store.read_graph(sctx.scope()).await {
        Ok(graph) => graph.unwrap_or_default(),
        Err(error) => {
            return LineageNodeSchemaResponse {
                ok: false,
                node_id: node_id.to_string(),
                kind: String::new(),
                label: String::new(),
                fields: Vec::new(),
                error: Some(error),
            };
        }
    };
    let Some(node) = graph.nodes.iter().find(|node| node.id.as_str() == node_id) else {
        return LineageNodeSchemaResponse {
            ok: false,
            node_id: node_id.to_string(),
            kind: String::new(),
            label: String::new(),
            fields: Vec::new(),
            error: Some(format!("lineage node '{node_id}' not found")),
        };
    };
    let mut fields = parse_schema_fields_metadata(&node.metadata);
    if fields.is_empty() {
        fields = schema_fields_from_graph_field_nodes(&graph, node);
    }
    if fields.is_empty()
        && matches!(
            node.kind,
            LineageNodeKind::RawSource | LineageNodeKind::Pipeline
        )
    {
        fields = skippr_schema_fields_for_node(sctx, pipeline, node).await;
    }
    LineageNodeSchemaResponse {
        ok: true,
        node_id: node.id.as_str().to_string(),
        kind: lineage_node_kind_name(&node.kind).to_string(),
        label: node.label.clone(),
        fields,
        error: None,
    }
}

fn lineage_node_kind_name(kind: &LineageNodeKind) -> &'static str {
    match kind {
        LineageNodeKind::RawSource => "raw_source",
        LineageNodeKind::Pipeline => "pipeline",
        LineageNodeKind::IngestTable => "ingest_table",
        LineageNodeKind::DbtSource => "dbt_source",
        LineageNodeKind::DbtModel => "dbt_model",
        LineageNodeKind::WarehouseTable => "warehouse_table",
        LineageNodeKind::Field => "field",
        LineageNodeKind::Query => "query",
        LineageNodeKind::Dashboard => "dashboard",
        LineageNodeKind::Metric => "metric",
        LineageNodeKind::ExternalSystem => "external_system",
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

    let mut status = if let Some(skippr) = sctx_skippr(sctx) {
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
    if skippr_status_missing_fields(status.as_ref()) {
        if let Some(persisted) =
            load_persisted_skipprd_metadata_status(sctx, pipeline, status.as_ref()).await
        {
            status = Some(persisted);
        }
    }

    let namespaces = status
        .as_ref()
        .map(|status| status.namespaces.clone())
        .filter(|namespaces| !namespaces.is_empty())
        .unwrap_or_else(|| {
            vec![crate::providers::SkipprNamespaceStatus {
                namespace: pipeline.to_string(),
                ..Default::default()
            }]
        });
    let source_ref = status
        .as_ref()
        .and_then(|status| status.metadata_location.clone());

    for namespace in namespaces {
        let raw_id = source.node_id.clone();
        let pipeline_id = LineageNodeId::generated(format!("pipeline:{pipeline}"));
        let source_schema_fields = schema_fields_for_source_side(&namespace.fields);
        let pipeline_schema_fields = schema_fields_for_pipeline_side(&namespace.fields);
        let mut source_metadata = source.metadata.clone();
        source_metadata.insert("pipeline".to_string(), pipeline.to_string());
        if let Some(json) = encode_schema_fields_metadata(&source_schema_fields) {
            source_metadata.insert("schema_fields".to_string(), json);
        }
        if let Some(ref location) = source_ref {
            source_metadata.insert("metadata_location".to_string(), location.clone());
        }
        builder.add_node(LineageNode {
            id: raw_id.clone(),
            label: source.label.clone(),
            kind: LineageNodeKind::RawSource,
            dataset_id: Some(source.dataset_id.clone()),
            field: None,
            path: source.path.clone().or_else(|| source_ref.clone()),
            metadata: source_metadata,
        });
        let dataset_id = if default_catalog.is_empty() || default_schema.is_empty() {
            canonical_dataset_id(&namespace.namespace)
        } else {
            canonical_dataset_id(&format!(
                "{}.{}.{}",
                default_catalog, default_schema, namespace.namespace
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
        if let Some(json) = encode_schema_fields_metadata(&pipeline_schema_fields) {
            pipeline_metadata.insert("schema_fields".to_string(), json);
        }
        if let Some(ref location) = source_ref {
            pipeline_metadata.insert("metadata_location".to_string(), location.clone());
        }
        builder.add_node(LineageNode {
            id: pipeline_id.clone(),
            label: pipeline.to_string(),
            kind: LineageNodeKind::Pipeline,
            dataset_id: Some(pipeline.to_string()),
            field: None,
            path: source_ref.clone(),
            metadata: pipeline_metadata,
        });
        let ingest_id = dataset_node_id(&dataset_id, LineageNodeKind::IngestTable);
        builder.add_node(LineageNode {
            id: ingest_id.clone(),
            label: dataset_id.clone(),
            kind: LineageNodeKind::IngestTable,
            dataset_id: Some(dataset_id.clone()),
            field: None,
            path: None,
            metadata: warehouse_node_metadata(cfg.as_ref()),
        });
        let warehouse_id = dataset_node_id(&dataset_id, LineageNodeKind::WarehouseTable);
        builder.add_node(LineageNode {
            id: warehouse_id.clone(),
            label: dataset_id.clone(),
            kind: LineageNodeKind::WarehouseTable,
            dataset_id: Some(dataset_id.clone()),
            field: None,
            path: None,
            metadata: warehouse_node_metadata(cfg.as_ref()),
        });
        builder.add_edge(LineageEdge {
            id: edge_id(LineageEdgeKind::Ingests, &raw_id, &pipeline_id),
            from_node_id: raw_id.clone(),
            to_node_id: pipeline_id.clone(),
            kind: LineageEdgeKind::Ingests,
            provenance: LineageProvenance::observed(
                LineageEvidenceSource::SkipprdMetadata,
                source_ref.clone(),
            ),
            metadata: BTreeMap::new(),
        });
        builder.add_edge(LineageEdge {
            id: edge_id(LineageEdgeKind::Ingests, &pipeline_id, &ingest_id),
            from_node_id: pipeline_id.clone(),
            to_node_id: ingest_id.clone(),
            kind: LineageEdgeKind::Ingests,
            provenance: LineageProvenance::observed(
                LineageEvidenceSource::SkipprdMetadata,
                source_ref.clone(),
            ),
            metadata: BTreeMap::new(),
        });
        builder.add_edge(LineageEdge {
            id: edge_id(LineageEdgeKind::Materializes, &ingest_id, &warehouse_id),
            from_node_id: ingest_id.clone(),
            to_node_id: warehouse_id,
            kind: LineageEdgeKind::Materializes,
            provenance: LineageProvenance::observed(
                LineageEvidenceSource::SkipprdMetadata,
                source_ref.clone(),
            ),
            metadata: BTreeMap::new(),
        });
        for field in namespace.fields {
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
                &dataset_id,
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
            add_contains_field_edge(
                builder,
                &raw_id,
                &source_field_id,
                LineageProvenance::observed(
                    LineageEvidenceSource::SkipprdMetadata,
                    source_ref.clone(),
                ),
            );
            add_contains_field_edge(
                builder,
                &pipeline_id,
                &pipeline_field_id,
                LineageProvenance::observed(
                    LineageEvidenceSource::SkipprdMetadata,
                    source_ref.clone(),
                ),
            );
            add_contains_field_edge(
                builder,
                &ingest_id,
                &output_field_id,
                LineageProvenance::observed(
                    LineageEvidenceSource::SkipprdMetadata,
                    source_ref.clone(),
                ),
            );
            add_field_lineage_edge(
                builder,
                LineageEdgeKind::FieldDerivesFrom,
                &source_field_id,
                &pipeline_field_id,
                LineageProvenance::observed(
                    LineageEvidenceSource::SkipprdMetadata,
                    source_ref.clone(),
                ),
                BTreeMap::new(),
            );
            add_field_lineage_edge(
                builder,
                LineageEdgeKind::FieldDerivesFrom,
                &pipeline_field_id,
                &output_field_id,
                LineageProvenance::observed(
                    LineageEvidenceSource::SkipprdMetadata,
                    source_ref.clone(),
                ),
                BTreeMap::new(),
            );
        }
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

async fn build_query_history_lineage(
    sctx: &SuiteCtx,
    since: Option<&str>,
    limit: usize,
    builder: &mut GraphBuilder,
    resolver: &RelationResolver,
) {
    let Some(warehouse) = sctx_warehouse(sctx) else {
        builder.warn(
            "warehouse provider unavailable; warehouse query-history lineage skipped",
            Some(LineageEvidenceSource::WarehouseQueryHistory),
        );
        return;
    };
    let capability = warehouse.query_history_capability();
    if let Some(message) = capability.diagnostic_message() {
        builder.warn(message, Some(LineageEvidenceSource::WarehouseQueryHistory));
    }
    if !capability.supported() {
        return;
    }
    let request = QueryHistoryRequest {
        since: since.map(ToString::to_string),
        limit,
        include_non_select: true,
        ..QueryHistoryRequest::default()
    };
    let result = match warehouse.list_query_history(&request).await {
        Ok(result) => result,
        Err(e) => {
            builder.warn(
                format!(
                    "warehouse query history lookup failed: {}",
                    e.diagnostic_message()
                ),
                Some(LineageEvidenceSource::WarehouseQueryHistory),
            );
            return;
        }
    };
    for diagnostic in result.diagnostics {
        builder.warn(
            diagnostic,
            Some(LineageEvidenceSource::WarehouseQueryHistory),
        );
    }
    for record in result.records {
        add_query_lineage(&record, builder, resolver);
    }
}

fn add_query_lineage(
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
    let query_dataset_id = format!("query:{}", record.query_id);
    let query_node_id = LineageNodeId::generated(query_dataset_id.clone());
    let output_tables = resolver.resolve_tables(&analysis.output_tables);
    let mut query_metadata = query_node_metadata(record);
    if output_tables.len() == 1 {
        query_metadata.insert(
            "transform_key".to_string(),
            format!("warehouse:{}", output_tables[0]),
        );
        query_metadata.insert(
            "transform_source".to_string(),
            "warehouse_query_history".to_string(),
        );
    }
    builder.add_node(LineageNode {
        id: query_node_id.clone(),
        label: record.query_id.clone(),
        kind: LineageNodeKind::Query,
        dataset_id: Some(query_dataset_id.clone()),
        field: None,
        path: None,
        metadata: query_metadata,
    });
    let input_tables = resolver.resolve_tables(&analysis.tables);
    for table in &input_tables {
        let table_node_id =
            ensure_warehouse_node(builder, table, query_warehouse_node_metadata(record));
        builder.add_edge(LineageEdge {
            id: edge_id(LineageEdgeKind::SelectsFrom, &table_node_id, &query_node_id),
            from_node_id: table_node_id,
            to_node_id: query_node_id.clone(),
            kind: LineageEdgeKind::SelectsFrom,
            provenance: LineageProvenance::unverified(
                LineageEvidenceSource::WarehouseQueryHistory,
                Some(
                    record
                        .source_ref
                        .clone()
                        .unwrap_or_else(|| record.query_id.clone()),
                ),
                75,
            ),
            metadata: BTreeMap::new(),
        });
    }
    let aggregate_fields = analysis
        .aggregate_fields
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let query_field_usages = analysis
        .selected_fields
        .iter()
        .chain(analysis.aggregate_fields.iter())
        .chain(analysis.join_fields.iter())
        .chain(analysis.filter_fields.iter())
        .filter(|field| field.trim() != "*")
        .cloned()
        .collect::<BTreeSet<_>>();
    for field in query_field_usages {
        let Some((source_dataset, field_path)) = resolve_query_field_source(&field, &input_tables)
        else {
            continue;
        };
        let query_field_id = add_field_node(
            builder,
            &query_dataset_id,
            &field_path,
            field_path.clone(),
            None,
            BTreeMap::from([
                ("query_id".to_string(), record.query_id.clone()),
                ("source_field".to_string(), field.clone()),
            ]),
        );
        add_contains_field_edge(
            builder,
            &query_node_id,
            &query_field_id,
            LineageProvenance::unverified(
                LineageEvidenceSource::WarehouseQueryHistory,
                Some(
                    record
                        .source_ref
                        .clone()
                        .unwrap_or_else(|| record.query_id.clone()),
                ),
                70,
            ),
        );
        let source_field_id = add_field_node(
            builder,
            &source_dataset,
            &field_path,
            field_path.clone(),
            None,
            BTreeMap::from([
                ("query_id".to_string(), record.query_id.clone()),
                ("source_field".to_string(), field.clone()),
            ]),
        );
        let source_table_id = ensure_warehouse_node(
            builder,
            &source_dataset,
            query_warehouse_node_metadata(record),
        );
        add_contains_field_edge(
            builder,
            &source_table_id,
            &source_field_id,
            LineageProvenance::unverified(
                LineageEvidenceSource::WarehouseQueryHistory,
                Some(
                    record
                        .source_ref
                        .clone()
                        .unwrap_or_else(|| record.query_id.clone()),
                ),
                60,
            ),
        );
        let edge_kind = if aggregate_fields.contains(&field) {
            LineageEdgeKind::AggregatesFrom
        } else {
            LineageEdgeKind::FieldDerivesFrom
        };
        add_field_lineage_edge(
            builder,
            edge_kind,
            &source_field_id,
            &query_field_id,
            LineageProvenance::unverified(
                LineageEvidenceSource::WarehouseQueryHistory,
                Some(
                    record
                        .source_ref
                        .clone()
                        .unwrap_or_else(|| record.query_id.clone()),
                ),
                60,
            ),
            BTreeMap::new(),
        );
    }
    for table in output_tables {
        let table_node_id =
            ensure_warehouse_node(builder, &table, query_warehouse_node_metadata(record));
        builder.add_edge(LineageEdge {
            id: edge_id(
                LineageEdgeKind::Materializes,
                &query_node_id,
                &table_node_id,
            ),
            from_node_id: query_node_id.clone(),
            to_node_id: table_node_id.clone(),
            kind: LineageEdgeKind::Materializes,
            provenance: LineageProvenance::unverified(
                LineageEvidenceSource::WarehouseQueryHistory,
                Some(
                    record
                        .source_ref
                        .clone()
                        .unwrap_or_else(|| record.query_id.clone()),
                ),
                70,
            ),
            metadata: BTreeMap::new(),
        });
        add_sql_selected_output_lineage(
            builder,
            SqlFieldLineageTarget {
                transform_entity_id: &query_node_id,
                output_entity_id: Some(&table_node_id),
                output_dataset: &table,
                input_tables: &input_tables,
                fallback_output_fields: &[],
                evidence_source: LineageEvidenceSource::WarehouseQueryHistory,
                source_ref: record
                    .source_ref
                    .clone()
                    .or_else(|| Some(record.query_id.clone())),
                warehouse_metadata: query_warehouse_node_metadata(record),
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

fn query_node_metadata(record: &QueryHistoryRecord) -> BTreeMap<String, String> {
    let brand = provider_brand_for_warehouse_kind(record.provider);
    let mut metadata = BTreeMap::from([
        ("provider".to_string(), record.provider.to_string()),
        ("provider_brand".to_string(), brand.to_string()),
        (
            "provider_label".to_string(),
            provider_label_for_brand(brand).to_string(),
        ),
        ("query_id".to_string(), record.query_id.clone()),
        ("sql_hash".to_string(), record.sql_hash.clone()),
        ("sql".to_string(), record.sql.chars().take(4000).collect()),
        ("status".to_string(), format!("{:?}", record.status)),
    ]);
    if let Some(value) = &record.user {
        metadata.insert("user".to_string(), value.clone());
    }
    if let Some(value) = &record.application {
        metadata.insert("application".to_string(), value.clone());
    }
    if let Some(value) = &record.warehouse {
        metadata.insert("warehouse".to_string(), value.clone());
    }
    if let Some(value) = &record.database {
        metadata.insert("database".to_string(), value.clone());
    }
    if let Some(value) = &record.schema {
        metadata.insert("schema".to_string(), value.clone());
    }
    if let Some(value) = record.started_at_epoch_ms {
        metadata.insert("started_at_epoch_ms".to_string(), value.to_string());
    }
    if let Some(value) = record.ended_at_epoch_ms {
        metadata.insert("ended_at_epoch_ms".to_string(), value.to_string());
    }
    if let Some(value) = &record.error {
        metadata.insert("error".to_string(), value.clone());
    }
    metadata
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

#[allow(dead_code)]
fn default_query() -> LineageGraphQuery {
    LineageGraphQuery {
        asset: None,
        field: None,
        direction: LineageDirection::Both,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_fields_metadata_encodes_source_and_pipeline_side_fields() {
        let fields = vec![SkipprFieldSchema {
            name: "bike_id".to_string(),
            field_type: "Long".to_string(),
            nullable: false,
            source_field_name: Some("BIKE_ID".to_string()),
            out_field_name: Some("bike_id".to_string()),
            field_id: Some(7),
            lineage_id: Some("bike_hire:bike_id".to_string()),
        }];
        let source_fields = schema_fields_for_source_side(&fields);
        let pipeline_fields = schema_fields_for_pipeline_side(&fields);

        assert_eq!(source_fields[0].name, "BIKE_ID");
        assert_eq!(pipeline_fields[0].name, "bike_id");

        let json = encode_schema_fields_metadata(&source_fields).expect("schema json");
        let parsed =
            parse_schema_fields_metadata(&BTreeMap::from([("schema_fields".to_string(), json)]));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "BIKE_ID");
        assert_eq!(parsed[0].field_type, "Long");
        assert!(!parsed[0].nullable);
    }

    #[test]
    fn schema_fields_metadata_merges_on_duplicate_nodes() {
        let fields = vec![SkipprFieldSchema {
            name: "bike_id".to_string(),
            field_type: "Long".to_string(),
            nullable: false,
            source_field_name: Some("BIKE_ID".to_string()),
            out_field_name: Some("bike_id".to_string()),
            field_id: None,
            lineage_id: None,
        }];
        let mut builder = GraphBuilder::default();
        let node_id = LineageNodeId::generated("raw:test");
        let schema_json = encode_schema_fields_metadata(&schema_fields_for_source_side(&fields))
            .expect("schema json");
        builder.add_node(LineageNode {
            id: node_id.clone(),
            label: "source".to_string(),
            kind: LineageNodeKind::RawSource,
            dataset_id: Some("source".to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::from([("schema_fields".to_string(), schema_json.clone())]),
        });
        builder.add_node(LineageNode {
            id: node_id,
            label: "source".to_string(),
            kind: LineageNodeKind::RawSource,
            dataset_id: Some("source".to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::from([(
                "schema_fields".to_string(),
                encode_schema_fields_metadata(&schema_fields_for_source_side(&[
                    SkipprFieldSchema {
                        name: "ride_id".to_string(),
                        field_type: "Long".to_string(),
                        nullable: true,
                        source_field_name: Some("RIDE_ID".to_string()),
                        out_field_name: Some("ride_id".to_string()),
                        field_id: None,
                        lineage_id: None,
                    },
                ]))
                .expect("schema json"),
            )]),
        });

        let graph = builder.finish();
        let node = graph
            .nodes
            .iter()
            .find(|node| node.kind == LineageNodeKind::RawSource)
            .expect("raw source");
        let parsed = parse_schema_fields_metadata(&node.metadata);
        assert_eq!(parsed.len(), 2);
        assert!(parsed.iter().any(|field| field.name == "BIKE_ID"));
        assert!(parsed.iter().any(|field| field.name == "RIDE_ID"));
    }

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
            id: LineageNodeId::generated("query:test"),
            label: "test".to_string(),
            kind: LineageNodeKind::Query,
            dataset_id: None,
            field: None,
            path: None,
            metadata: BTreeMap::from([("provider".to_string(), "snowflake".to_string())]),
        });
        let summary = query_history_summary(&graph);
        assert_eq!(summary.provider.as_deref(), Some("snowflake"));
        assert_eq!(summary.queries_imported, 1);
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
            sql: "select count(bike_id) from fct_bike_hire_events".to_string(),
            normalized_sql: "select count(bike_id) from fct_bike_hire_events".to_string(),
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

        add_query_lineage(&record, &mut builder, &resolver);

        let graph = builder.finish();
        graph.validate().expect("resolved query graph validates");
        let short_table = dataset_node_id("fct_bike_hire_events", LineageNodeKind::WarehouseTable);
        let canonical_table = dataset_node_id(
            "bike_hire_gold.bike_hire.fct_bike_hire_events",
            LineageNodeKind::WarehouseTable,
        );
        let query = LineageNodeId::generated("query:aggregate-query");
        assert!(!graph.nodes.iter().any(|node| node.id == short_table));
        assert!(graph.nodes.iter().any(|node| node.id == canonical_table));
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::SelectsFrom
                && edge.from_node_id == canonical_table
                && edge.to_node_id == query
        }));
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

        add_query_lineage(&record, &mut builder, &resolver);

        let graph = builder.finish();
        graph
            .validate()
            .expect("query CTAS field lineage validates");
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

        add_query_lineage(&record, &mut builder, &resolver);

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

        add_query_lineage(&record, &mut builder, &resolver);

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
            sql: "select count(bike_id) from bike_hire_gold.bike_hire.fct_bike_hire_events"
                .to_string(),
            normalized_sql:
                "select count(bike_id) from bike_hire_gold.bike_hire.fct_bike_hire_events"
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

        add_query_lineage(&record, &mut builder, &resolver);

        let graph = builder.finish();
        graph.validate().expect("query lineage graph validates");
        let source_field_id =
            field_node_id("bike_hire_gold.bike_hire.fct_bike_hire_events", "bike_id");
        let query_field_id = field_node_id("query:01c47dc1-0003-542f-0003-40a600225d1e", "bike_id");
        let table_id = dataset_node_id(
            "bike_hire_gold.bike_hire.fct_bike_hire_events",
            LineageNodeKind::WarehouseTable,
        );
        assert!(graph.nodes.iter().any(|node| node.id == source_field_id));
        assert!(graph.nodes.iter().any(|node| node.id == query_field_id));
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::ContainsField
                && edge.from_node_id == table_id
                && edge.to_node_id == source_field_id
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.kind == LineageEdgeKind::AggregatesFrom
                && edge.from_node_id == source_field_id
                && edge.to_node_id == query_field_id
        }));
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
