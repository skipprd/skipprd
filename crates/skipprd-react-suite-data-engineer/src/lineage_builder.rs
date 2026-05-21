use std::collections::{BTreeMap, BTreeSet};

use react_core::storage::{retry_get_bytes, retry_list_prefix};
use react_core::suite::SuiteCtx;
use serde_json::Value;

use crate::ctx_ext::{sctx_catalog, sctx_datasets, sctx_skippr, sctx_warehouse, ProvidersCfgCap};
use crate::lineage_sql::analyze_select_sql;
use crate::lineage_store::{slice_graph, LineageStore};
use crate::lineage_types::{
    canonical_dataset_id, dataset_node_id, edge_id, field_node_id, LineageDiagnostic,
    LineageDiagnosticSeverity, LineageDirection, LineageEdge, LineageEdgeKind,
    LineageEvidenceSource, LineageFieldRef, LineageGraphQuery, LineageGraphSnapshot, LineageNode,
    LineageNodeId, LineageNodeKind, LineageProvenance, LineageRefreshResult,
    QueryHistoryImportSummary,
};
use crate::providers::{
    DataCatalog, DatasetCatalogProvider, EvidenceStatus, QueryHistoryRecord, QueryHistoryRequest,
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
            .and_modify(|existing| merge_node(existing, &node))
            .or_insert(node);
        id
    }

    fn add_edge(&mut self, edge: LineageEdge) {
        self.edges.insert(edge.id.0.clone(), edge);
    }

    fn warn(&mut self, message: impl Into<String>, source: Option<LineageEvidenceSource>) {
        self.diagnostics.push(LineageDiagnostic {
            severity: LineageDiagnosticSeverity::Warning,
            message: message.into(),
            source,
        });
    }

    fn info(&mut self, message: impl Into<String>, source: Option<LineageEvidenceSource>) {
        self.diagnostics.push(LineageDiagnostic {
            severity: LineageDiagnosticSeverity::Info,
            message: message.into(),
            source,
        });
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

fn merge_node(existing: &mut LineageNode, incoming: &LineageNode) {
    if existing.label.trim().is_empty() {
        existing.label = incoming.label.clone();
    }
    if existing.dataset_id.is_none() {
        existing.dataset_id = incoming.dataset_id.clone();
    }
    if existing.field.is_none() {
        existing.field = incoming.field.clone();
    }
    if existing.path.is_none() {
        existing.path = incoming.path.clone();
    }
    for (key, value) in &incoming.metadata {
        existing
            .metadata
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }
}

pub async fn refresh_lineage_graph_for_suite(
    sctx: &SuiteCtx,
    options: LineageBuildOptions,
) -> Result<LineageRefreshResult, String> {
    let mut builder = GraphBuilder::default();
    build_catalog_lineage(sctx, &mut builder).await;
    build_skipprd_metadata_lineage(sctx, &options.pipeline, &mut builder).await;
    build_dbt_manifest_lineage(sctx, &mut builder).await;
    build_plan_contract_lineage(sctx, &mut builder).await;
    if options.include_query_history {
        build_query_history_lineage(
            sctx,
            options.query_history_since.as_deref(),
            options.query_history_limit,
            &mut builder,
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
    build_query_history_lineage(sctx, since.as_deref(), limit, &mut builder).await;
    let next = builder.finish();
    next.validate()?;
    let store = LineageStore::new(sctx.storage().clone(), sctx.keyspace().clone());
    let mut current = store.read_graph(sctx.scope()).await?.unwrap_or_default();
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

async fn build_catalog_lineage(sctx: &SuiteCtx, builder: &mut GraphBuilder) {
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
            Ok(Some(data_catalog)) => add_catalog_dataset(&data_catalog, builder),
            Ok(None) => add_dataset_schema_fallback(datasets.as_ref(), &fqn, builder).await,
            Err(e) => builder.warn(
                format!("catalog read failed for {fqn}: {e}"),
                Some(LineageEvidenceSource::Catalog),
            ),
        }
    }
}

fn add_catalog_dataset(catalog: &DataCatalog, builder: &mut GraphBuilder) {
    let dataset_id = canonical_dataset_id(&catalog.dataset_id);
    let table_id = dataset_node_id(&dataset_id, LineageNodeKind::WarehouseTable);
    builder.add_node(LineageNode {
        id: table_id.clone(),
        label: dataset_id.clone(),
        kind: LineageNodeKind::WarehouseTable,
        dataset_id: Some(dataset_id.clone()),
        field: None,
        path: None,
        metadata: BTreeMap::from([
            ("catalog".to_string(), catalog.catalog.clone()),
            ("database".to_string(), catalog.database.clone()),
            ("table".to_string(), catalog.table.clone()),
        ]),
    });
    for field in &catalog.fields {
        let field_path = field
            .field_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(field.name.as_str());
        let field_id = field_node_id(&dataset_id, field_path);
        builder.add_node(LineageNode {
            id: field_id.clone(),
            label: field_path.to_string(),
            kind: LineageNodeKind::Field,
            dataset_id: Some(dataset_id.clone()),
            field: Some(LineageFieldRef {
                dataset_id: dataset_id.clone(),
                field_path: field_path.to_string(),
                field_id: None,
            }),
            path: None,
            metadata: BTreeMap::from([(
                "type".to_string(),
                field
                    .data_type
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            )]),
        });
        builder.add_edge(LineageEdge {
            id: edge_id(LineageEdgeKind::ContainsField, &table_id, &field_id),
            from_node_id: table_id.clone(),
            to_node_id: field_id,
            kind: LineageEdgeKind::ContainsField,
            provenance: LineageProvenance::observed(
                LineageEvidenceSource::Catalog,
                Some(catalog.dataset_id.clone()),
            ),
            metadata: BTreeMap::new(),
        });
    }
}

async fn add_dataset_schema_fallback(
    datasets: &dyn DatasetCatalogProvider,
    dataset_id: &str,
    builder: &mut GraphBuilder,
) {
    let Ok(parsed) = crate::providers::DatasetId::parse_fqn_strict(dataset_id) else {
        return;
    };
    let dataset_id = canonical_dataset_id(dataset_id);
    let table_id = dataset_node_id(&dataset_id, LineageNodeKind::WarehouseTable);
    builder.add_node(LineageNode {
        id: table_id.clone(),
        label: dataset_id.clone(),
        kind: LineageNodeKind::WarehouseTable,
        dataset_id: Some(dataset_id.clone()),
        field: None,
        path: None,
        metadata: BTreeMap::new(),
    });
    let Ok(cols) = datasets.get_dataset_schema(&parsed).await else {
        return;
    };
    for (name, ty) in cols {
        let field_id = field_node_id(&dataset_id, &name);
        builder.add_node(LineageNode {
            id: field_id.clone(),
            label: name.clone(),
            kind: LineageNodeKind::Field,
            dataset_id: Some(dataset_id.clone()),
            field: Some(LineageFieldRef {
                dataset_id: dataset_id.clone(),
                field_path: name.clone(),
                field_id: None,
            }),
            path: None,
            metadata: BTreeMap::from([("type".to_string(), ty)]),
        });
        builder.add_edge(LineageEdge {
            id: edge_id(LineageEdgeKind::ContainsField, &table_id, &field_id),
            from_node_id: table_id.clone(),
            to_node_id: field_id,
            kind: LineageEdgeKind::ContainsField,
            provenance: LineageProvenance::unverified(
                LineageEvidenceSource::Catalog,
                Some(dataset_id.to_string()),
                80,
            ),
            metadata: BTreeMap::new(),
        });
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
            node_id: LineageNodeId::generated(format!("raw:{}", label)),
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

async fn build_skipprd_metadata_lineage(
    sctx: &SuiteCtx,
    pipeline: &PipelineName,
    builder: &mut GraphBuilder,
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

    let status = if let Some(skippr) = sctx_skippr(sctx) {
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
        let mut source_metadata = source.metadata.clone();
        source_metadata.insert("pipeline".to_string(), pipeline.to_string());
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
            id: edge_id(LineageEdgeKind::Ingests, &raw_id, &ingest_id),
            from_node_id: raw_id.clone(),
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
            let raw_field_id = LineageNodeId::generated(format!(
                "raw_field:{pipeline}:{}#{}",
                namespace.namespace, field.name
            ));
            builder.add_node(LineageNode {
                id: raw_field_id.clone(),
                label: field.name.clone(),
                kind: LineageNodeKind::Field,
                dataset_id: None,
                field: None,
                path: None,
                metadata: BTreeMap::from([("type".to_string(), field.field_type.clone())]),
            });
            let output_field_id = field_node_id(&dataset_id, &field.name);
            builder.add_node(LineageNode {
                id: output_field_id.clone(),
                label: field.name.clone(),
                kind: LineageNodeKind::Field,
                dataset_id: Some(dataset_id.clone()),
                field: Some(LineageFieldRef {
                    dataset_id: dataset_id.clone(),
                    field_path: field.name.clone(),
                    field_id: None,
                }),
                path: None,
                metadata: BTreeMap::from([
                    ("type".to_string(), field.field_type),
                    ("nullable".to_string(), field.nullable.to_string()),
                ]),
            });
            builder.add_edge(LineageEdge {
                id: edge_id(LineageEdgeKind::ContainsField, &raw_id, &raw_field_id),
                from_node_id: raw_id.clone(),
                to_node_id: raw_field_id.clone(),
                kind: LineageEdgeKind::ContainsField,
                provenance: LineageProvenance::observed(
                    LineageEvidenceSource::SkipprdMetadata,
                    source_ref.clone(),
                ),
                metadata: BTreeMap::new(),
            });
            builder.add_edge(LineageEdge {
                id: edge_id(
                    LineageEdgeKind::FieldDerivesFrom,
                    &raw_field_id,
                    &output_field_id,
                ),
                from_node_id: raw_field_id,
                to_node_id: output_field_id,
                kind: LineageEdgeKind::FieldDerivesFrom,
                provenance: LineageProvenance::observed(
                    LineageEvidenceSource::SkipprdMetadata,
                    source_ref.clone(),
                ),
                metadata: BTreeMap::new(),
            });
        }
    }
}

async fn build_dbt_manifest_lineage(sctx: &SuiteCtx, builder: &mut GraphBuilder) {
    let Some(manifest) = load_manifest_value(sctx).await else {
        builder.info(
            "target/manifest.json was not available; DBT lineage skipped",
            Some(LineageEvidenceSource::DbtManifest),
        );
        return;
    };
    add_manifest_sources(&manifest, builder);
    add_manifest_models(&manifest, builder);
    add_manifest_exposures_and_metrics(&manifest, builder);
}

async fn load_manifest_value(sctx: &SuiteCtx) -> Option<Value> {
    let base = sctx
        .keyspace()
        .scoped_prefix(sctx.scope(), &["dbt"])
        .trim_end_matches('/')
        .to_string()
        + "/";
    let key = format!("{}target/manifest.json", base);
    let bytes = retry_get_bytes(sctx.storage().as_ref(), &key).await.ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

fn add_manifest_sources(manifest: &Value, builder: &mut GraphBuilder) {
    let Some(sources) = manifest.get("sources").and_then(|value| value.as_object()) else {
        return;
    };
    for (unique_id, source) in sources {
        let fqn = canonical_dataset_id(&manifest_relation_fqn(source).unwrap_or_else(|| {
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
        let table_id = dataset_node_id(&fqn, LineageNodeKind::WarehouseTable);
        builder.add_node(LineageNode {
            id: table_id.clone(),
            label: fqn.clone(),
            kind: LineageNodeKind::WarehouseTable,
            dataset_id: Some(fqn),
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        });
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

fn add_manifest_models(manifest: &Value, builder: &mut GraphBuilder) {
    let Some(nodes) = manifest.get("nodes").and_then(|value| value.as_object()) else {
        return;
    };
    for (unique_id, node) in nodes {
        if node.get("resource_type").and_then(Value::as_str) != Some("model") {
            continue;
        }
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(unique_id);
        let id = LineageNodeId::generated(format!("dbt_model:{unique_id}"));
        let relation_fqn = manifest_relation_fqn(node).map(|fqn| canonical_dataset_id(&fqn));
        builder.add_node(LineageNode {
            id: id.clone(),
            label: name.to_string(),
            kind: LineageNodeKind::DbtModel,
            dataset_id: relation_fqn.clone(),
            field: None,
            path: manifest_path(node),
            metadata: manifest_common_metadata(unique_id, node),
        });

        if let Some(fqn) = relation_fqn {
            let relation_id = dataset_node_id(&fqn, LineageNodeKind::WarehouseTable);
            builder.add_node(LineageNode {
                id: relation_id.clone(),
                label: fqn.clone(),
                kind: LineageNodeKind::WarehouseTable,
                dataset_id: Some(fqn),
                field: None,
                path: None,
                metadata: BTreeMap::new(),
            });
            builder.add_edge(LineageEdge {
                id: edge_id(LineageEdgeKind::Materializes, &id, &relation_id),
                from_node_id: id.clone(),
                to_node_id: relation_id,
                kind: LineageEdgeKind::Materializes,
                provenance: LineageProvenance::observed(
                    LineageEvidenceSource::DbtManifest,
                    Some("target/manifest.json".to_string()),
                ),
                metadata: BTreeMap::new(),
            });
        }

        for dep in manifest_depends_on(node) {
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

async fn build_plan_contract_lineage(sctx: &SuiteCtx, builder: &mut GraphBuilder) {
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
        add_plan_value_lineage(&key, &value, builder);
    }
}

fn add_plan_value_lineage(plan_key: &str, value: &Value, builder: &mut GraphBuilder) {
    let Some(tasks) = value.get("tasks").and_then(Value::as_array) else {
        return;
    };
    for task in tasks {
        let task_name = task
            .get("name")
            .or_else(|| task.get("dataset_id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown_task");
        let output_dataset = task
            .get("grounded_inputs")
            .and_then(Value::as_array)
            .and_then(|inputs| inputs.first())
            .and_then(|input| input.get("relation_fqn"))
            .and_then(Value::as_str)
            .unwrap_or(task_name);
        if output_dataset.trim().is_empty() {
            continue;
        }
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
            let output_node = field_node_id(output_dataset, output_name);
            builder.add_node(LineageNode {
                id: output_node.clone(),
                label: output_name.to_string(),
                kind: LineageNodeKind::Field,
                dataset_id: Some(output_dataset.to_string()),
                field: Some(LineageFieldRef {
                    dataset_id: output_dataset.to_string(),
                    field_path: output_name.to_string(),
                    field_id: None,
                }),
                path: task
                    .get("expected_model_path")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                metadata: BTreeMap::from([("plan_task".to_string(), task_name.to_string())]),
            });
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
                    .unwrap_or(task_name);
                if source_relation.trim().is_empty() {
                    continue;
                }
                let source_node = field_node_id(source_relation, source_name);
                builder.add_node(LineageNode {
                    id: source_node.clone(),
                    label: source_name.to_string(),
                    kind: LineageNodeKind::Field,
                    dataset_id: Some(source_relation.to_string()),
                    field: Some(LineageFieldRef {
                        dataset_id: source_relation.to_string(),
                        field_path: source_name.to_string(),
                        field_id: None,
                    }),
                    path: None,
                    metadata: BTreeMap::new(),
                });
                builder.add_edge(LineageEdge {
                    id: edge_id(
                        LineageEdgeKind::FieldDerivesFrom,
                        &source_node,
                        &output_node,
                    ),
                    from_node_id: source_node,
                    to_node_id: output_node.clone(),
                    kind: LineageEdgeKind::FieldDerivesFrom,
                    provenance: LineageProvenance {
                        source: LineageEvidenceSource::DbtPlanContract,
                        status: EvidenceStatus::UserProvided,
                        confidence: 95,
                        source_ref: Some(plan_key.to_string()),
                        run_id: None,
                        thread_id: None,
                        observed_at_epoch_secs: crate::lineage_types::now_epoch_secs(),
                    },
                    metadata: item
                        .get("role")
                        .and_then(Value::as_str)
                        .map(|role| BTreeMap::from([("role".to_string(), role.to_string())]))
                        .unwrap_or_default(),
                });
            }
        }
    }
}

async fn build_query_history_lineage(
    sctx: &SuiteCtx,
    since: Option<&str>,
    limit: usize,
    builder: &mut GraphBuilder,
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
        add_query_lineage(&record, builder);
    }
}

fn add_query_lineage(record: &QueryHistoryRecord, builder: &mut GraphBuilder) {
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
    let query_node_id = LineageNodeId::generated(format!("query:{}", record.query_id));
    builder.add_node(LineageNode {
        id: query_node_id.clone(),
        label: record.query_id.clone(),
        kind: LineageNodeKind::Query,
        dataset_id: None,
        field: None,
        path: None,
        metadata: query_node_metadata(record),
    });
    for table in analysis.tables {
        let table = canonical_dataset_id(&table);
        let table_node_id = dataset_node_id(&table, LineageNodeKind::WarehouseTable);
        builder.add_node(LineageNode {
            id: table_node_id.clone(),
            label: table.clone(),
            kind: LineageNodeKind::WarehouseTable,
            dataset_id: Some(table),
            field: None,
            path: None,
            metadata: query_warehouse_node_metadata(record),
        });
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
    for table in analysis.output_tables {
        let table = canonical_dataset_id(&table);
        let table_node_id = dataset_node_id(&table, LineageNodeKind::WarehouseTable);
        builder.add_node(LineageNode {
            id: table_node_id.clone(),
            label: table.clone(),
            kind: LineageNodeKind::WarehouseTable,
            dataset_id: Some(table),
            field: None,
            path: None,
            metadata: query_warehouse_node_metadata(record),
        });
        builder.add_edge(LineageEdge {
            id: edge_id(
                LineageEdgeKind::Materializes,
                &query_node_id,
                &table_node_id,
            ),
            from_node_id: query_node_id.clone(),
            to_node_id: table_node_id,
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
    }
    for field in analysis.aggregate_fields {
        let field_node_id =
            LineageNodeId::generated(format!("query_field:{}:{field}", record.query_id));
        builder.add_node(LineageNode {
            id: field_node_id.clone(),
            label: field,
            kind: LineageNodeKind::Field,
            dataset_id: None,
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        });
        builder.add_edge(LineageEdge {
            id: edge_id(
                LineageEdgeKind::AggregatesFrom,
                &field_node_id,
                &query_node_id,
            ),
            from_node_id: field_node_id,
            to_node_id: query_node_id.clone(),
            kind: LineageEdgeKind::AggregatesFrom,
            provenance: LineageProvenance::unverified(
                LineageEvidenceSource::WarehouseQueryHistory,
                Some(
                    record
                        .source_ref
                        .clone()
                        .unwrap_or_else(|| record.query_id.clone()),
                ),
                60,
            ),
            metadata: BTreeMap::new(),
        });
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
    let mut out = BTreeMap::from([("unique_id".to_string(), unique_id.to_string())]);
    if let Some(resource_type) = node.get("resource_type").and_then(Value::as_str) {
        out.insert("resource_type".to_string(), resource_type.to_string());
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

        add_query_lineage(&record, &mut builder);

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

        add_query_lineage(&record, &mut builder);

        let graph = builder.finish();
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
        assert!(graph.diagnostics.is_empty());
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
