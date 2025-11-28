use std::sync::Arc;
use url::Url;
use datafusion::prelude::SessionContext;
use datafusion::datasource::MemTable;
use datafusion::datasource::view::ViewTable;
use datafusion::error::DataFusionError;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::logical_expr::{Expr, col};
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl};
use tracing::{debug, info, warn};
use crate::helpers::configuration::Config;
use std::time::Duration;
use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use aws_credential_types::provider::ProvideCredentials;

/// Register an S3 object store for the given s3:// URL
pub async fn register_s3_object_store(ctx: &SessionContext, s3_loc: &str) {
    if let Ok(url) = Url::parse(s3_loc) {
        if url.scheme() != "s3" { return; }
        if let Some(bucket) = url.host_str() {
            // Avoid IMDS calls in environments without metadata service
            std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");
            // Preload credentials via AWS SDK (respects AWS_PROFILE/SSO/role) and export to env for object_store chain
            let conf = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
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
                        ctx.runtime_env().register_object_store(&endpoint, store_arc);
                        info!("Registered S3 object store for bucket='{}'", bucket);
                    } else {
                        warn!("register_s3_object_store: failed to parse url for bucket='{}'", bucket);
                    }
                }
                Err(e) => {
                    warn!("register_s3_object_store: failed to build store for bucket='{}': {}", bucket, e);
                }
            }
        }
    }
}

/// Find the longest common directory prefix among absolute S3 URLs
fn find_common_s3_prefix(paths: &[String]) -> Option<String> {
    if paths.is_empty() { return None; }
    if paths.len() == 1 { return Some(paths[0].trim_end_matches('/').to_string() + "/"); }
    // Parse and compare per-component (bucket + key segments)
    let mut split_paths: Vec<(String, Vec<String>)> = Vec::new();
    for p in paths {
        let u = Url::parse(p).ok()?;
        if u.scheme() != "s3" { return None; }
        let bucket = u.host_str()?.to_string();
        let key = u.path().trim_start_matches('/').trim_end_matches('/').to_string();
        let segs: Vec<String> = if key.is_empty() { vec![] } else { key.split('/').map(|s| s.to_string()).collect() };
        split_paths.push((bucket, segs));
    }
    // All buckets must match
    let bucket0 = &split_paths[0].0;
    if split_paths.iter().any(|(b, _)| b != bucket0) { return None; }
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
    let key_prefix = if common.is_empty() { String::new() } else { common.join("/") + "/" };
    Some(format!("s3://{}/{}", bucket0, key_prefix))
}

fn cast_or_keep_expr(name: &str, dt: &ArrowDataType) -> Expr {
    match dt {
        ArrowDataType::Timestamp(_, _) => {
            Expr::Cast(datafusion::logical_expr::expr::Cast {
                expr: Box::new(col(name)),
                data_type: ArrowDataType::Timestamp(datafusion::arrow::datatypes::TimeUnit::Millisecond, None),
            }).alias(name)
        }
        _ => col(name),
    }
}

fn build_timestamp_projection(df: &datafusion::prelude::DataFrame, _namespace: &str) -> datafusion::prelude::DataFrame {
    let arrow_schema = df.schema();
    if arrow_schema.fields().is_empty() { return df.clone(); }
    let mut exprs: Vec<Expr> = Vec::with_capacity(arrow_schema.fields().len());
    for f in arrow_schema.fields() {
        exprs.push(cast_or_keep_expr(f.name(), f.data_type()));
    }
    match df.clone().select(exprs) { Ok(dfp) => dfp, Err(_) => df.clone() }
}

async fn build_s3_df(ctx: &SessionContext, _namespace: &str, s3_paths: &[String]) -> Result<datafusion::prelude::DataFrame, DataFusionError> {
    let common = find_common_s3_prefix(s3_paths).unwrap_or_else(|| s3_paths[0].trim_end_matches('/').to_string() + "/");
    debug!("build_s3_df: {} s3 path(s), common prefix '{}'", s3_paths.len(), common);
    register_s3_object_store(ctx, &common).await;
    // Use ListingTable with ParquetFormat
    let url = ListingTableUrl::parse(&common).map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let fmt = ParquetFormat::default();
    let mut listing_opts = ListingOptions::new(Arc::new(fmt));
    listing_opts = listing_opts.with_file_extension(".parquet");
    let cfg = ListingTableConfig::new(url).with_listing_options(listing_opts);
    // Explicitly infer schema to avoid 'No schema provided' when reading tables with lazy schema
    let cfg = cfg
        .infer_schema(&ctx.state())
        .await
        .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let table = ListingTable::try_new(cfg).map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let df = ctx.read_table(Arc::new(table))?;
    debug!("build_s3_df: created DataFrame from listing at '{}'", common);
    Ok(df)
}

