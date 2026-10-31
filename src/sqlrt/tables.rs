use crate::helpers::configuration::Config;
use crate::pipeline_view::{PipelineConfigView, QueryBackend};
use crate::query_flight::TenantScope;
use aws_credential_types::provider::ProvideCredentials;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::DataFusionError;
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::empty::EmptyExec;
use datafusion::physical_plan::union::UnionExec;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::SessionContext;
use object_store::aws::AmazonS3Builder;
use object_store::ObjectStore;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{info, warn};
use url::Url;

/// Register an S3 object store for the given s3:// URL
pub async fn register_s3_object_store(ctx: &SessionContext, s3_loc: &str) {
    if let Ok(url) = Url::parse(s3_loc) {
        if url.scheme() != "s3" {
            return;
        }
        if let Some(bucket) = url.host_str() {
            // Avoid IMDS calls in environments without metadata service
            std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");
            // Preload credentials via AWS SDK (respects AWS_PROFILE/SSO/role) and export to env for object_store chain
            let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .load()
                .await;
            if let Some(region) = conf.region().map(|r| r.to_string()) {
                std::env::set_var("AWS_REGION", &region);
                std::env::set_var("AWS_DEFAULT_REGION", &region);
            }
            if let Some(provider) = conf.credentials_provider() {
                if let Ok(creds) = provider.provide_credentials().await {
                    std::env::set_var("AWS_ACCESS_KEY_ID", creds.access_key_id());
                    std::env::set_var("AWS_SECRET_ACCESS_KEY", creds.secret_access_key());
                    if let Some(tok) = creds.session_token() {
                        std::env::set_var("AWS_SESSION_TOKEN", tok);
                    }
                }
            }
            match AmazonS3Builder::from_env().with_bucket_name(bucket).build() {
                Ok(store) => {
                    let store_arc: Arc<dyn ObjectStore> = Arc::new(store);
                    // Register store for base bucket URL
                    if let Ok(endpoint) = Url::parse(&format!("s3://{}/", bucket)) {
                        ctx.runtime_env()
                            .register_object_store(&endpoint, store_arc);
                        info!("Registered S3 object store for bucket='{}'", bucket);
                    } else {
                        warn!(
                            "register_s3_object_store: failed to parse url for bucket='{}'",
                            bucket
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "register_s3_object_store: failed to build store for bucket='{}': {}",
                        bucket, e
                    );
                }
            }
        }
    }
}

pub(crate) async fn wal_table_for_namespace(
    config: &Config,
    pipeline: &str,
    namespace: &str,
) -> Result<Option<Arc<dyn TableProvider>>, DataFusionError> {
    let bound = config.bind_pipeline(pipeline);
    let Ok(pipeline_metadata) = bound.get_metadata().await else {
        return Ok(None);
    };
    let flatten = bound.get_transform_flatten_events();
    let schema = crate::ingest_work::Ingest::prepare_arrow_schema_with_metadata_for_query(
        namespace,
        &pipeline_metadata.metadata,
        flatten,
    )
    .map_err(|err| DataFusionError::Plan(err.to_string()))?;
    let key =
        skippr_lease::PipelineKey::new(config.get_tenant(), config.get_workspace_name(), pipeline)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
    if schema.fields().is_empty() && crate::buffer::wal_reader::wal_reader_for(&key).is_none() {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        crate::sqlrt::wal_table::WalTableProvider::new(
            schema,
            key,
            namespace.to_string(),
            HashSet::new(),
        ),
    )))
}

pub async fn register_namespace_view(
    ctx: &SessionContext,
    config: &Config,
    pipeline: &str,
    namespace: &str,
) -> Result<(), DataFusionError> {
    config.init().await;
    let view = PipelineConfigView::for_name(config, pipeline)
        .map_err(|err| DataFusionError::Plan(err.to_string()))?;
    match &view.backend {
        QueryBackend::Iceberg(_) => register_iceberg_union_view(ctx, namespace, &view, config)
            .await
            .map_err(|err| {
                DataFusionError::Plan(format!(
                    "Iceberg catalog unavailable for '{pipeline}.{namespace}': {err}"
                ))
            }),
        QueryBackend::WalOnly => {
            info!(
                "Registering WAL-only namespace view for pipeline '{}', namespace '{}'",
                pipeline, namespace
            );
            register_wal_namespace_view(ctx, config, pipeline, namespace).await
        }
    }
}

