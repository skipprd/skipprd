use crate::cluster::identity::{ClusterIdentity, TenantScope};
use crate::cluster::peer::ReplicaRegistry;
use crate::cluster::{PipelineConfigView, QueryBackend};
use crate::helpers::configuration::Config;
use aws_credential_types::provider::ProvideCredentials;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::datasource::view::ViewTable;
use datafusion::datasource::MemTable;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::DataFusionError;
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::limit::GlobalLimitExec;
use datafusion::physical_plan::union::UnionExec;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::SessionContext;
use object_store::aws::AmazonS3Builder;
use object_store::ObjectStore;
use std::any::Any;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, info, warn};
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

async fn build_wal_df(
    config: &Config,
    ctx: &SessionContext,
    pipeline: &str,
    namespace: &str,
) -> Result<Option<datafusion::prelude::DataFrame>, DataFusionError> {
    let reader =
        crate::sqlrt::wal_reader::WalReaderFactory::for_pipeline_async(config, pipeline).await;
    let wal_batches: Vec<RecordBatch> = reader
        .load_committed_batches(namespace, usize::MAX)
        .unwrap_or_default();
    if wal_batches.is_empty() {
        return Ok(None);
    }
    let schema = wal_batches[0].schema();
    let filtered: Vec<RecordBatch> = wal_batches
        .into_iter()
        .filter(|b| b.schema().as_ref() == schema.as_ref())
        .collect();
    if filtered.is_empty() {
        return Ok(None);
    }
    let mem = MemTable::try_new(schema.clone(), vec![filtered])
        .map_err(|e| DataFusionError::Internal(e.to_string()))?;
    let tname = format!("{}_wal", namespace);
    ctx.register_table(&tname, Arc::new(mem))?;
    let df = ctx.table(&tname).await?;
    Ok(Some(df))
}

