use crate::cluster::identity::{ClusterIdentity, TenantScope};
use crate::cluster::peer::ReplicaRegistry;
use crate::helpers::configuration::Config;
use aws_credential_types::provider::ProvideCredentials;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
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
use datafusion::logical_expr::{col, Expr};
use datafusion::physical_plan::limit::GlobalLimitExec;
use datafusion::physical_plan::union::UnionExec;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::SessionContext;
use object_store::aws::AmazonS3Builder;
use object_store::ObjectStore;
use std::any::Any;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
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

/// Find the longest common directory prefix among absolute S3 URLs
fn find_common_s3_prefix(paths: &[String]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    if paths.len() == 1 {
        return Some(paths[0].trim_end_matches('/').to_string() + "/");
    }
    // Parse and compare per-component (bucket + key segments)
    let mut split_paths: Vec<(String, Vec<String>)> = Vec::new();
    for p in paths {
        let u = Url::parse(p).ok()?;
        if u.scheme() != "s3" {
            return None;
        }
        let bucket = u.host_str()?.to_string();
        let key = u
            .path()
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string();
        let segs: Vec<String> = if key.is_empty() {
            vec![]
        } else {
            key.split('/').map(|s| s.to_string()).collect()
        };
        split_paths.push((bucket, segs));
    }
    // All buckets must match
    let bucket0 = &split_paths[0].0;
    if split_paths.iter().any(|(b, _)| b != bucket0) {
        return None;
    }
    // Find common key prefix
    let mut common: Vec<String> = Vec::new();
    let min_len = split_paths.iter().map(|(_, s)| s.len()).min().unwrap_or(0);
    for i in 0..min_len {
        let seg0 = &split_paths[0].1[i];
        if split_paths.iter().all(|(_, s)| &s[i] == seg0) {
            common.push(seg0.clone());
        } else {
            break;
        }
    }
    let key_prefix = if common.is_empty() {
        String::new()
    } else {
        common.join("/") + "/"
    };
    Some(format!("s3://{}/{}", bucket0, key_prefix))
}

fn cast_or_keep_expr(name: &str, dt: &ArrowDataType) -> Expr {
    match dt {
        ArrowDataType::Timestamp(_, _) => Expr::Cast(datafusion::logical_expr::expr::Cast {
            expr: Box::new(col(name)),
            data_type: ArrowDataType::Timestamp(
                datafusion::arrow::datatypes::TimeUnit::Millisecond,
                None,
            ),
        })
        .alias(name),
        _ => col(name),
    }
}

fn build_timestamp_projection(
    df: &datafusion::prelude::DataFrame,
    _namespace: &str,
) -> datafusion::prelude::DataFrame {
    let arrow_schema = df.schema();
    if arrow_schema.fields().is_empty() {
        return df.clone();
    }
    let mut exprs: Vec<Expr> = Vec::with_capacity(arrow_schema.fields().len());
    for f in arrow_schema.fields() {
        exprs.push(cast_or_keep_expr(f.name(), f.data_type()));
    }
    match df.clone().select(exprs) {
        Ok(dfp) => dfp,
        Err(_) => df.clone(),
    }
}

