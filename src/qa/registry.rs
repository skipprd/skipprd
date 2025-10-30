use std::sync::Arc;
use url::Url;
use arrow::record_batch::RecordBatch;
use datafusion::datasource::MemTable;
use datafusion::datasource::view::ViewTable;
use datafusion::error::DataFusionError;
use datafusion::prelude::{ParquetReadOptions, SessionContext};
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::logical_expr::{Expr, col};

use crate::helpers::configuration::Config;
use crate::sql::query::register_s3_object_store;
use crate::{ARROW_SCHEMA, METADATA};
use crate::sql::registry::{find_entry};

/// Register a logical view for a namespace by unifying S3 parquet and WAL batches.
pub async fn register_namespace_view(ctx: &SessionContext, ns: &str) -> Result<(), String> {
    println!("{} ASK: registering namespace view for '{}'", chrono::Utc::now().to_rfc3339(), ns);
    // Ensure metadata and Arrow schema are prepared
    match Config::get_metadata().await {
        Ok(pm) => {
            METADATA.store(Arc::new(pm.clone()));
            let flatten = Config::get_transform_flatten_events();
            if pm.metadata.contains_key(ns) {
                let _ = crate::ingest_work::Ingest::prepare_arrow_schema_with_metadata(ns, &pm.metadata, flatten);
            }
        },
        Err(_) => {}
    }

    // WAL side: load any in-flight segments for this namespace
    let seg_dir = format!("{}/segment_buffer/segs", Config::get_data_dir());
    let mut wal_batches: Vec<RecordBatch> = Vec::new();
    if std::path::Path::new(&seg_dir).exists() {
        for entry in std::fs::read_dir(&seg_dir).unwrap_or_else(|_| std::fs::read_dir("/").unwrap()) {
            if let Ok(ent) = entry {
                let path = ent.path();
                if path.extension().and_then(|s| s.to_str()) != Some("seg") { continue; }
                let seg = crate::buffer::segment_file::SegmentFile { path: path.clone() };
                if let Ok(meta) = seg.read_metadata() {
                    for idx in meta.index.iter() {
                        if idx.key.0 != ns { continue; }
                        if let Ok(mut file) = std::fs::OpenOptions::new().read(true).open(&path) {
                            use std::io::{Seek, Read};
                            if file.seek(std::io::SeekFrom::Start(idx.start)).is_ok() {
                                let mut reader = std::io::BufReader::new(file);
                                let mut take = reader.take(idx.len);
                                if let Ok(sr) = arrow::ipc::reader::StreamReader::try_new(&mut take, None) {
                                    for it in sr { if let Ok(b) = it { wal_batches.push(b); } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if !wal_batches.is_empty() {
        let schema = wal_batches[0].schema();
        let filtered: Vec<RecordBatch> = wal_batches.into_iter().filter(|b| b.schema().as_ref() == schema.as_ref()).collect();
        if !filtered.is_empty() {
            let mem = MemTable::try_new(schema.clone(), vec![filtered]).map_err(|e| DataFusionError::Internal(e.to_string())).unwrap();
            let _ = ctx.register_table(&format!("{}_wal", ns), Arc::new(mem));
        }
    }

    // S3 side: use registry data_prefixes for this namespace
    let mut df_base_opt: Option<datafusion::prelude::DataFrame> = None;
    let pipeline = Config::get_pipeline_name();
    if let Some(entry) = find_entry(&pipeline, ns).await {
        // gather and normalize prefixes
        let mut s3_paths: Vec<String> = entry.data_prefixes.clone();
        // fallback: use configured output parquet base if registry has no prefixes
        if s3_paths.is_empty() {
            if let Some(loc) = Config::get_output_parquet_s3_location(ns) { s3_paths.push(loc); }
        }
        // normalize relative-like prefixes to full s3 URLs using configured output bucket (if any)
        if let Some(s3_loc) = Config::get_output_parquet_s3_location(ns) {
            if let Ok(u) = Url::parse(&s3_loc) {
                if let Some(bucket) = u.host_str() {
                    for p in s3_paths.iter_mut() {
                        if !p.starts_with("s3://") {
                            let mut dir = p.trim_matches('/').to_string(); if !dir.ends_with('/') { dir.push('/'); }
                            *p = format!("s3://{}/{}", bucket, dir);
                        }
                    }
                }
            }
        }
        // dedupe after normalization and register object stores
        s3_paths.sort(); s3_paths.dedup();
        // For responsiveness in Ask, cap to first prefix for now
        if s3_paths.len() > 1 { s3_paths.truncate(1); }
        for p in &s3_paths { register_s3_object_store(ctx, p).await; }
        let mut sources: Vec<String> = Vec::new();
        for (idx, path) in s3_paths.iter().enumerate() {
            let tname = format!("{}_s3_{}", ns, idx);
            // Guard registration to avoid long stalls when listing huge prefixes
            let reg = tokio::time::timeout(std::time::Duration::from_secs(5), ctx.register_parquet(&tname, path, ParquetReadOptions::default())).await;
            if reg.is_ok() { sources.push(tname); }
        }
        if !sources.is_empty() {
            // Table resolution can also stall; guard with a timeout
            if let Ok(Ok(mut df_s3)) = tokio::time::timeout(std::time::Duration::from_secs(5), ctx.table(&sources[0])).await {
                for t in sources.iter().skip(1) {
                    if let Ok(Ok(df_next)) = tokio::time::timeout(std::time::Duration::from_secs(3), ctx.table(t)).await {
                        match df_s3.clone().union(df_next) {
                            Ok(un) => df_s3 = un,
                            Err(_) => {}
                        }
                    }
                }
                df_base_opt = Some(df_s3);
            }
        }
    }

    // Project timestamps based on ARROW_SCHEMA to a consistent unit
    let df_base_opt = if let Some(mut df_s3) = df_base_opt {
        if let Some(swap) = ARROW_SCHEMA.get(ns) { let arrow_schema = swap.load(); if arrow_schema.fields().is_empty() { Some(df_s3) } else {
            fn build_expr_for_field(name: &str, dt: &ArrowDataType) -> Expr { match dt { ArrowDataType::Timestamp(_, _) => Expr::Cast(datafusion::logical_expr::expr::Cast { expr: Box::new(col(name)), data_type: ArrowDataType::Timestamp(datafusion::arrow::datatypes::TimeUnit::Millisecond, None) }).alias(name), _ => col(name) } }
            let exprs: Vec<Expr> = arrow_schema.fields().iter().map(|f| build_expr_for_field(f.name(), f.data_type())).collect();
            let base = df_s3.clone();
            match base.clone().select(exprs) { Ok(sel) => Some(sel), Err(_) => Some(base) }
        } } else { Some(df_s3) }
    } else { None };

    // Decide final DF: S3 if available, else WAL-only if present
    let df_final = if let Some(df_s3) = df_base_opt {
        // WAL fallback empty DF of same schema and union
        let df_wal = match ctx.table(&format!("{}_wal", ns)).await { Ok(df) => df, Err(_) => df_s3.clone().filter(datafusion::logical_expr::lit(false)).unwrap() };
        df_s3.union(df_wal).map_err(|e| e.to_string())?
    } else {
        // No S3 configured; attempt WAL-only registration
        match ctx.table(&format!("{}_wal", ns)).await {
            Ok(df) => df,
            Err(_) => return Err("No data sources (S3 or WAL) available".to_string()),
        }
    };
    let view = ViewTable::try_new(df_final.into_optimized_plan().map_err(|e| e.to_string())?, Some(ns.to_string())).map_err(|e| e.to_string())?;
    ctx.register_table(ns, Arc::new(view)).map_err(|e| e.to_string())?;
    println!("{} ASK: namespace '{}' registered", chrono::Utc::now().to_rfc3339(), ns);
    Ok(())
}