fn register_table_under_pipeline_schema(
    ctx: &SessionContext,
    pipeline: &str,
    namespace: &str,
    table: Arc<dyn TableProvider>,
) -> Result<(), DataFusionError> {
    use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
    use datafusion::catalog::{CatalogProvider, SchemaProvider};
    let state = ctx.state();
    let cat_list = state.catalog_list();
    let Some(catalog) = cat_list.catalog("datafusion") else {
        return Err(DataFusionError::Plan(
            "Default catalog 'datafusion' not found".to_string(),
        ));
    };
    if let Some(schema) = catalog.schema(pipeline) {
        let _ = schema.deregister_table(namespace);
        schema
            .register_table(namespace.to_string(), table)
            .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    } else if let Some(memcat) = catalog.downcast_ref::<MemoryCatalogProvider>() {
        let new_schema = Arc::new(MemorySchemaProvider::new());
        let _ = memcat.register_schema(pipeline, new_schema.clone());
        let _ = new_schema.deregister_table(namespace);
        new_schema
            .register_table(namespace.to_string(), table)
            .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    } else {
        return Err(DataFusionError::Plan(format!(
            "Cannot register schema for pipeline '{pipeline}' in default catalog"
        )));
    }
    info!("DF FQN: registered datafusion.{pipeline}.{namespace}.");
    Ok(())
}

/// Catalog namespaces that are not ingest tables become DataFusion schemas of the same name.
pub(crate) async fn register_user_namespaces(
    ctx: &SessionContext,
    config: &Config,
) -> Result<(), DataFusionError> {
    use std::collections::BTreeSet;
    let pipeline_names: BTreeSet<String> = config.pipelines.keys().cloned().collect();
    let mut lakes = Vec::new();
    for name in &pipeline_names {
        match QueryBackend::for_pipeline(config, name)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?
        {
            QueryBackend::Iceberg(lake) => {
                lakes.push(lake);
            }
            QueryBackend::WalOnly => {}
        }
    }
    let mut seen = BTreeSet::new();
    for lake in lakes {
        if !seen.insert(lake.catalog.physical_key()) {
            continue;
        }
        let catalog = crate::sqlrt::iceberg_catalog::open_iceberg_catalog(&lake.catalog)
            .await
            .map_err(DataFusionError::Plan)?;
        let listed = catalog
            .list_namespaces(None)
            .await
            .map_err(|err| DataFusionError::External(Box::new(err)))?;
        let extra = crate::sqlrt::iceberg_lake::extra_namespace_names(
            listed.iter().map(|ns| ns.to_string()),
            &pipeline_names,
        )
        .map_err(DataFusionError::Plan)?;
        let catalog_json = serde_json::to_string(&lake.catalog)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
        for name in extra {
            let ns = iceberg::NamespaceIdent::new(name.clone());
            let tables = catalog
                .list_tables(&ns)
                .await
                .map_err(|err| DataFusionError::External(Box::new(err)))?;
            for ident in tables {
                let table = catalog
                    .load_table(&ident)
                    .await
                    .map_err(|err| DataFusionError::External(Box::new(err)))?;
                let Some(snapshot_id) = table
                    .metadata()
                    .current_snapshot()
                    .map(|snap| snap.snapshot_id())
                else {
                    continue;
                };
                let provider =
                    crate::sqlrt::iceberg_table::IcebergScanTableProvider::new_with_catalog(
                        table,
                        snapshot_id,
                        catalog_json.clone(),
                        name.clone(),
                        ident.name().to_string(),
                    )?;
                register_table_under_pipeline_schema(ctx, &name, ident.name(), Arc::new(provider))?;
            }
        }
    }
    Ok(())
}

async fn register_wal_namespace_view(
    ctx: &SessionContext,
    config: &Config,
    pipeline: &str,
    namespace: &str,
) -> Result<(), DataFusionError> {
    match wal_table_for_namespace(config, pipeline, namespace).await? {
        Some(table) => register_table_under_pipeline_schema(ctx, pipeline, namespace, table),
        None => Err(DataFusionError::Plan(format!(
            "table '{pipeline}.{namespace}' not found"
        ))),
    }
}

/// Register deadletters as a recursive Parquet listing with partition columns
pub async fn register_deadletters(
    ctx: &SessionContext,
    config: &Config,
    pipeline: &str,
) -> Result<(), DataFusionError> {
    // ensure config for bucket resolution
    config.init().await;
    let bucket = config.get_skippr_s3_bucket();
    let tenant = config.get_tenant();
    let workspace = config.get_workspace_name();
    let dl_url = format!(
        "s3://{}/deadletters/{}/{}/{}/",
        bucket, tenant, workspace, pipeline
    );
    register_s3_object_store(ctx, &dl_url).await;
    // ListingTable for deadletters with partition columns
    let url =
        ListingTableUrl::parse(&dl_url).map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let fmt = ParquetFormat::default();
    let mut listing_opts = ListingOptions::new(Arc::new(fmt));
    listing_opts = listing_opts.with_file_extension(".parquet");
    let cfg = ListingTableConfig::new(url).with_listing_options(listing_opts);
    let table =
        ListingTable::try_new(cfg).map_err(|e| DataFusionError::Execution(e.to_string()))?;
    ctx.register_table("deadletters", Arc::new(table))?;
    Ok(())
}