async fn build_s3_df(
    ctx: &SessionContext,
    _namespace: &str,
    s3_paths: &[String],
) -> Result<datafusion::prelude::DataFrame, DataFusionError> {
    let common = find_common_s3_prefix(s3_paths)
        .unwrap_or_else(|| s3_paths[0].trim_end_matches('/').to_string() + "/");
    debug!(
        "build_s3_df: {} s3 path(s), common prefix '{}'",
        s3_paths.len(),
        common
    );
    register_s3_object_store(ctx, &common).await;
    // Use ListingTable with ParquetFormat
    let url =
        ListingTableUrl::parse(&common).map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let fmt = ParquetFormat::default();
    let mut listing_opts = ListingOptions::new(Arc::new(fmt));
    listing_opts = listing_opts.with_file_extension(".parquet");
    let cfg = ListingTableConfig::new(url).with_listing_options(listing_opts);
    // Explicitly infer schema to avoid 'No schema provided' when reading tables with lazy schema
    let cfg = cfg
        .infer_schema(&ctx.state())
        .await
        .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let table =
        ListingTable::try_new(cfg).map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let df = ctx.read_table(Arc::new(table))?;
    debug!(
        "build_s3_df: created DataFrame from listing at '{}'",
        common
    );
    Ok(df)
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
    // Initialize config (tenant/workspace/bucket), but do NOT touch global pipeline state
    config.init().await;
    match crate::cluster::PipelineConfigView::for_name(config, pipeline) {
        Ok(view) if view.iceberg => {
            return register_iceberg_union_view(
                config,
                ctx,
                pipeline,
                namespace,
                &view,
                &namespace_union_opts(config),
            )
            .await;
        }
        Ok(_) => {}
        Err(err)
            if matches!(
                config.get_wal_storage(),
                crate::helpers::wal_storage::WalStorage::Clustered
            ) =>
        {
            return Err(DataFusionError::Plan(format!(
                "clustered Iceberg pipeline '{pipeline}' cannot fall back to Parquet listing: {err}"
            )));
        }
        Err(_) => {}
    }
    info!(
        "Registering namespace view for pipeline '{}', namespace '{}'",
        pipeline, namespace
    );

    // manifest -> prefixes (bounded to avoid stalls)
    let mut s3_paths: Vec<String> = Vec::new();
    // Build manifest key from explicit pipeline/namespace (no global pipeline)
    let man_opt = {
        let key = crate::sqlrt::registry::manifest_key_for(config, pipeline, namespace);
        let storage = crate::adapters::storage::get_storage(config);
        match tokio::time::timeout(Duration::from_secs(12), storage.get_json_opt(&key)).await {
            Ok(Ok(Some(v))) => {
                debug!("Reading manifest key='{}'", key);
                debug!("Manifest content: {}", v);
                Some(v)
            }
            Ok(Ok(None)) => {
                warn!(
                    "register_namespace_view: manifest not found for '{}.{}'",
                    pipeline, namespace
                );
                None
            }
            Ok(Err(e)) => {
                warn!(
                    "register_namespace_view: failed to fetch manifest for '{}.{}': {}",
                    pipeline, namespace, e
                );
                None
            }
            Err(_) => {
                warn!(
                    "register_namespace_view: timed out reading manifest for '{}.{}'",
                    pipeline, namespace
                );
                None
            }
        }
    };
    if let Some(man) = man_opt {
        if let Some(tables) = man.get("tables").and_then(|t| t.as_object()) {
            if let Some(ns) = tables.get(namespace).and_then(|v| v.as_object()) {
                if let Some(prefixes) = ns.get("prefixes").and_then(|p| p.as_array()) {
                    for p in prefixes {
                        if let Some(pref) = p.as_str() {
                            if pref.starts_with("s3://") {
                                let mut v = pref.trim().to_string();
                                if !v.ends_with('/') {
                                    v.push('/');
                                }
                                s3_paths.push(v);
                            }
                        }
                    }
                }
            }
        }
    }
    if s3_paths.is_empty() {
        // First-sync fallback: derive standard datalake prefix and proceed
        let bucket = config.get_skippr_s3_bucket();
        let fallback = format!("s3://{}/datalake/{}/", bucket, namespace);
        info!(
            "register_namespace_view: no manifest prefixes for '{}.{}'; falling back to {}",
            pipeline, namespace, fallback
        );
        s3_paths.push(fallback);
    }
    // S3 DF (single listing from common prefix) + timestamp projection
    info!(
        "register_namespace_view: {} prefix(es) for '{}.{}'",
        s3_paths.len(),
        pipeline,
        namespace
    );
    let df_s3 = match tokio::time::timeout(
        Duration::from_secs(45),
        build_s3_df(ctx, namespace, &s3_paths),
    )
    .await
    {
        Ok(Ok(df)) => df,
        Ok(Err(e)) => {
            warn!(
                "register_namespace_view: failed to build S3 DF for '{}.{}': {}",
                pipeline, namespace, e
            );
            return Ok(());
        }
        Err(_) => {
            warn!(
                "register_namespace_view: timed out building S3 DF for '{}.{}'",
                pipeline, namespace
            );
            return Ok(());
        }
    };
    let df_s3 = build_timestamp_projection(&df_s3, namespace);

    info!(
        "Registered S3 namespace view for '{}.{}' with {} prefixes",
        pipeline,
        namespace,
        s3_paths.len()
    );

    // WAL DF
    let df_wal_opt = build_wal_df(config, ctx, pipeline, namespace).await?;
    let df_union = match df_wal_opt {
        None => df_s3.clone(),
        Some(df_wal) => {
            let left_schema = df_s3.schema();
            let right_schema = df_wal.schema();
            if left_schema.fields().len() == right_schema.fields().len() {
                df_s3.union(df_wal)?
            } else {
                // Project WAL to S3 columns by name if possible
                let mut exprs: Vec<datafusion::logical_expr::Expr> = Vec::new();
                for f in left_schema.fields() {
                    exprs.push(col(f.name()));
                }
                if exprs.is_empty() {
                    df_s3.union(df_wal)?
                } else {
                    match df_wal.clone().select(exprs) {
                        Ok(projected) => df_s3.union(projected)?,
                        Err(_) => df_s3.union(df_wal)?,
                    }
                }
            }
        }
    };

    // Register view under DataFusion catalog.schema.table → datafusion.<pipeline>.<namespace>
    debug!(
        "Registered WAL for '{}.{}' into namespace view",
        pipeline, namespace
    );
    let plan = df_union.into_optimized_plan()?;
    debug!(
        "Created optimized plan for '{}.{}' namespace view",
        pipeline, namespace
    );
    let view = ViewTable::new(plan, Some(namespace.to_string()));
    // Register strictly under schema = pipeline
    {
        use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
        use datafusion::catalog::{CatalogProvider, SchemaProvider};
        // Access default catalog "datafusion"
        let state = ctx.state();
        let cat_list = state.catalog_list();
        if let Some(catalog) = cat_list.catalog("datafusion") {
            // Try to get or create schema for this pipeline
            if let Some(schema) = catalog.schema(pipeline) {
                schema
                    .register_table(namespace.to_string(), Arc::new(view))
                    .map_err(|e| DataFusionError::Execution(e.to_string()))?;
            } else {
                // Downcast to memory catalog and create schema
                if let Some(memcat) = catalog.as_any().downcast_ref::<MemoryCatalogProvider>() {
                    let new_schema = Arc::new(MemorySchemaProvider::new());
                    let _ = memcat.register_schema(pipeline, new_schema.clone());
                    new_schema
                        .register_table(namespace.to_string(), Arc::new(view))
                        .map_err(|e| DataFusionError::Execution(e.to_string()))?;
                } else {
                    return Err(DataFusionError::Plan(format!(
                        "Cannot register schema for pipeline '{}' in default catalog",
                        pipeline
                    )));
                }
            }
            info!("DF FQN: registered datafusion.{}.{}.", pipeline, namespace);
        } else {
            return Err(DataFusionError::Plan(
                "Default catalog 'datafusion' not found".to_string(),
            ));
        }
    }
    debug!("Registered FQN view for '{}.{}'", pipeline, namespace);
    Ok(())
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

pub struct ClusteredSelectOpts {
    pub identity: ClusterIdentity,
    pub scope: TenantScope,
    pub local_flight: SocketAddr,
    pub registry: Option<Arc<ReplicaRegistry>>,
    pub iceberg_only: bool,
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
        iceberg_only: false,
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
            iceberg_only: false,
        }
    })
}

