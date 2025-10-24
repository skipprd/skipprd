use datafusion::prelude::SessionContext;
use crate::llm::{self, ChatMessage};
use crate::helpers::configuration::Config;
use crate::sql::query::{register_semantic_and_catalog, register_s3_object_store};
use datafusion::datasource::MemTable;
use datafusion::error::DataFusionError;
use datafusion::prelude::ParquetReadOptions;
use datafusion::datasource::view::ViewTable;
use arrow::record_batch::RecordBatch;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::logical_expr::Expr;
use datafusion::logical_expr::col;
use crate::{ARROW_SCHEMA, METADATA};
use arc_swap::ArcSwap;
use url::Url;
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct AskOpts {
    pub namespace: Option<String>,
    pub top_k: usize,
    pub use_docs: bool,
    pub use_sql: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Answer {
    pub text: String,
    pub followup: Option<String>,
}

pub async fn ask(question: &str, opts: &AskOpts) -> Result<Answer, String> {
    let ns = opts.namespace.clone().unwrap_or_else(|| Config::get_pipeline_name());

    // Prepare DataFusion context
    let ctx = SessionContext::new();
    // Establish pipeline context and metadata/arrow schema as in query path
    super::super::helpers::configuration::PIPELINE_NAME.write().clear();
    super::super::helpers::configuration::PIPELINE_NAME.write().push_str(&ns);
    Config::init().await;
    match Config::get_metadata().await { Ok(pm) => { METADATA.store(Arc::new(pm.clone())); let flatten = Config::get_transform_flatten_events(); if pm.metadata.contains_key(&ns) { let _ = super::super::ingest_work::Ingest::prepare_arrow_schema_with_metadata(&ns, &pm.metadata, flatten); } }, Err(_) => {} }

    // Register S3 + WAL union view for the namespace, same as query path
    // WAL side
    let seg_dir = format!("{}/segment_buffer/segs", Config::get_data_dir());
    let mut wal_batches: Vec<RecordBatch> = Vec::new();
    if std::path::Path::new(&seg_dir).exists() {
        for entry in std::fs::read_dir(&seg_dir).unwrap_or_else(|_| std::fs::read_dir("/").unwrap()) {
            if let Ok(ent) = entry { let path = ent.path(); if path.extension().and_then(|s| s.to_str()) != Some("seg") { continue; }
                let seg = super::super::buffer::segment_file::SegmentFile { path: path.clone() };
                if let Ok(meta) = seg.read_metadata() {
                    for idx in meta.index.iter() {
                        if idx.key.0 != ns { continue; }
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
    if !wal_batches.is_empty() { let schema = wal_batches[0].schema(); let filtered: Vec<RecordBatch> = wal_batches.into_iter().filter(|b| b.schema().as_ref() == schema.as_ref()).collect(); if !filtered.is_empty() { let mem = MemTable::try_new(schema.clone(), vec![filtered]).map_err(|e| DataFusionError::Internal(e.to_string())).unwrap(); let _ = ctx.register_table(&format!("{}_wal", &ns), Arc::new(mem)); } }

    // S3 side
    let s3_loc = Config::get_output_parquet_s3_location(&ns).ok_or_else(|| "No S3 parquet output configured".to_string())?;
    register_s3_object_store(&ctx, &s3_loc).await;
    let mut s3_paths: Vec<String> = Vec::new();
    if let Some(man) = Config::read_manifest(&ns).await {
        if let Some(tables) = man.get("tables").and_then(|t| t.as_object()) {
            if let Some(nsobj) = tables.get(&ns).and_then(|v| v.as_object()) {
                if let Some(prefixes) = nsobj.get("prefixes").and_then(|p| p.as_array()) {
                    if let Ok(u) = Url::parse(&s3_loc) { if let Some(bucket) = u.host_str() { for pref in prefixes { if let Some(pstr) = pref.as_str() { let mut dir = pstr.trim_matches('/').to_string(); if !dir.ends_with('/') { dir.push('/'); } s3_paths.push(format!("s3://{}/{}", bucket, dir)); } } } }
                }
            }
        }
    }
    if s3_paths.is_empty() { s3_paths.push(s3_loc.clone()); }
    let mut sources: Vec<String> = Vec::new();
    for (idx, path) in s3_paths.iter().enumerate() { let tname = format!("{}_s3_{}", &ns, idx); let _ = ctx.register_parquet(&tname, path, ParquetReadOptions::default()).await; sources.push(tname); }
    // Union S3
    let mut df_s3 = ctx.table(&sources[0]).await.map_err(|e| e.to_string())?;
    for t in sources.iter().skip(1) { if let Ok(df_next) = ctx.table(t).await { df_s3 = df_s3.union(df_next).map_err(|e| e.to_string())?; } }
    // Project timestamps based on ARROW_SCHEMA
    let df_s3 = {
        if let Some(swap) = ARROW_SCHEMA.get(&ns) { let arrow_schema = swap.load(); if arrow_schema.fields().is_empty() { df_s3 } else {
            fn build_expr_for_field(name: &str, dt: &ArrowDataType) -> Expr { match dt { ArrowDataType::Timestamp(_, _) => Expr::Cast(datafusion::logical_expr::expr::Cast { expr: Box::new(col(name)), data_type: ArrowDataType::Timestamp(datafusion::arrow::datatypes::TimeUnit::Millisecond, None) }).alias(name), _ => col(name) } }
            let exprs: Vec<Expr> = arrow_schema.fields().iter().map(|f| build_expr_for_field(f.name(), f.data_type())).collect();
            let base = df_s3.clone();
            match base.clone().select(exprs) { Ok(sel) => sel, Err(_) => base }
        } } else { df_s3 }
    };
    let df_wal = match ctx.table(&format!("{}_wal", &ns)).await { Ok(df) => df, Err(_) => df_s3.clone().filter(datafusion::logical_expr::lit(false)).unwrap() };
    let df_union = df_s3.union(df_wal).map_err(|e| e.to_string())?;
    let view = ViewTable::try_new(df_union.into_optimized_plan().map_err(|e| e.to_string())?, Some(ns.clone())).map_err(|e| e.to_string())?;
    ctx.register_table(&ns, Arc::new(view)).map_err(|e| e.to_string())?;

    // Fallback: LLM with semantic/catalog context
    register_semantic_and_catalog(&ctx).await;
    let mut context_lines: Vec<String> = Vec::new();
    if let Ok(df) = ctx.sql(&format!("SELECT namespace, field, role FROM semantic WHERE namespace='{}'", ns)).await { if let Ok(batches) = df.collect().await { for b in batches { for row in 0..b.num_rows() { let f = crate::sql::tui::value_to_string(b.column(1).as_ref(), row); let r = crate::sql::tui::value_to_string(b.column(2).as_ref(), row); context_lines.push(format!("field:{} role:{}", f, r)); } } } }
    if let Ok(df) = ctx.sql(&format!("SELECT field, coalesce(description,'') AS description, coalesce(synonyms,'') AS synonyms FROM catalog WHERE namespace='{}'", ns)).await { if let Ok(batches) = df.collect().await { for b in batches { for row in 0..b.num_rows() { let f = crate::sql::tui::value_to_string(b.column(0).as_ref(), row); let d = crate::sql::tui::value_to_string(b.column(1).as_ref(), row); let s = crate::sql::tui::value_to_string(b.column(2).as_ref(), row); context_lines.push(format!("catalog:{} desc:{} syn:{}", f, d, s)); } } } }
    // Add SQL docs to context
    // @todo @critical @security - ensure the destructive SQL statements are not included in the docs context
    if opts.use_docs {
        if let Ok(df) = ctx.sql(&format!("SHOW DOCS",)).await {
            if let Ok(batches) = df.collect().await {
                for b in batches {
                    for row in 0..b.num_rows() {
                        let stmt = crate::sql::tui::value_to_string(b.column(0).as_ref(), row);
                        let desc = crate::sql::tui::value_to_string(b.column(1).as_ref(), row);
                        context_lines.push(format!("sql_doc_statement:{} sql_doc_description:{}", stmt, desc));
                    }
                }
            }
        }
    }
    let prompt = format!("You are a data assistant. Given the the schemas and data catalog Context, understand the context of the Question, decide how best to query the data, query the data and sumarise the answer. You may provide tables of values in the response but not SQL or code. TableName: {}. Context:\n{}\nQuestion: {}\n.", ns, context_lines.join("\n"), question);
    let cfg = llm::config_from_env(); let model = llm::create_llm(&cfg);
    let out = model.chat(&[ChatMessage { role: "user".into(), content: prompt }]).unwrap_or_else(|_| "Sorry, I could not generate an answer.".to_string());
    // Create the SQL
    let prompt_sql = format!("You are a data assistant. Given the the schemas and data catalog Context, understand the context of the Question. Provide only the SQL query without any explanation or additional text. TableName: {}. Context: {} Question: {} Suggested Query Approach: {}", ns, context_lines.join("\n"), question, out);
    let sql = model.chat(&[ChatMessage { role: "user".into(), content: prompt_sql }]).unwrap_or_else(|_| "SELECT * FROM {}".to_string());
    // Execute the SQL
    let data = match ctx.sql(&sql).await {
        Ok(df) => {
            match df.collect().await {
                Ok(batches) => {
                    let mut results: Vec<String> = Vec::new();
                    for b in batches {
                        for row in 0..b.num_rows() {
                            let mut row_vals: Vec<String> = Vec::new();
                            for col in 0..b.num_columns() {
                                let val = crate::sql::tui::value_to_string(b.column(col).as_ref(), row);
                                row_vals.push(val);
                            }
                            results.push(row_vals.join(", "));
                        }
                    }
                    results.join("\n")
                },
                Err(_) => "Error collecting query results.".to_string(),
            }
        },
        Err(_) => "Error executing SQL query.".to_string(),
    };

    println!("\n\n--- Debug Info ---");
    println!("Question: {}\n", question);
    println!("Context:\n{}", context_lines.join("\n"));
    println!("Executed SQL Query: {}\n", sql);
    println!("Data:\n{}", data);
    println!("------------------\n\n");

    // Final prompt to decide if enough info to answer
    let final_prompt = format!("You are a data assistant. Given the the schemas, data catalog in Context and the data in Data, understand the context of the Question, decide if you have enough information to answer the question. If 'yes', provide a concise answer based on the data. You may provide tables of values in the response but no SQL or code. FOCUS ON DIRECTLY, CONCISELY ANSWERING THE QUESTION, not explaining why or how. Namespace: {}. Context:\n{}\nQuestion: {}\nSQL Query: {}\n.Data: {}\n.", ns, context_lines.join("\n"), question, sql, data);

    let final_out = model.chat(&[ChatMessage { role: "user".into(), content: final_prompt }]).unwrap_or_else(|_| "Sorry, I could not generate an answer.".to_string());
    Ok(Answer { text: final_out, followup: None })


}


