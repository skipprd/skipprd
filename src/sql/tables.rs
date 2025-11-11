use std::sync::Arc;
use url::Url;
use tracing::{debug, info, warn};
use datafusion::prelude::{SessionContext, ParquetReadOptions};
use datafusion::datasource::MemTable;
use datafusion::datasource::view::ViewTable;
use datafusion::error::DataFusionError;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::logical_expr::{Expr, col};
use object_store::aws::AmazonS3Builder;
use aws_credential_types::provider::ProvideCredentials;

use crate::helpers::configuration::Config;
use crate::ARROW_SCHEMA;

/// Register an S3 object store for the given s3:// URL
pub async fn register_s3_object_store(ctx: &SessionContext, s3_loc: &str) {
    if let Ok(u) = Url::parse(s3_loc) {
        if u.scheme() != "s3" { return; }
        if let Some(bucket) = u.host_str() {
            // Use AWS SDK defaults chain
            let conf = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
            let region_opt = conf.region().map(|r| r.as_ref().to_string());
            let creds_opt = if let Some(p) = conf.credentials_provider() {
                match p.provide_credentials().await { Ok(c) => Some(c), Err(_) => None }
            } else { None };
            let mut b = AmazonS3Builder::new().with_bucket_name(bucket);
            if let Some(region) = region_opt { b = b.with_region(region); }
            if let Some(c) = creds_opt {
                b = b.with_access_key_id(c.access_key_id().to_string())
                     .with_secret_access_key(c.secret_access_key().to_string());
                if let Some(t) = c.session_token() { b = b.with_token(t.to_string()); }
            }
            if let Ok(store) = b.build() {
                let base = Url::parse(&format!("s3://{}/", bucket)).unwrap_or(u.clone());
                let _ = ctx.runtime_env().register_object_store(&base, Arc::new(store));
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

fn build_timestamp_projection(df: &datafusion::prelude::DataFrame, namespace: &str) -> datafusion::prelude::DataFrame {
    if let Some(swap) = ARROW_SCHEMA.get(namespace) {
        let arrow_schema = swap.load();
        if arrow_schema.fields().is_empty() { return df.clone(); }
        let mut exprs: Vec<Expr> = Vec::with_capacity(arrow_schema.fields().len());
        for f in arrow_schema.fields() {
            exprs.push(cast_or_keep_expr(f.name(), f.data_type()));
        }
        match df.clone().select(exprs) { Ok(dfp) => dfp, Err(_) => df.clone() }
    } else {
        df.clone()
    }
}

async fn build_s3_df(ctx: &SessionContext, namespace: &str, s3_paths: &[String]) -> Result<datafusion::prelude::DataFrame, DataFusionError> {
    let common = find_common_s3_prefix(s3_paths).unwrap_or_else(|| s3_paths[0].trim_end_matches('/').to_string() + "/");
    register_s3_object_store(ctx, &common).await;
    let opts = ParquetReadOptions {
        schema: None,
        file_extension: "parquet",
        table_partition_cols: vec![],
        parquet_pruning: None,
        skip_metadata: Some(true),
        file_sort_order: vec![],
    };
    let tname = format!("{}_s3", namespace);
    ctx.register_parquet(&tname, &common, opts).await?;
    let df = ctx.table(&tname).await?;
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
    // ensure pipeline context
    crate::helpers::configuration::PIPELINE_NAME.write().clear();
    crate::helpers::configuration::PIPELINE_NAME.write().push_str(pipeline);
    Config::init().await;

    // manifest -> prefixes
    let mut s3_paths: Vec<String> = Vec::new();
    if let Some(man) = Config::read_manifest(namespace).await {
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
        return Err(DataFusionError::Plan(format!("Manifest missing or contains no absolute S3 prefixes for '{}.{}'", pipeline, namespace)));
    }
    // S3 DF (single listing from common prefix) + timestamp projection
    let df_s3 = build_s3_df(ctx, namespace, &s3_paths).await?;
    let df_s3 = build_timestamp_projection(&df_s3, namespace);

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

    // Register view as namespace
    let view = ViewTable::try_new(df_union.into_optimized_plan()?, Some(namespace.to_string()))?;
    ctx.register_table(namespace, Arc::new(view))?;
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
    let opts = ParquetReadOptions {
        schema: None,
        file_extension: "parquet",
        table_partition_cols: vec![
            ("namespace".to_string(), ArrowDataType::Utf8),
            ("p_year".to_string(), ArrowDataType::Utf8),
            ("p_month".to_string(), ArrowDataType::Utf8),
            ("p_day".to_string(), ArrowDataType::Utf8),
        ],
        parquet_pruning: None,
        skip_metadata: Some(true),
        file_sort_order: vec![],
    };
    ctx.register_parquet("deadletters", &dl_url, opts).await?;
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