pub async fn plan_clustered_select(
    config: &Config,
    sql: &str,
    opts: &ClusteredSelectOpts,
) -> Result<datafusion::dataframe::DataFrame, DataFusionError> {
    config.init().await;
    let cfg = config.clone();
    let ctx = if opts.iceberg_only {
        SessionContext::new()
    } else {
        crate::query_flight::ballista::query_context()?
    };
    for name in cfg.pipelines.keys() {
        let view = crate::cluster::PipelineConfigView::for_name(&cfg, name)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
        if !view.iceberg {
            continue;
        }
        if !opts.scope.matches_pipeline(&view.key) {
            continue;
        }
        let namespaces = match list_iceberg_source_namespaces(config, &view).await {
            Ok(namespaces) => namespaces,
            Err(err) => {
                warn!(
                    pipeline = %name,
                    error = %err,
                    "skipping Iceberg pipeline with no catalog tables"
                );
                continue;
            }
        };
        if namespaces.is_empty() {
            continue;
        }
        for namespace in namespaces {
            let _ = ctx.deregister_table(&namespace);
            register_iceberg_union_view(config, &ctx, name, &namespace, &view, opts).await?;
        }
    }
    ctx.sql(sql).await
}

pub async fn execute_clustered_select(
    config: &Config,
    sql: &str,
    opts: &ClusteredSelectOpts,
) -> Result<Vec<RecordBatch>, DataFusionError> {
    plan_clustered_select(config, sql, opts)
        .await?
        .collect()
        .await
}

