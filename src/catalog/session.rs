use std::sync::Arc;
use datafusion::prelude::{SessionConfig, SessionContext};
use arrow::record_batch::RecordBatch;

pub struct SessionFactory;

impl SessionFactory {
    pub async fn new_context() -> SessionContext {
        let mut session_config = SessionConfig::new();
        session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
        session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());
        SessionContext::new_with_config(session_config)
    }

    pub async fn register_s3_and_wal(ctx: &SessionContext, namespace: &str) {
        // Register semantic/catalog tables
        crate::sql::query::register_semantic_and_catalog(ctx).await;
        // Register WAL memtable for namespace (best-effort)
        let seg_dir = format!("{}/segment_buffer/segs", crate::helpers::configuration::Config::get_data_dir());
        let mut wal_batches: Vec<RecordBatch> = Vec::new();
        if std::path::Path::new(&seg_dir).exists() {
            for entry in std::fs::read_dir(&seg_dir).unwrap_or_else(|_| std::fs::read_dir("/").unwrap()) {
                if let Ok(ent) = entry { let path = ent.path(); if path.extension().and_then(|s| s.to_str()) != Some("seg") { continue; }
                    let seg = crate::buffer::segment_file::SegmentFile { path: path.clone() };
                    if let Ok(meta) = seg.read_metadata() {
                        for idx in meta.index.iter() {
                            if idx.key.0 != namespace { continue; }
                            if let Ok(mut file) = std::fs::OpenOptions::new().read(true).open(&path) {
                                use std::io::{Seek, Read};
                                if file.seek(std::io::SeekFrom::Start(idx.start)).is_ok() {
                                    let mut reader = std::io::BufReader::new(file);
                                    let mut take = reader.take(idx.len);
                                    if let Ok(sr) = arrow::ipc::reader::StreamReader::try_new(&mut take, None) { for it in sr { if let Ok(b) = it { wal_batches.push(b); } } }
                                }
                            }
                        }
                    }
                }
            }
        }
        if !wal_batches.is_empty() { let schema = wal_batches[0].schema(); let filtered: Vec<RecordBatch> = wal_batches.into_iter().filter(|b| b.schema().as_ref() == schema.as_ref()).collect(); if !filtered.is_empty() { let mem = datafusion::datasource::MemTable::try_new(schema.clone(), vec![filtered]).map_err(|e| datafusion::error::DataFusionError::Internal(e.to_string())).unwrap(); let _ = ctx.register_table(&format!("{}_wal", &namespace), Arc::new(mem)); } }

        // Register S3 object store for namespace
        if let Some(s3_loc) = crate::helpers::configuration::Config::get_output_parquet_s3_location(namespace) {
            crate::sql::query::register_s3_object_store(ctx, &s3_loc).await;
        }
    }
}