async fn build_wal_df(ctx: &SessionContext, pipeline: &str, namespace: &str) -> Result<Option<datafusion::prelude::DataFrame>, DataFusionError> {
    let reader = crate::buffer::wal_store::WalReaderFactory::for_pipeline_async(pipeline).await;
    let wal_batches: Vec<RecordBatch> = reader.load_committed_batches(namespace, 64).unwrap_or_default();
    if wal_batches.is_empty() { return Ok(None); }
    let schema = wal_batches[0].schema();
    let filtered: Vec<RecordBatch> = wal_batches.into_iter().filter(|b| b.schema().as_ref() == schema.as_ref()).collect();
    if filtered.is_empty() { return Ok(None); }
    let mem = MemTable::try_new(schema.clone(), vec![filtered]).map_err(|e| DataFusionError::Internal(e.to_string()))?;
    let tname = format!("{}_wal", namespace);
    ctx.register_table(&tname, Arc::new(mem))?;
    let df = ctx.table(&tname).await?;
    Ok(Some(df))
}

pub async fn register_namespace_view(ctx: &SessionContext, pipeline: &str, namespace: &str) -> Result<(), DataFusionError> {
	// If this table already exists in this context, skip work
	{
		use datafusion::catalog::CatalogProvider;
		let state = ctx.state();
		let cat_list = state.catalog_list();
		if let Some(catalog) = cat_list.catalog("datafusion") {
			if let Some(schema) = catalog.schema(pipeline) {
				// Async check for existing table in this context
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
    Config::init().await;
    info!("Registering namespace view for pipeline '{}', namespace '{}'", pipeline, namespace);

    // manifest -> prefixes (bounded to avoid stalls)
    let mut s3_paths: Vec<String> = Vec::new();
    // Build manifest key from explicit pipeline/namespace (no global pipeline)
    let man_opt = {
        let key = crate::sql::registry::manifest_key_for(pipeline, namespace);
        match tokio::time::timeout(Duration::from_secs(12), crate::helpers::s3::get_json(&key)).await {
            Ok(Ok(v)) => {
                info!("Reading manifest from s3://{}/{}", Config::get_skippr_s3_bucket(), key);
                info!("Manifest content: {}", v);
                Some(v)
            }
            Ok(Err(e)) => {
                warn!("register_namespace_view: failed to fetch manifest for '{}.{}': {:?}", pipeline, namespace, e);
                None
            }
            Err(_) => {
                warn!("register_namespace_view: timed out reading manifest for '{}.{}'", pipeline, namespace);
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
                                if !v.ends_with('/') { v.push('/'); }
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
        let bucket = Config::get_skippr_s3_bucket();
        let fallback = format!("s3://{}/datalake/{}/", bucket, namespace);
        info!("register_namespace_view: no manifest prefixes for '{}.{}'; falling back to {}", pipeline, namespace, fallback);
        s3_paths.push(fallback);
    }
    // S3 DF (single listing from common prefix) + timestamp projection
    info!("register_namespace_view: {} prefix(es) for '{}.{}'", s3_paths.len(), pipeline, namespace);
    let df_s3 = match tokio::time::timeout(Duration::from_secs(45), build_s3_df(ctx, namespace, &s3_paths)).await {
        Ok(Ok(df)) => df,
        Ok(Err(e)) => {
            warn!("register_namespace_view: failed to build S3 DF for '{}.{}': {}", pipeline, namespace, e);
            return Ok(());
        }
        Err(_) => {
            warn!("register_namespace_view: timed out building S3 DF for '{}.{}'", pipeline, namespace);
            return Ok(());
        }
    };
    let df_s3 = build_timestamp_projection(&df_s3, namespace);

    info!("Registered S3 namespace view for '{}.{}' with {} prefixes", pipeline, namespace, s3_paths.len());

    // WAL DF
    let df_wal_opt = build_wal_df(ctx, pipeline, namespace).await?;
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
                for f in left_schema.fields() { exprs.push(col(f.name())); }
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
    debug!("Registered WAL for '{}.{}' into namespace view", pipeline, namespace);
    let plan = df_union.into_optimized_plan()?;
    debug!("Created optimized plan for '{}.{}' namespace view", pipeline, namespace);
    let view = ViewTable::new(plan, Some(namespace.to_string()));
    // Register strictly under schema = pipeline
    {
        use datafusion::catalog::{CatalogProvider, SchemaProvider};
        use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
        // Access default catalog "datafusion"
        let state = ctx.state();
        let cat_list = state.catalog_list();
        if let Some(catalog) = cat_list.catalog("datafusion") {
            // Try to get or create schema for this pipeline
            if let Some(schema) = catalog.schema(pipeline) {
                schema.register_table(namespace.to_string(), Arc::new(view)).map_err(|e| DataFusionError::Execution(e.to_string()))?;
            } else {
                // Downcast to memory catalog and create schema
                if let Some(memcat) = catalog.as_any().downcast_ref::<MemoryCatalogProvider>() {
                    let new_schema = Arc::new(MemorySchemaProvider::new());
                    let _ = memcat.register_schema(pipeline, new_schema.clone());
                    new_schema.register_table(namespace.to_string(), Arc::new(view)).map_err(|e| DataFusionError::Execution(e.to_string()))?;
                } else {
                    return Err(DataFusionError::Plan(format!("Cannot register schema for pipeline '{}' in default catalog", pipeline)));
                }
            }
            info!("DF FQN: registered datafusion.{}.{}.", pipeline, namespace);
        } else {
            return Err(DataFusionError::Plan("Default catalog 'datafusion' not found".to_string()));
        }
    }
    debug!("Registered FQN view for '{}.{}'", pipeline, namespace);
    Ok(())
}

/// Register deadletters as a recursive Parquet listing with partition columns
pub async fn register_deadletters(ctx: &SessionContext, pipeline: &str) -> Result<(), DataFusionError> {
    // ensure config for bucket resolution
    Config::init().await;
    let bucket = Config::get_skippr_s3_bucket();
    let tenant = Config::get_tenant();
    let workspace = Config::get_workspace_name();
    let dl_url = format!("s3://{}/deadletters/{}/{}/{}/", bucket, tenant, workspace, pipeline);
    register_s3_object_store(ctx, &dl_url).await;
    // ListingTable for deadletters with partition columns
    let url = ListingTableUrl::parse(&dl_url).map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let fmt = ParquetFormat::default();
    let mut listing_opts = ListingOptions::new(Arc::new(fmt));
    listing_opts = listing_opts.with_file_extension(".parquet");
    let cfg = ListingTableConfig::new(url)
        .with_listing_options(listing_opts);
    let table = ListingTable::try_new(cfg).map_err(|e| DataFusionError::Execution(e.to_string()))?;
    ctx.register_table("deadletters", Arc::new(table))?;
    Ok(())
}

/// Ensure the logical schema 'dbt' exists under the default 'datafusion' catalog.
pub fn ensure_dbt_schema(ctx: &SessionContext) -> Result<(), DataFusionError> {
    use datafusion::catalog::{CatalogProvider, SchemaProvider};
    use datafusion::catalog::memory::{MemoryCatalogProvider, MemorySchemaProvider};
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
            Err(DataFusionError::Plan("Default catalog is not a memory catalog; cannot create 'dbt' schema".to_string()))
        }
    } else {
        Err(DataFusionError::Plan("Default catalog 'datafusion' not found".to_string()))
    }
}

/// Scan S3 for compiled dbt model SQL and register each as a view: dbt.<model>
pub async fn register_dbt_models(ctx: &SessionContext) -> Result<(), DataFusionError> {
    // Ensure config (tenant/workspace/bucket)
    Config::init().await;
    ensure_dbt_schema(ctx)?;
    let bucket = Config::get_skippr_s3_bucket();
    let tenant = Config::get_tenant();
    let workspace = Config::get_workspace_name();
    // List all pipelines
    let pipelines = crate::sql::registry::list_pipelines().await;
    let client = crate::helpers::s3::get_s3_client().await;
    use std::collections::HashSet;
    let mut seen_models: HashSet<String> = HashSet::new();
    for pipeline in pipelines {
        // compiled SQL can live in various target subdirs; scan target recursively
        let prefix = format!("{}/{}/{}/dbt/target/", tenant, workspace, pipeline);
        // list objects under this prefix
        let mut token: Option<String> = None;
        loop {
            let mut req = client.list_objects_v2()
                .bucket(&bucket)
                .prefix(&prefix)
                .max_keys(1000);
            if let Some(t) = token.as_ref() { req = req.continuation_token(t); }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    warn!("register_dbt_models: list_objects_v2 failed for prefix '{}' in bucket '{}': {:?}", prefix, bucket, e);
                    break;
                }
            };
            let contents = resp.contents();
            for obj in contents {
                if let Some(key) = obj.key() {
                    // interested only in compiled SQL files
                    if !key.ends_with(".sql") { continue; }
                    if !key.contains("/compiled/") { continue; }
                    // derive model name from filename
                    let model = key.rsplit('/').next().unwrap_or("").trim_end_matches(".sql");
                    if model.is_empty() { continue; }
                    if seen_models.contains(model) {
                        warn!("dbt model name collision: dbt.{} already registered; replacing with {}", model, key);
                    }
                    // fetch SQL
                    match crate::helpers::s3::get_bytes(key).await {
                        Ok(bytes) => {
                            let sql_text = String::from_utf8_lossy(&bytes).to_string();
                            if sql_text.trim().is_empty() { continue; }
                            // Register or replace view using DDL to avoid provider plumbing
                            let view_stmt = format!("CREATE OR REPLACE VIEW dbt.\"{}\" AS {}", model, sql_text);
                            match ctx.sql(&view_stmt).await {
                                Ok(df) => {
                                    // execute DDL
                                    let _ = df.collect().await;
                                    info!("Registered dbt view: dbt.{} (from {})", model, key);
                                    seen_models.insert(model.to_string());
                                }
                                Err(e) => {
                                    warn!("Failed to register dbt view for model '{}' from key '{}': {}", model, key, e);
                                }
                            }
                        }
                        Err(e) => {
                            warn!("register_dbt_models: failed to fetch compiled SQL '{}': {:?}", key, e);
                        }
                    }
                }
            }
            if resp.next_continuation_token().is_none() { break; }
            token = resp.next_continuation_token().map(|s| s.to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::find_common_s3_prefix;

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
        let v = vec![
            "s3://bkt/x/".to_string(),
            "s3://bkt/y/".to_string(),
        ];
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