pub async fn schema_for_clustered_select(
    config: &Config,
    sql: &str,
    opts: &ClusteredSelectOpts,
) -> Result<SchemaRef, DataFusionError> {
    let df = plan_clustered_select(config, sql, opts).await?;
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
        let view = crate::cluster::PipelineConfigView::for_name(&cfg, name)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
        if !view.iceberg {
            continue;
        }
        if !scope.matches_pipeline(&view.key) {
            continue;
        }
        match load_iceberg_scan_provider(config, &view, namespace).await {
            Ok(loaded) => {
                let _ = ctx.deregister_table(namespace);
                ctx.register_table(namespace, loaded.provider)
                    .map_err(|err| DataFusionError::Execution(err.to_string()))?;
                found = true;
                break;
            }
            Err(_) => continue,
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
    for name in cfg.pipelines.keys() {
        let view = crate::cluster::PipelineConfigView::for_name(&cfg, name)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
        if !view.iceberg {
            continue;
        }
        if let Ok(loaded) = load_iceberg_scan_provider(config, &view, namespace).await {
            return Ok(datafusion::datasource::TableProvider::schema(
                loaded.provider.as_ref(),
            ));
        }
    }
    Err(DataFusionError::Plan(format!(
        "no Iceberg schema for namespace '{namespace}'"
    )))
}

pub async fn list_configured_iceberg_tables(
    config: &Config,
    scope: &TenantScope,
) -> Vec<(String, SchemaRef)> {
    config.init().await;
    let cfg = config.clone();
    let mut out = Vec::new();
    for name in cfg.pipelines.keys() {
        let Ok(view) = crate::cluster::PipelineConfigView::for_name(&cfg, name) else {
            continue;
        };
        if !view.iceberg || !scope.matches_pipeline(&view.key) {
            continue;
        }
        let Ok(namespaces) = list_iceberg_source_namespaces(config, &view).await else {
            continue;
        };
        for namespace in namespaces {
            let schema = iceberg_schema_for_namespace(config, &namespace)
                .await
                .unwrap_or_else(|_| Arc::new(datafusion::arrow::datatypes::Schema::empty()));
            out.push((namespace, schema));
        }
    }
    out
}

#[allow(dead_code)]
struct IcebergSinkCatalog {
    catalog_cfg: skippr_iceberg_catalog::IcebergCatalogConfig,
    catalog_ns: String,
    table_prefix: Option<String>,
}

fn iceberg_sink_catalog(
    config: &Config,
    view: &crate::cluster::PipelineConfigView,
) -> Result<IcebergSinkCatalog, DataFusionError> {
    let cfg = config.clone();
    let sink_ref = view.sink_ref.as_ref().ok_or_else(|| {
        DataFusionError::Plan(format!(
            "Iceberg pipeline '{}' has no data_sink",
            view.key.pipeline()
        ))
    })?;
    let sink_name =
        Config::parse_registry_ref(sink_ref, "data_sinks").map_err(DataFusionError::Plan)?;
    let entry = cfg
        .data_sinks
        .as_ref()
        .and_then(|sinks| sinks.get(&sink_name))
        .ok_or_else(|| DataFusionError::Plan(format!("data_sinks.{sink_name} is not defined")))?;
    let catalog_val = entry.config.config.get("catalog").cloned().ok_or_else(|| {
        DataFusionError::Plan("Iceberg sink is missing catalog configuration".into())
    })?;
    let catalog_cfg: skippr_iceberg_catalog::IcebergCatalogConfig =
        serde_json::from_value(catalog_val)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
    let catalog_ns = entry
        .config
        .config
        .get("table_namespace")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let table_prefix = entry
        .config
        .config
        .get("table_prefix")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(IcebergSinkCatalog {
        catalog_cfg,
        catalog_ns,
        table_prefix,
    })
}

#[cfg_attr(
    not(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    )),
    allow(dead_code)
)]
pub(crate) fn catalog_table_to_namespace(name: &str, prefix: Option<&str>) -> Option<String> {
    match prefix {
        Some(prefix) => name.strip_prefix(&format!("{prefix}_")).map(str::to_string),
        None => Some(name.to_string()),
    }
}