pub async fn register_namespace_view(
    ctx: &SessionContext,
    config: &Config,
    pipeline: &str,
    namespace: &str,
) -> Result<(), DataFusionError> {
    // If this table already exists in this context, skip work
    {
        let state = ctx.state();
        let cat_list = state.catalog_list();
        if let Some(catalog) = cat_list.catalog("datafusion") {
            if let Some(schema) = catalog.schema(pipeline) {
                match schema.table(namespace).await {
                    Ok(Some(_tbl)) => {
                        debug!("register_namespace_view: table already present in context: datafusion.{}.{}", pipeline, namespace);
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
    }
    config.init().await;
    let view = PipelineConfigView::for_name(config, pipeline)
        .map_err(|err| DataFusionError::Plan(err.to_string()))?;
    match &view.backend {
        QueryBackend::Iceberg(_) => register_iceberg_union_view(
            ctx,
            pipeline,
            namespace,
            &view,
            &namespace_union_opts(config),
        )
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
    } else if let Some(memcat) = catalog.as_any().downcast_ref::<MemoryCatalogProvider>() {
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
        let catalog = crate::cluster::backend::open_iceberg_catalog(&lake.catalog)
            .await
            .map_err(DataFusionError::Plan)?;
        let listed = catalog
            .list_namespaces(None)
            .await
            .map_err(|err| DataFusionError::External(Box::new(err)))?;
        let extra = crate::cluster::extra_namespace_names(
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
    let bound = config.bind_pipeline(pipeline);
    match build_wal_df(&bound, ctx, pipeline, namespace).await? {
        Some(df) => {
            let plan = df.into_optimized_plan()?;
            let view = ViewTable::new(plan, Some(namespace.to_string()));
            register_table_under_pipeline_schema(ctx, pipeline, namespace, Arc::new(view))
        }
        None => {
            warn!("register_wal_namespace_view: no WAL batches for '{pipeline}.{namespace}'");
            Ok(())
        }
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
        if let Some(memcat) = catalog.as_any().downcast_ref::<MemoryCatalogProvider>() {
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

pub struct ClusteredSelectPlan {
    pub df: datafusion::dataframe::DataFrame,
    /// Pipelines whose sink is not Iceberg. Not registered. Never silently omitted.
    pub wal_only: Vec<String>,
}

pub struct ClusteredSelectOpts {
    pub identity: ClusterIdentity,
    pub scope: TenantScope,
    pub local_flight: SocketAddr,
    pub registry: Option<Arc<ReplicaRegistry>>,
    pub lake_only: bool,
}

pub(crate) fn process_clustered_select_opts(
    scope: TenantScope,
) -> Result<ClusteredSelectOpts, DataFusionError> {
    let bind = crate::cluster::identity::process_query_bind().ok_or_else(|| {
        DataFusionError::Execution("clustered query bind is not installed".into())
    })?;
    Ok(ClusteredSelectOpts {
        identity: bind.identity,
        scope,
        local_flight: bind.flight,
        registry: crate::cluster::peer::process_registry(),
        lake_only: false,
    })
}

fn fallback_ingest_scope(config: &Config) -> TenantScope {
    TenantScope::new(config.get_tenant(), config.get_workspace_name()).unwrap_or_else(|_| {
        TenantScope {
            tenant: config.get_tenant(),
            workspace: config.get_workspace_name(),
        }
    })
}

fn namespace_union_opts(config: &Config) -> ClusteredSelectOpts {
    process_clustered_select_opts(fallback_ingest_scope(config)).unwrap_or_else(|_| {
        ClusteredSelectOpts {
            identity: ClusterIdentity::new(
                skippr_lease::ClusterId::new("local").expect("static cluster id"),
                skippr_lease::NodeId::from_uuid(uuid::Uuid::nil()),
            ),
            scope: fallback_ingest_scope(config),
            local_flight: "127.0.0.1:0".parse().unwrap(),
            registry: crate::cluster::peer::process_registry(),
            lake_only: false,
        }
    })
}

pub async fn plan_clustered_select(
    config: &Config,
    sql: &str,
    opts: &ClusteredSelectOpts,
) -> Result<ClusteredSelectPlan, DataFusionError> {
    config.init().await;
    let cfg = config.clone();
    let ctx = if opts.lake_only {
        SessionContext::new()
    } else {
        crate::query_flight::ballista::query_context()?
    };
    let mut wal_only = Vec::new();
    for name in cfg.pipelines.keys() {
        let view = PipelineConfigView::for_name(&cfg, name)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
        if !opts.scope.matches_pipeline(&view.key) {
            continue;
        }
        match &view.backend {
            QueryBackend::WalOnly => {
                wal_only.push(name.clone());
            }
            QueryBackend::Iceberg(_) => {
                let namespaces = list_iceberg_source_namespaces(&view).await?;
                if namespaces.is_empty() {
                    continue;
                }
                for namespace in namespaces {
                    let _ = ctx.deregister_table(&namespace);
                    register_iceberg_union_view(&ctx, name, &namespace, &view, opts).await?;
                }
            }
        }
    }
    register_user_namespaces(&ctx, config).await?;
    wal_only.sort();
    if !wal_only.is_empty() {
        info!(
            wal_only = ?wal_only,
            "clustered query does not register WAL-only pipelines"
        );
    }
    Ok(ClusteredSelectPlan {
        df: ctx.sql(sql).await?,
        wal_only,
    })
}

pub async fn execute_clustered_select(
    config: &Config,
    sql: &str,
    opts: &ClusteredSelectOpts,
) -> Result<Vec<RecordBatch>, DataFusionError> {
    plan_clustered_select(config, sql, opts)
        .await?
        .df
        .collect()
        .await
}

pub async fn schema_for_clustered_select(
    config: &Config,
    sql: &str,
    opts: &ClusteredSelectOpts,
) -> Result<SchemaRef, DataFusionError> {
    let ClusteredSelectPlan { df, wal_only } = plan_clustered_select(config, sql, opts).await?;
    let _ = wal_only;
    Ok(df.schema().inner().clone())
}

pub async fn plan_iceberg_scan(
    config: &Config,
    namespace: &str,
    scope: &TenantScope,
) -> Result<datafusion::dataframe::DataFrame, DataFusionError> {
    config.init().await;
    let cfg = config.clone();
    let ctx = datafusion::prelude::SessionContext::new();
    let mut found = false;
    for name in cfg.pipelines.keys() {
        let view = PipelineConfigView::for_name(&cfg, name)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
        if !matches!(view.backend, QueryBackend::Iceberg(_)) {
            continue;
        }
        if !scope.matches_pipeline(&view.key) {
            continue;
        }
        match load_iceberg_scan_provider(&view, namespace).await {
            Ok(loaded) => {
                let _ = ctx.deregister_table(namespace);
                ctx.register_table(namespace, loaded.provider)
                    .map_err(|err| DataFusionError::Execution(err.to_string()))?;
                found = true;
                break;
            }
            Err(err) => return Err(err),
        }
    }
    if !found {
        return Err(DataFusionError::Plan(format!(
            "no Iceberg snapshot for namespace '{namespace}'"
        )));
    }
    ctx.sql(&format!("SELECT * FROM {namespace}")).await
}

pub async fn execute_iceberg_scan(
    config: &Config,
    namespace: &str,
    scope: &TenantScope,
) -> Result<Vec<RecordBatch>, DataFusionError> {
    plan_iceberg_scan(config, namespace, scope)
        .await?
        .collect()
        .await
}

pub async fn iceberg_schema_for_namespace(
    config: &Config,
    namespace: &str,
) -> Result<SchemaRef, DataFusionError> {
    config.init().await;
    let cfg = config.clone();
    let mut last_err: Option<DataFusionError> = None;
    for name in cfg.pipelines.keys() {
        let view = PipelineConfigView::for_name(&cfg, name)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
        if !matches!(view.backend, QueryBackend::Iceberg(_)) {
            continue;
        }
        match load_iceberg_scan_provider(&view, namespace).await {
            Ok(loaded) => {
                return Ok(datafusion::datasource::TableProvider::schema(
                    loaded.provider.as_ref(),
                ));
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        DataFusionError::Plan(format!("no Iceberg schema for namespace '{namespace}'"))
    }))
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
        let catalog = crate::cluster::backend::open_iceberg_catalog(&lake.catalog)
            .await
            .map_err(DataFusionError::Plan)?;
        let listed = catalog
            .list_namespaces(None)
            .await
            .map_err(|err| DataFusionError::External(Box::new(err)))?;
        let extra = crate::cluster::extra_namespace_names(
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
) -> Result<&crate::cluster::IcebergLake, DataFusionError> {
    match &view.backend {
        QueryBackend::Iceberg(lake) => Ok(lake),
        QueryBackend::WalOnly => Err(DataFusionError::Plan(format!(
            "pipeline '{}' is not an Iceberg pipeline",
            view.key.pipeline()
        ))),
    }
}

#[cfg_attr(
    not(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    )),
    allow(dead_code)
)]
pub(crate) fn catalog_table_to_namespace(name: &str) -> String {
    name.to_string()
}

async fn list_iceberg_source_namespaces(
    view: &PipelineConfigView,
) -> Result<Vec<String>, DataFusionError> {
    let lake = iceberg_lake(view)?;
    let catalog = crate::cluster::backend::open_iceberg_catalog(&lake.catalog)
        .await
        .map_err(DataFusionError::Plan)?;
    let ns = iceberg::NamespaceIdent::from_strs([&lake.ingest_namespace])
        .map_err(|err| DataFusionError::External(Box::new(err)))?;
    let tables = iceberg::Catalog::list_tables(catalog.as_ref(), &ns)
        .await
        .map_err(|err| DataFusionError::External(Box::new(err)))?;
    Ok(tables
        .into_iter()
        .map(|ident| catalog_table_to_namespace(ident.name()))
        .collect())
}

struct LoadedIcebergScan {
    provider: Arc<dyn TableProvider>,
    compacted_segment_ids: Vec<String>,
}

async fn register_iceberg_union_view(
    ctx: &SessionContext,
    pipeline: &str,
    namespace: &str,
    view: &PipelineConfigView,
    opts: &ClusteredSelectOpts,
) -> Result<(), DataFusionError> {
    let (loaded, skip_wal) = match load_iceberg_scan_provider(view, namespace).await {
        Ok(loaded) => (loaded, opts.lake_only),
        Err(err) if !opts.lake_only => match load_iceberg_from_peer(namespace, opts).await {
            Ok(loaded) => (loaded, opts.lake_only),
            Err(_) => return Err(err),
        },
        Err(err) => return Err(err),
    };
    let schema = datafusion::datasource::TableProvider::schema(loaded.provider.as_ref());
    let wal_provider: Option<Arc<dyn datafusion::datasource::TableProvider>> = if skip_wal {
        None
    } else {
        wal_child_provider(
            view,
            namespace,
            schema.clone(),
            &loaded.compacted_segment_ids,
            opts,
        )
        .await?
    };
    let table: Arc<dyn TableProvider> = match wal_provider {
        Some(wal) => Arc::new(IcebergWalUnionProvider {
            iceberg: loaded.provider,
            wal,
            schema,
        }),
        None => loaded.provider,
    };
    register_table_under_pipeline_schema(ctx, pipeline, namespace, table)?;
    Ok(())
}

async fn wal_child_provider(
    view: &PipelineConfigView,
    namespace: &str,
    schema: SchemaRef,
    exclude_segment_ids: &[String],
    opts: &ClusteredSelectOpts,
) -> Result<Option<Arc<dyn TableProvider>>, DataFusionError> {
    let ads = match crate::cluster::gossip::gossip_directory() {
        Some(gossip) => gossip.known_ads().await,
        None => Vec::new(),
    };
    let paths = match crate::cluster::wal_head::local_wal_paths(&view.key, opts.registry.as_deref())
        .await
    {
        Some(paths) => Some(paths),
        None => view
            .query_wal_paths()
            .map_err(|err| DataFusionError::Plan(err.to_string()))?,
    };
    if crate::cluster::identity::process_query_bind().is_none() {
        let Some(paths) = paths else {
            return Ok(None);
        };
        return Ok(Some(Arc::new(
            crate::sqlrt::wal_table::WalTableProvider::live_unpinned(
                schema,
                view.key.clone(),
                namespace.to_string(),
                exclude_segment_ids.to_vec(),
                paths,
            ),
        )));
    }
    let local_committed = if paths.is_some() {
        crate::cluster::wal_head::local_committed_for(&view.key, opts.registry.as_deref()).await
    } else {
        None
    };
    let pipeline = view.key.clone();
    let identity = opts.identity.clone();
    let pick = crate::cluster::wal_head::pick_wal_endpoint(
        &pipeline,
        opts.local_flight,
        opts.identity.node,
        local_committed,
        &ads,
        |replica| {
            let identity = identity.clone();
            let pipeline = pipeline.clone();
            async move {
                crate::cluster::wal_head::confirm_replica_head(replica, &pipeline, &identity).await
            }
        },
    )
    .await;
    match pick {
        Some(endpoint) => Ok(Some(Arc::new(
            crate::sqlrt::flight_sql_table::FlightSqlTableProvider::live_wal(
                endpoint.to_string(),
                &view.key,
                namespace,
                exclude_segment_ids,
                schema,
                opts.scope.basic_authorization(),
            ),
        ))),
        None => Ok(None),
    }
}

async fn load_iceberg_from_peer(
    namespace: &str,
    opts: &ClusteredSelectOpts,
) -> Result<LoadedIcebergScan, DataFusionError> {
    let ads = match crate::cluster::gossip::gossip_directory() {
        Some(gossip) => gossip.known_ads().await,
        None => Vec::new(),
    };
    let mut flights: Vec<SocketAddr> = ads
        .into_iter()
        .filter(|ad| ad.ready && ad.node_id != opts.identity.node && ad.flight != opts.local_flight)
        .map(|ad| ad.flight)
        .collect();
    flights.sort_by_key(|addr| addr.to_string());
    let sql = crate::query_flight::sql::iceberg_scan_sql(namespace);
    let mut last_err = None;
    for flight in flights {
        match skippr_query_ballista::fetch_statement_schema_with_auth(
            &flight.to_string(),
            &sql,
            &opts.scope.basic_authorization(),
        )
        .await
        {
            Ok(schema) => {
                let compacted = schema
                    .metadata()
                    .get("skippr.wal-segment-ids")
                    .map(|value| {
                        crate::sqlrt::iceberg_table::segment_ids_from_snapshot_properties([
                            value.clone()
                        ])
                    })
                    .unwrap_or_default();
                let provider = crate::sqlrt::flight_sql_table::FlightSqlTableProvider::iceberg_scan(
                    flight.to_string(),
                    namespace,
                    schema,
                    opts.scope.basic_authorization(),
                );
                return Ok(LoadedIcebergScan {
                    provider: Arc::new(provider),
                    compacted_segment_ids: compacted,
                });
            }
            Err(err) => last_err = Some(err.to_string()),
        }
    }
    Err(DataFusionError::Plan(last_err.unwrap_or_else(|| {
        format!("no Iceberg snapshot for namespace '{namespace}'")
    })))
}

pub(crate) struct IcebergWalUnionProvider {
    pub(crate) iceberg: Arc<dyn TableProvider>,
    pub(crate) wal: Arc<dyn TableProvider>,
    pub(crate) schema: SchemaRef,
}

impl std::fmt::Debug for IcebergWalUnionProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IcebergWalUnionProvider").finish()
    }
}

#[async_trait::async_trait]
impl TableProvider for IcebergWalUnionProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::View
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> datafusion::error::Result<Arc<dyn ExecutionPlan>> {
        let schema = crate::sqlrt::wal_table::projected_schema(&self.schema, projection)?;
        // COUNT(*) projects no columns but still needs rows. Scan children
        // unprojected, then drop columns so the physical schema matches.
        let child_projection = if schema.fields().is_empty() {
            None
        } else {
            projection
        };
        let iceberg_plan = self
            .iceberg
            .scan(state, child_projection, filters, None)
            .await?;
        let wal_plan = self
            .wal
            .scan(state, child_projection, filters, None)
            .await?;
        let union = UnionExec::try_new(vec![iceberg_plan, wal_plan])?;
        let union = crate::sqlrt::wal_table::align_exec_to_schema(union, schema)?;
        if let Some(limit) = limit {
            Ok(Arc::new(GlobalLimitExec::new(union, 0, Some(limit))))
        } else {
            Ok(union)
        }
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
    let catalog = crate::cluster::backend::open_iceberg_catalog(&lake.catalog)
        .await
        .map_err(DataFusionError::Plan)?;
    let (table, snapshot_id) =
        crate::sqlrt::iceberg_table::load_pinned_iceberg_table(catalog, &ident).await?;
    let compacted_segment_ids = crate::sqlrt::iceberg_table::compacted_wal_segment_ids(&table);
    let catalog_json = serde_json::to_string(&lake.catalog)
        .map_err(|err| DataFusionError::Plan(err.to_string()))?;
    let provider = crate::sqlrt::iceberg_table::IcebergScanTableProvider::new_with_catalog(
        table,
        snapshot_id,
        catalog_json,
        lake.ingest_namespace.clone(),
        table_name,
    )?;
    Ok(LoadedIcebergScan {
        provider: Arc::new(provider),
        compacted_segment_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::physical_plan::empty::EmptyExec;

    #[derive(Debug)]
    struct ScanProvider {
        schema: SchemaRef,
    }

    #[async_trait::async_trait]
    impl TableProvider for ScanProvider {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn schema(&self) -> SchemaRef {
            self.schema.clone()
        }

        fn table_type(&self) -> TableType {
            TableType::Base
        }

        async fn scan(
            &self,
            _state: &dyn Session,
            projection: Option<&Vec<usize>>,
            _filters: &[Expr],
            _limit: Option<usize>,
        ) -> datafusion::error::Result<Arc<dyn ExecutionPlan>> {
            let schema = crate::sqlrt::wal_table::projected_schema(&self.schema, projection)?;
            Ok(Arc::new(EmptyExec::new(schema)))
        }
    }

    #[test]
    fn file_wal_query_registers_without_invented_datalake_prefix() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            prod.contains("register_wal_namespace_view"),
            "WAL-only File pipelines must register a WAL TableProvider"
        );
        assert!(
            prod.contains("query_wal_paths"),
            "query() has no durable store; WAL paths come from PipelineConfigView"
        );
        assert!(
            !prod.contains("falling back to {}"),
            "must not invent s3://bucket/datalake/ns/ when the lake manifest is missing"
        );
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
    fn clustered_catalog_list_errors_fail_the_plan() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let start = prod
            .find("pub async fn plan_clustered_select")
            .expect("plan_clustered_select");
        let body = prod[start..]
            .split("pub async fn execute_clustered_select")
            .next()
            .unwrap();
        assert!(
            body.contains("list_iceberg_source_namespaces(&view).await?"),
            "catalog-list errors must fail the clustered plan"
        );
        assert!(
            !body.contains("skipping"),
            "must not warn+continue past catalog-list failures"
        );
        assert!(body.contains("wal_only.push"));
        assert!(body.contains("QueryBackend::WalOnly"));
        assert!(
            body.contains("register_user_namespaces(&ctx, config).await?"),
            "clustered query must register extra Iceberg namespaces"
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
            body.contains("register_table_under_pipeline_schema(ctx, pipeline, namespace, table)"),
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

    #[tokio::test]
    async fn count_star_union_physical_schema_is_empty() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("a", DataType::Utf8, true),
            Field::new("b", DataType::Utf8, true),
            Field::new("c", DataType::Utf8, true),
            Field::new("d", DataType::Utf8, true),
        ]));
        let child = Arc::new(ScanProvider {
            schema: schema.clone(),
        });
        let provider = IcebergWalUnionProvider {
            iceberg: child.clone(),
            wal: child,
            schema,
        };
        let ctx = SessionContext::new();
        let union = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        assert_eq!(
            union.name(),
            "UnionExec",
            "UNION must be the table scan root so aggregate/join sit above it"
        );
        ctx.register_table("hla_events", Arc::new(provider))
            .unwrap();
        let batches = ctx
            .sql("SELECT count(*) FROM hla_events")
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

    #[tokio::test]
    async fn union_with_flight_sql_wal_child_is_union_of_leaves() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let iceberg = Arc::new(ScanProvider {
            schema: schema.clone(),
        });
        let wal = Arc::new(crate::sqlrt::flight_sql_table::FlightSqlTableProvider::new(
            "127.0.0.1:9".into(),
            "SELECT * FROM live_wal_scan('t','w','p','ns')".into(),
            schema.clone(),
        ));
        let provider = IcebergWalUnionProvider {
            iceberg,
            wal,
            schema,
        };
        let ctx = SessionContext::new();
        let plan = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        assert_eq!(plan.name(), "UnionExec");
        let children = plan.children();
        assert_eq!(children.len(), 2);
        assert!(
            children.iter().any(|child| child.name() == "FlightSqlExec"),
            "UNION must include FlightSqlExec WAL leaf, got {:?}",
            children.iter().map(|c| c.name()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn peer_iceberg_fallback_keeps_wal_picker() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(prod.contains("load_iceberg_from_peer"));
        assert!(!prod.contains("Ok(loaded) => (loaded, true)"));
    }

    #[test]
    fn lake_only_is_allowed_without_local_wal() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(prod.contains("lake_only: bool"));
        assert!(prod.contains("FlightSqlTableProvider::live_wal"));
        assert!(!prod.contains("WalPick"));
        assert!(prod.contains("None => Ok(None)"));
        assert!(!prod.contains("has no local WAL copy"));
        assert!(!prod.contains("ClusteredWalPin"));
        assert!(!prod.contains("wal_endpoint: Option<SocketAddr>"));
        assert!(prod.contains("clustered query bind is not installed"));
    }

    #[test]
    fn clustered_select_opts_fail_closed_without_bind() {
        crate::cluster::identity::clear_process_query_bind();
        let err = match process_clustered_select_opts(
            crate::cluster::identity::TenantScope::new("t", "w").unwrap(),
        ) {
            Err(err) => err,
            Ok(_) => panic!("expected clustered query bind to be missing"),
        };
        assert!(err
            .to_string()
            .contains("clustered query bind is not installed"));
    }

    fn config_with_sink(plugin: &str, body: serde_json::Value) -> Config {
        let mut sink = serde_json::Map::new();
        sink.insert(plugin.to_string(), body);
        serde_json::from_value(serde_json::json!({
            "skippr": { "workspace": "ws", "tenant": "t" },
            "pipelines": {
                "p": {
                    "data_source": "data_sources.sample",
                    "data_sink": "data_sinks.out"
                }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            },
            "data_sinks": {
                "out": sink
            }
        }))
        .unwrap()
    }

    fn lake_only_opts() -> ClusteredSelectOpts {
        ClusteredSelectOpts {
            identity: ClusterIdentity::new(
                skippr_lease::ClusterId::new("local").expect("static cluster id"),
                skippr_lease::NodeId::from_uuid(uuid::Uuid::nil()),
            ),
            scope: TenantScope::new("t", "ws").unwrap(),
            local_flight: "127.0.0.1:0".parse().unwrap(),
            registry: None,
            lake_only: true,
        }
    }

    #[tokio::test]
    async fn clustered_select_notices_wal_only_pipelines() {
        let cfg = config_with_sink("File", serde_json::json!({"path": "/tmp/out"}));
        let plan = plan_clustered_select(&cfg, "SELECT 1", &lake_only_opts())
            .await
            .unwrap();
        assert_eq!(plan.wal_only, vec!["p".to_string()]);
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
        assert!(!body.contains("get_json_opt"));
        assert!(!body.contains("ListingTable"));
        assert!(!body.contains("prefixes"));
    }
}