/// Ensure the logical schema 'dbt' exists under the default 'datafusion' catalog.
pub fn ensure_dbt_schema(ctx: &SessionContext) -> Result<(), DataFusionError> {
    use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
    use datafusion::catalog::CatalogProvider;
    let state = ctx.state();
    let cat_list = state.catalog_list();
    if let Some(catalog) = cat_list.catalog("datafusion") {
        if catalog.schema("dbt").is_some() {
            return Ok(());
        }
        if let Some(memcat) = catalog.downcast_ref::<MemoryCatalogProvider>() {
            let new_schema = Arc::new(MemorySchemaProvider::new());
            let _ = memcat.register_schema("dbt", new_schema);
            info!("Created DataFusion schema 'dbt'");
            Ok(())
        } else {
            Err(DataFusionError::Plan(
                "Default catalog is not a memory catalog; cannot create 'dbt' schema".to_string(),
            ))
        }
    } else {
        Err(DataFusionError::Plan(
            "Default catalog 'datafusion' not found".to_string(),
        ))
    }
}

/// Scan S3 for compiled dbt model SQL and register each as a view: dbt.<model>
pub async fn register_dbt_models(
    ctx: &SessionContext,
    config: &Config,
) -> Result<(), DataFusionError> {
    // Ensure config (tenant/workspace/bucket)
    config.init().await;
    ensure_dbt_schema(ctx)?;
    let tenant = config.get_tenant();
    let workspace = config.get_workspace_name();
    let pipelines = crate::sqlrt::registry::list_pipelines(config).await;
    let storage = crate::adapters::storage::get_storage(config);
    use std::collections::HashSet;
    let mut seen_models: HashSet<String> = HashSet::new();
    let mut total_registered: usize = 0;
    for pipeline in pipelines {
        let prefix = format!("{}/{}/{}/dbt/target/", tenant, workspace, pipeline);
        let keys = match storage.list_prefix(&prefix).await {
            Ok(k) => k,
            Err(e) => {
                warn!(
                    "register_dbt_models: list_prefix failed for '{}': {}",
                    prefix, e
                );
                continue;
            }
        };
        let mut registered_for_pipeline: usize = 0;
        for key in &keys {
            if !key.ends_with(".sql") || !key.contains("/compiled/") {
                continue;
            }
            let model = key
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_end_matches(".sql");
            if model.is_empty() {
                continue;
            }
            if seen_models.contains(model) {
                warn!(
                    "dbt model name collision: dbt.{} already registered; replacing with {}",
                    model, key
                );
            }
            match storage.get_bytes(key).await {
                Ok(bytes) => {
                    let sql_text = String::from_utf8_lossy(&bytes).to_string();
                    if sql_text.trim().is_empty() {
                        continue;
                    }
                    let view_stmt =
                        format!("CREATE OR REPLACE VIEW dbt.\"{}\" AS {}", model, sql_text);
                    match ctx.sql(&view_stmt).await {
                        Ok(df) => {
                            let _ = df.collect().await;
                            info!("Registered dbt view: dbt.{} (from {})", model, key);
                            seen_models.insert(model.to_string());
                            registered_for_pipeline += 1;
                        }
                        Err(e) => {
                            warn!(
                                "Failed to register dbt view for model '{}' from key '{}': {}",
                                model, key, e
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "register_dbt_models: failed to fetch compiled SQL '{}': {}",
                        key, e
                    );
                }
            }
        }
        if registered_for_pipeline > 0 {
            info!(
                "DBT registration: pipeline='{}' registered {} compiled model view(s)",
                pipeline, registered_for_pipeline
            );
            total_registered += registered_for_pipeline;
        } else {
            info!("DBT registration: pipeline='{}' no compiled models found under {}/dbt/target/compiled/", pipeline, pipeline);
        }
    }
    info!(
        "DBT registration: total compiled model views registered: {}",
        total_registered
    );
    Ok(())
}

pub async fn iceberg_schema_for_namespace(
    config: &Config,
    pipeline: &str,
    namespace: &str,
) -> Result<SchemaRef, DataFusionError> {
    let wrap = |err: DataFusionError| {
        DataFusionError::Plan(format!(
            "no Iceberg schema for pipeline {pipeline} namespace {namespace}: {err}"
        ))
    };
    config.init().await;
    let view = PipelineConfigView::for_name(config, pipeline)
        .map_err(|err| wrap(DataFusionError::Plan(err.to_string())))?;
    if !matches!(view.backend, QueryBackend::Iceberg(_)) {
        return Err(wrap(DataFusionError::Plan(format!(
            "pipeline {pipeline} is not Iceberg"
        ))));
    }
    let loaded = load_iceberg_scan_provider(&view, namespace)
        .await
        .map_err(wrap)?;
    Ok(datafusion::datasource::TableProvider::schema(
        loaded.provider.as_ref(),
    ))
}

/// Iceberg `namespace.table` as Flight SQL GetTables / SDE lake identity.
#[derive(Debug, Clone)]
pub struct IcebergListedTable {
    pub namespace: String,
    pub name: String,
    pub schema: SchemaRef,
}

pub async fn list_configured_iceberg_tables(
    config: &Config,
    scope: &TenantScope,
) -> Result<Vec<IcebergListedTable>, DataFusionError> {
    config.init().await;
    let pipeline_names: std::collections::BTreeSet<String> =
        config.pipelines.keys().cloned().collect();
    let mut lakes = Vec::new();
    for name in &pipeline_names {
        let view = PipelineConfigView::for_name(config, name)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
        if !scope.matches_pipeline(&view.key) {
            continue;
        }
        match QueryBackend::for_pipeline(config, name)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?
        {
            QueryBackend::Iceberg(lake) => lakes.push(lake),
            QueryBackend::WalOnly => {}
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for lake in lakes {
        if !seen.insert(lake.catalog.physical_key()) {
            continue;
        }
        let catalog = crate::sqlrt::iceberg_catalog::open_iceberg_catalog(&lake.catalog)
            .await
            .map_err(DataFusionError::Plan)?;
        let listed = catalog
            .list_namespaces(None)
            .await
            .map_err(|err| DataFusionError::External(Box::new(err)))?;
        let extra = crate::sqlrt::iceberg_lake::extra_namespace_names(
            listed.iter().map(|ns| ns.to_string()),
            &pipeline_names,
        )
        .map_err(DataFusionError::Plan)?;
        for name in extra {
            let ns = iceberg::NamespaceIdent::new(name.clone());
            let tables = catalog
                .list_tables(&ns)
                .await
                .map_err(|err| DataFusionError::External(Box::new(err)))?;
            for ident in tables {
                let table = catalog
                    .load_table(&ident)
                    .await
                    .map_err(|err| DataFusionError::External(Box::new(err)))?;
                let Some(snapshot_id) = table
                    .metadata()
                    .current_snapshot()
                    .map(|snap| snap.snapshot_id())
                else {
                    continue;
                };
                let provider =
                    crate::sqlrt::iceberg_table::IcebergScanTableProvider::new_with_catalog(
                        table,
                        snapshot_id,
                        String::new(),
                        name.clone(),
                        ident.name().to_string(),
                    )?;
                out.push(IcebergListedTable {
                    namespace: name.clone(),
                    name: ident.name().to_string(),
                    schema: datafusion::datasource::TableProvider::schema(&provider),
                });
            }
        }
    }
    Ok(out)
}

fn iceberg_lake(
    view: &PipelineConfigView,
) -> Result<&crate::sqlrt::iceberg_lake::IcebergLake, DataFusionError> {
    match &view.backend {
        QueryBackend::Iceberg(lake) => Ok(lake),
        QueryBackend::WalOnly => Err(DataFusionError::Plan(format!(
            "pipeline '{}' is not an Iceberg pipeline",
            view.key.pipeline()
        ))),
    }
}

#[cfg_attr(not(feature = "iceberg-catalog-dynamodb"), allow(dead_code))]
pub(crate) fn catalog_table_to_namespace(name: &str) -> String {
    name.to_string()
}

struct LoadedIcebergScan {
    provider: Arc<dyn TableProvider>,
    catalog_json: String,
    catalog_ns: String,
    table_name: String,
}

struct UnionPin {
    pipeline: skippr_lease::PipelineKey,
    namespace: String,
    catalog_json: String,
    catalog_ns: String,
    table_name: String,
}

async fn register_iceberg_union_view(
    ctx: &SessionContext,
    namespace: &str,
    view: &PipelineConfigView,
    config: &Config,
) -> Result<(), DataFusionError> {
    let loaded = load_iceberg_scan_provider(view, namespace).await?;
    let schema = datafusion::datasource::TableProvider::schema(loaded.provider.as_ref());
    let pipeline_name = view.key.pipeline();
    let pipeline_cfg = config
        .pipelines
        .get(pipeline_name)
        .ok_or_else(|| DataFusionError::Plan(format!("pipeline '{pipeline_name}' not found")))?;
    let bound = config.bind_pipeline(pipeline_name);
    let pipeline_type = bound.pipeline_type().map_err(DataFusionError::Plan)?;
    let source_keys = match bound.get_metadata().await {
        Ok(md) => crate::plugins::source_contract::cdc_source_key_columns(&md.source_contracts),
        Err(_) => std::collections::HashMap::new(),
    };
    let business_key_columns = crate::helpers::configuration::query_namespace_business_keys(
        pipeline_type,
        pipeline_cfg.cdc.as_ref(),
        &source_keys,
        namespace,
    );
    let pin = UnionPin {
        pipeline: view.key.clone(),
        namespace: namespace.to_string(),
        catalog_json: loaded.catalog_json,
        catalog_ns: loaded.catalog_ns,
        table_name: loaded.table_name,
    };
    if crate::buffer::wal_reader::wal_reader_for(&view.key).is_none() {
        return Err(DataFusionError::Plan(format!(
            "WAL catalog for pipeline {} is not open",
            view.key.pipeline()
        )));
    }
    let table = IcebergWalTableProvider::local(schema, business_key_columns, pin);
    register_table_under_pipeline_schema(ctx, view.key.pipeline(), namespace, Arc::new(table))?;
    Ok(())
}

enum IcebergWalMode {
    Local(UnionPin),
}

pub(crate) struct IcebergWalTableProvider {
    pub(crate) schema: SchemaRef,
    pub(crate) business_key_columns: Vec<String>,
    mode: IcebergWalMode,
}

impl IcebergWalTableProvider {
    fn local(schema: SchemaRef, business_key_columns: Vec<String>, pin: UnionPin) -> Self {
        Self {
            schema,
            business_key_columns,
            mode: IcebergWalMode::Local(pin),
        }
    }

    async fn pin_union_leaves(
        &self,
        pin: &UnionPin,
        projection: Option<&Vec<usize>>,
        projected: &SchemaRef,
    ) -> datafusion::error::Result<(Arc<dyn ExecutionPlan>, Arc<dyn ExecutionPlan>)> {
        let reader = crate::buffer::wal_reader::wal_reader_for(&pin.pipeline).ok_or_else(|| {
            DataFusionError::Plan(format!(
                "WAL catalog for pipeline {} is not open",
                pin.pipeline.pipeline()
            ))
        })?;
        let live = reader
            .live_slices(&pin.namespace)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
        let (lake, excluded, iceberg_plan, scan_schema) =
            self.load_iceberg_leaf(pin, projection, projected).await?;
        let boundary = crate::sqlrt::read_boundary::ReadBoundary::new(lake, live, &excluded);
        boundary.emit_query_pin(&pin.namespace);
        let include_cdc = !self.business_key_columns.is_empty();
        let wal_schema = if include_cdc {
            crate::sqlrt::cdc_reconcile::cdc_sidecar_schema(&scan_schema)
        } else {
            scan_schema
        };
        let wal_plan = Arc::new(crate::sqlrt::wal_table::WalScanExec::new(
            wal_schema,
            boundary.wal,
            None,
            include_cdc,
            pin.namespace.clone(),
        )?) as Arc<dyn ExecutionPlan>;
        Ok((iceberg_plan, wal_plan))
    }

    async fn load_iceberg_leaf(
        &self,
        pin: &UnionPin,
        projection: Option<&Vec<usize>>,
        projected: &SchemaRef,
    ) -> datafusion::error::Result<(
        crate::sqlrt::read_boundary::LakeSnapshot,
        HashSet<crate::wal_slice_ref::WalSliceRef>,
        Arc<dyn ExecutionPlan>,
        SchemaRef,
    )> {
        let table = crate::sqlrt::iceberg_table::reload_iceberg_table(
            &pin.catalog_json,
            &pin.catalog_ns,
            &pin.table_name,
        )
        .await?;
        let scan_schema = if projected.fields().is_empty() || !self.business_key_columns.is_empty()
        {
            self.schema.clone()
        } else {
            projected.clone()
        };
        let child_projection = if self.business_key_columns.is_empty() {
            if projected.fields().is_empty() {
                None
            } else {
                projection.cloned()
            }
        } else {
            None
        };
        Ok(match table.metadata().current_snapshot() {
            Some(snap) => {
                let snapshot_id = snap.snapshot_id();
                let excluded = crate::sqlrt::iceberg_table::excluded_slices(&table, snapshot_id)?;
                let plan = crate::sqlrt::iceberg_table::IcebergScanExec::from_table(
                    table,
                    snapshot_id,
                    scan_schema.clone(),
                    child_projection,
                    None,
                    pin.catalog_json.clone(),
                    pin.catalog_ns.clone(),
                    pin.table_name.clone(),
                )?;
                (
                    crate::sqlrt::read_boundary::LakeSnapshot::At(snapshot_id),
                    excluded,
                    Arc::new(plan) as Arc<dyn ExecutionPlan>,
                    scan_schema,
                )
            }
            None => (
                crate::sqlrt::read_boundary::LakeSnapshot::Empty,
                HashSet::new(),
                Arc::new(EmptyExec::new(scan_schema.clone())) as Arc<dyn ExecutionPlan>,
                scan_schema,
            ),
        })
    }
}

impl std::fmt::Debug for IcebergWalTableProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let IcebergWalMode::Local(pin) = &self.mode;
        let mode = "local";
        f.debug_struct("IcebergWalTableProvider")
            .field("namespace", &pin.namespace)
            .field("mode", &mode)
            .field("keys", &self.business_key_columns)
            .finish()
    }
}

#[async_trait::async_trait]
impl TableProvider for IcebergWalTableProvider {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::View
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        limit: Option<usize>,
    ) -> datafusion::error::Result<Arc<dyn ExecutionPlan>> {
        let schema = crate::sqlrt::wal_table::projected_schema(&self.schema, projection)?;
        let IcebergWalMode::Local(pin) = &self.mode;
        let (iceberg_plan, wal_plan) = self.pin_union_leaves(pin, projection, &schema).await?;
        if self.business_key_columns.is_empty() {
            let union = UnionExec::try_new(vec![iceberg_plan, wal_plan])?;
            return crate::sqlrt::cdc_reconcile::wrap_limit(union, schema, limit);
        }
        let merge = Arc::new(crate::sqlrt::cdc_reconcile::CdcReconcileExec::try_new(
            iceberg_plan,
            wal_plan,
            self.business_key_columns.clone(),
            self.schema.clone(),
        )?);
        crate::sqlrt::cdc_reconcile::wrap_limit(merge, schema, limit)
    }
}

async fn load_iceberg_scan_provider(
    view: &PipelineConfigView,
    namespace: &str,
) -> Result<LoadedIcebergScan, DataFusionError> {
    let lake = iceberg_lake(view)?;
    let table_name = namespace.to_string();
    let ident =
        iceberg::TableIdent::from_strs([lake.ingest_namespace.as_str(), table_name.as_str()])
            .map_err(|err| DataFusionError::External(Box::new(err)))?;
    let catalog = crate::sqlrt::iceberg_catalog::open_iceberg_catalog(&lake.catalog)
        .await
        .map_err(DataFusionError::Plan)?;
    let table = iceberg::Catalog::load_table(catalog.as_ref(), &ident)
        .await
        .map_err(|err| DataFusionError::External(Box::new(err)))?;
    let snapshot_id = table
        .metadata()
        .current_snapshot()
        .map(|snap| snap.snapshot_id())
        .unwrap_or(0);
    let catalog_json = serde_json::to_string(&lake.catalog)
        .map_err(|err| DataFusionError::Plan(err.to_string()))?;
    let catalog_ns = lake.ingest_namespace.clone();
    let provider = crate::sqlrt::iceberg_table::IcebergScanTableProvider::new_with_catalog(
        table,
        snapshot_id,
        catalog_json.clone(),
        catalog_ns.clone(),
        table_name.clone(),
    )?;
    Ok(LoadedIcebergScan {
        provider: Arc::new(provider),
        catalog_json,
        catalog_ns,
        table_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::datasource::MemTable;

    #[test]
    fn file_wal_query_registers_without_invented_datalake_prefix() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            prod.contains("register_wal_namespace_view"),
            "WAL-only File pipelines must register a WAL TableProvider"
        );
        assert!(
            prod.contains("prepare_arrow_schema_with_metadata_for_query"),
            "empty WAL after reclaim must still register the metadata schema so query/migrate work"
        );
        assert!(
            prod.contains("schema.fields().is_empty()")
                && prod.contains("wal_reader_for(&key).is_none()"),
            "STREAM must register a WAL provider from live slices when discover metadata is empty"
        );
        assert!(
            prod.contains("wal_reader_for"),
            "WAL child is catalog-backed WalCatalogReader, not PipelineConfigView paths"
        );
        assert!(
            !prod.contains("falling back to {}"),
            "must not invent s3://bucket/datalake/ns/ when the lake manifest is missing"
        );
        assert!(
            prod.contains("WalTableProvider::new"),
            "WAL-only query registers WalTableProvider, not a MemTable dump"
        );
        assert!(
            !prod.contains("WalReaderFactory"),
            "CatalogWalReader MemTable path is deleted"
        );
        assert!(!prod.contains("build_wal_df"));
    }

    #[test]
    fn iceberg_namespace_register_does_not_fall_back_to_wal_only() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let start = prod
            .find("QueryBackend::Iceberg(_)")
            .expect("Iceberg PipelineConfigView arm");
        let rest = &prod[start..];
        let arm_end = rest
            .find("QueryBackend::WalOnly")
            .expect("end of Iceberg arm");
        let arm = &rest[..arm_end];
        assert!(
            arm.contains("register_iceberg_union_view"),
            "Iceberg pipelines register Iceberg ∪ WAL picker, not listing"
        );
        assert!(
            !arm.contains("register_wal_namespace_view"),
            "Iceberg catalog/scan failure must not succeed as a WAL-only view"
        );
    }

    #[test]
    fn get_tables_catalog_list_errors_fail_closed() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let start = prod
            .find("pub async fn list_configured_iceberg_tables")
            .expect("list_configured_iceberg_tables");
        let body = prod[start..].split("fn iceberg_lake").next().unwrap();
        assert!(
            body.contains("list_namespaces(None)"),
            "GetTables must list every Iceberg namespace, not only ingest"
        );
        assert!(
            body.contains("IcebergListedTable"),
            "GetTables rows are Iceberg namespace.table, not pipeline.namespace"
        );
        assert!(body.contains("list_tables(&ns)"));
        assert!(!body.contains("list_iceberg_source_namespaces"));
        assert!(!body.contains("unwrap_or_else"));
        assert!(!body.contains("let Ok(view)"));
    }

    #[test]
    fn iceberg_union_registers_pipeline_namespace_fqn() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let start = prod
            .find("async fn register_iceberg_union_view")
            .expect("register_iceberg_union_view");
        let body = &prod[start..];
        let body = body.split("async fn ").nth(1).unwrap_or(body);
        assert!(
            body.contains(
                "register_table_under_pipeline_schema(ctx, view.key.pipeline(), namespace,",
            ),
            "Iceberg ∪ WAL must register datafusion.{{pipeline}}.{{namespace}}, not a bare default-schema table"
        );
        assert!(
            !body.contains("ctx.register_table(namespace, table)"),
            "bare SessionContext::register_table(namespace) is illegal next to pipeline.namespace"
        );
    }

    #[tokio::test]
    async fn register_table_under_pipeline_schema_is_two_part_fqn() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let ctx = SessionContext::new();
        let mem = MemTable::try_new(schema, vec![vec![]]).unwrap();
        register_table_under_pipeline_schema(&ctx, "orders_el", "orders_el", Arc::new(mem))
            .unwrap();
        let batches = ctx
            .sql("SELECT count(*) FROM orders_el.orders_el")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let count = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<datafusion::arrow::array::Int64Array>()
            .unwrap()
            .value(0);
        assert_eq!(count, 0);
    }

    #[test]
    fn catalog_table_to_namespace_is_the_iceberg_table_name() {
        assert_eq!(
            catalog_table_to_namespace("hla_events"),
            "hla_events".to_string()
        );
        assert_eq!(catalog_table_to_namespace("orders"), "orders".to_string());
    }

    #[test]
    fn union_scan_is_union_exec_when_keys_are_empty() {
        let src = include_str!("tables.rs");
        let scan = src
            .split("impl TableProvider for IcebergWalTableProvider")
            .nth(1)
            .expect("TableProvider impl")
            .split("async fn load_iceberg_scan_provider")
            .next()
            .unwrap();
        assert!(scan.contains("UnionExec::try_new"));
        assert!(scan.contains("CdcReconcileExec::try_new"));
        assert!(scan.contains("business_key_columns.is_empty()"));
        assert!(
            !scan.contains("self.iceberg"),
            "scan must not fall back to child TableProvider::scan"
        );
    }

    #[test]
    fn cdc_merge_uses_full_user_schema_not_projection() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let scan = prod
            .split("async fn scan(")
            .nth(1)
            .expect("scan")
            .split("async fn load_iceberg_scan_provider")
            .next()
            .unwrap();
        let merge = scan
            .split("CdcReconcileExec::try_new")
            .nth(1)
            .expect("try_new");
        let args = merge.split(")?").next().unwrap();
        assert!(
            args.contains("self.schema.clone()"),
            "CDC merge identity is the provider user schema, not the SQL projection: {args}"
        );
    }

    #[test]
    fn union_pin_cdc_is_derived_from_keys() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let pin = prod
            .split("struct UnionPin")
            .nth(1)
            .expect("UnionPin")
            .split('}')
            .next()
            .unwrap();
        assert!(
            !pin.contains("include_cdc"),
            "UnionPin must not store a second CDC switch beside business_key_columns: {pin}"
        );
    }

    #[test]
    fn cdc_scan_does_not_push_filters_into_pin() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let scan = prod
            .split("async fn scan(")
            .nth(1)
            .expect("IcebergWalTableProvider::scan")
            .split("async fn load_iceberg_scan_provider")
            .next()
            .unwrap();
        assert!(scan.contains("_filters"));
        assert!(!scan.contains(".scan(_state"));
        assert!(!scan.contains(".scan(state"));
    }

    #[test]
    fn union_provider_is_hard_cutover_to_iceberg_wal_table() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(!prod.contains("IcebergWalUnionProvider"));
        assert!(prod.contains("IcebergWalTableProvider"));
        assert!(prod.contains("CdcReconcileExec"));
        assert!(prod.contains("business_key_columns"));
        assert!(prod.contains("query_namespace_business_keys"));
        assert!(prod.contains("cdc_source_key_columns"));
        assert!(!prod.contains("get_namespace_cdc_contract"));
        assert!(!prod.contains("pipeline_cdc_contracts"));
    }

    #[test]
    fn union_scan_pins_wal_then_loads_iceberg() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let scan = prod
            .split("struct IcebergWalTableProvider")
            .nth(1)
            .expect("IcebergWalTableProvider");
        let scan = scan
            .split("async fn load_iceberg_scan_provider")
            .next()
            .expect("scan before load_iceberg_scan_provider");
        let pin_w = scan
            .find("live_slices")
            .expect("scan must pin W from WalCatalogReader::live_slices");
        let load_s = scan
            .find("reload_iceberg_table")
            .expect("scan must load S after pinning W");
        assert!(
            pin_w < load_s,
            "W-then-S: live_slices must run before reload_iceberg_table"
        );
        assert!(
            scan.contains("ReadBoundary::new"),
            "scan must freeze ReadBoundary from W, S, and E"
        );
        assert!(
            scan.contains("WalScanExec::new"),
            "local union WAL leaf must be WalScanExec over the pinned live set"
        );
        assert!(
            scan.contains("LakeSnapshot::Empty") && scan.contains("LakeSnapshot::At"),
            "no Iceberg snapshot is Empty; a current snapshot is At"
        );
        assert!(
            scan.contains("excluded_slices"),
            "scan must walk E from the snapshot loaded after W"
        );
        assert!(
            prod.contains("enum IcebergWalMode") && !prod.contains("local_pin: Option"),
            "unpinned child-provider union must be unrepresentable"
        );
        let register = prod
            .split("async fn register_iceberg_union_view")
            .nth(1)
            .expect("register_iceberg_union_view")
            .split("async fn load_iceberg_scan_provider")
            .next()
            .unwrap();
        assert!(
            register.contains("IcebergWalTableProvider::local"),
            "local Iceberg ∪ WAL pins at IcebergWalTableProvider::scan, not at registration"
        );
        assert!(
            !register.contains("IcebergWalTableProvider::clustered"),
            "clustered Iceberg ∪ WAL pin is deleted"
        );
        assert!(
            !prod.contains("struct ClusteredUnionPin"),
            "ClusteredUnionPin must not store a registration-time endpoint"
        );
        assert!(!register.contains("WalTableProvider::new"));
        assert!(!prod.contains("with_local_pin"));
        assert!(!prod.contains("with_clustered_pin"));
        let load = prod
            .split("async fn load_iceberg_scan_provider")
            .nth(1)
            .expect("load_iceberg_scan_provider");
        let load = load.split("#[cfg(test)]").next().unwrap();
        assert!(
            !load.contains("load_pinned_iceberg_table"),
            "union register must not fail closed on a table with no current snapshot"
        );
        assert!(
            load.contains("Catalog::load_table"),
            "union register loads Iceberg for schema only; S is pinned at scan"
        );
        let schema_fn = prod
            .split("pub async fn iceberg_schema_for_namespace")
            .nth(1)
            .expect("iceberg_schema_for_namespace")
            .split("/// Iceberg")
            .next()
            .unwrap();
        assert!(
            schema_fn.contains("PipelineConfigView::for_name(config, pipeline)"),
            "owner DoGet schema must use LiveWalTicket pipeline, not first matching namespace"
        );
        assert!(
            schema_fn.contains("no Iceberg schema for pipeline"),
            "missing pipeline, non-Iceberg, and missing table must share one fail-closed Iceberg schema error"
        );
        assert!(!schema_fn.contains("cfg.pipelines.keys()"));
    }

    #[test]
    fn peer_iceberg_fallback_is_deleted() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(!prod.contains("load_iceberg_from_peer"));
        assert!(!prod.contains("Ok(loaded) => (loaded, true)"));
    }

    #[test]
    fn lake_only_mode_is_deleted() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(!prod.contains("lake_only"));
        assert!(!prod.contains("SKIPPR_QUERY_LAKE_ONLY"));
        assert!(!prod.contains("None => loaded.provider"));
        assert!(!prod.contains("FlightSqlTableProvider"));
        assert!(!prod.contains("live_wal_scan"));
        assert!(!prod.contains("WalPick"));
        assert!(!prod.contains("has no local WAL copy"));
        assert!(!prod.contains("ClusteredWalPin"));
        assert!(!prod.contains("wal_endpoint: Option<SocketAddr>"));
        assert!(!prod.contains("plan_clustered_select"));
        assert!(!prod.contains("IcebergWalTableProvider::clustered"));
    }

    #[tokio::test]
    async fn wal_only_pipeline_never_reads_manifest_or_lists_s3() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let start = prod
            .find("pub async fn register_namespace_view")
            .expect("register_namespace_view");
        let body = prod[start..]
            .split("fn register_table_under_pipeline_schema")
            .next()
            .unwrap();
        assert!(body.contains("QueryBackend::WalOnly"));
        assert!(body.contains("register_wal_namespace_view"));
        assert!(!body.contains("already present in context"));
        assert!(!body.contains("get_json_opt"));
        assert!(!body.contains("ListingTable"));
        assert!(!body.contains("prefixes"));
    }
}