async fn list_iceberg_source_namespaces(
    config: &Config,
    view: &crate::cluster::PipelineConfigView,
) -> Result<Vec<String>, DataFusionError> {
    let sink = iceberg_sink_catalog(config, view)?;
    #[cfg(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    ))]
    {
        match &sink.catalog_cfg {
            skippr_iceberg_catalog::IcebergCatalogConfig::Skippr { .. } => {
                let catalog = crate::cluster::backend::open_skippr_catalog(&sink.catalog_cfg)
                    .await
                    .map_err(|err| DataFusionError::Plan(err))?;
                let ns = iceberg::NamespaceIdent::from_strs([&sink.catalog_ns])
                    .map_err(|err| DataFusionError::External(Box::new(err)))?;
                let tables = iceberg::Catalog::list_tables(catalog.as_ref(), &ns)
                    .await
                    .map_err(|err| DataFusionError::External(Box::new(err)))?;
                let prefix = sink.table_prefix.as_deref();
                Ok(tables
                    .into_iter()
                    .filter_map(|ident| catalog_table_to_namespace(ident.name(), prefix))
                    .collect())
            }
            other => Err(DataFusionError::Plan(format!(
                "Iceberg pipeline '{}' cannot list catalog tables via adapter '{}'",
                view.key.pipeline(),
                other.adapter_name()
            ))),
        }
    }
    #[cfg(not(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    )))]
    {
        let _ = sink;
        Err(DataFusionError::Plan(format!(
            "Iceberg pipeline '{}' requires offset-store-dynamodb or offset-store-cloud-tables to list catalog tables",
            view.key.pipeline()
        )))
    }
}

struct LoadedIcebergScan {
    provider: Arc<dyn TableProvider>,
    compacted_segment_ids: Vec<String>,
}

async fn register_iceberg_union_view(
    config: &Config,
    ctx: &SessionContext,
    _pipeline: &str,
    namespace: &str,
    view: &crate::cluster::PipelineConfigView,
    opts: &ClusteredSelectOpts,
) -> Result<(), DataFusionError> {
    let (loaded, skip_wal) = match load_iceberg_scan_provider(config, view, namespace).await {
        Ok(loaded) => (loaded, opts.iceberg_only),
        Err(err) if !opts.iceberg_only => match load_iceberg_from_peer(namespace, opts).await {
            Ok(loaded) => (loaded, opts.iceberg_only),
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
    let _ = ctx.deregister_table(namespace);
    ctx.register_table(namespace, table)
        .map_err(|err| DataFusionError::Execution(err.to_string()))?;
    Ok(())
}

async fn wal_child_provider(
    view: &crate::cluster::PipelineConfigView,
    namespace: &str,
    schema: SchemaRef,
    exclude_segment_ids: &[String],
    opts: &ClusteredSelectOpts,
) -> Result<Option<Arc<dyn TableProvider>>, DataFusionError> {
    let ads = match crate::cluster::gossip::gossip_directory() {
        Some(gossip) => gossip.known_ads().await,
        None => Vec::new(),
    };
    let paths =
        crate::cluster::wal_head::local_wal_paths(&view.key, opts.registry.as_deref()).await;
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
    config: &Config,
    view: &crate::cluster::PipelineConfigView,
    namespace: &str,
) -> Result<LoadedIcebergScan, DataFusionError> {
    let sink = iceberg_sink_catalog(config, view)?;
    let table_name = match sink.table_prefix.as_deref() {
        Some(prefix) => format!("{prefix}_{namespace}"),
        None => namespace.to_string(),
    };
    let ident = iceberg::TableIdent::from_strs([sink.catalog_ns.as_str(), table_name.as_str()])
        .map_err(|err| DataFusionError::External(Box::new(err)))?;
    #[cfg(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    ))]
    {
        match &sink.catalog_cfg {
            skippr_iceberg_catalog::IcebergCatalogConfig::Skippr { .. } => {
                let catalog = crate::cluster::backend::open_skippr_catalog(&sink.catalog_cfg)
                    .await
                    .map_err(|err| DataFusionError::Plan(err))?;
                let (table, snapshot_id) =
                    crate::sqlrt::iceberg_table::load_pinned_iceberg_table(catalog, &ident).await?;
                let compacted_segment_ids =
                    crate::sqlrt::iceberg_table::compacted_wal_segment_ids(&table);
                let catalog_json = serde_json::to_string(&sink.catalog_cfg)
                    .map_err(|err| DataFusionError::Plan(err.to_string()))?;
                let provider =
                    crate::sqlrt::iceberg_table::IcebergScanTableProvider::new_with_catalog(
                        table,
                        snapshot_id,
                        catalog_json,
                        sink.catalog_ns.clone(),
                        table_name,
                    )?;
                return Ok(LoadedIcebergScan {
                    provider: Arc::new(provider),
                    compacted_segment_ids,
                });
            }
            other => {
                return Err(DataFusionError::Plan(format!(
                    "Iceberg pipeline '{}' cannot use Parquet listing; catalog adapter '{}' is not available for namespace '{namespace}' UNION live WAL",
                    view.key.pipeline(),
                    other.adapter_name()
                )));
            }
        }
    }
    #[cfg(not(any(
        feature = "offset-store-dynamodb",
        feature = "offset-store-cloud-tables"
    )))]
    {
        let _ = ident;
        Err(DataFusionError::Plan(format!(
            "Iceberg pipeline '{}' cannot use Parquet listing; Iceberg catalog scan is required for namespace '{namespace}' UNION live WAL",
            view.key.pipeline()
        )))
    }
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
    fn catalog_table_to_namespace_keeps_only_matching_prefix() {
        assert_eq!(
            catalog_table_to_namespace("hla_hla_events", Some("hla")).as_deref(),
            Some("hla_events")
        );
        assert_eq!(
            catalog_table_to_namespace("hla-b_hla_events_b", Some("hla-b")).as_deref(),
            Some("hla_events_b")
        );
        assert_eq!(
            catalog_table_to_namespace("hla_hla_events", Some("hla-b")),
            None
        );
        assert_eq!(
            catalog_table_to_namespace("hla-b_hla_events_b", Some("hla")),
            None
        );
        assert_eq!(
            catalog_table_to_namespace("hla_events", None).as_deref(),
            Some("hla_events")
        );
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
    fn iceberg_only_is_allowed_without_local_wal() {
        let src = include_str!("tables.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(prod.contains("iceberg_only: bool"));
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

    #[test]
    fn test_common_prefix_same_dir() {
        let v = vec![
            "s3://bkt/a/b/c/".to_string(),
            "s3://bkt/a/b/c/d/".to_string(),
            "s3://bkt/a/b/c/e/".to_string(),
        ];
        let pref = find_common_s3_prefix(&v).unwrap();
        assert_eq!(pref, "s3://bkt/a/b/c/");
    }

    #[test]
    fn test_common_prefix_bucket_root() {
        let v = vec!["s3://bkt/x/".to_string(), "s3://bkt/y/".to_string()];
        let pref = find_common_s3_prefix(&v).unwrap();
        assert_eq!(pref, "s3://bkt/");
    }

    #[test]
    fn test_common_prefix_single() {
        let v = vec!["s3://bkt/a/".to_string()];
        let pref = find_common_s3_prefix(&v).unwrap();
        assert_eq!(pref, "s3://bkt/a/");
    }
}
