use std::{fs, process};
use std::fs::OpenOptions;
use std::io::{BufReader};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use arrow::array::{Array, ArrayRef, Int32Array, StringArray};
use arrow_schema::DataType;
use datafusion::prelude::{SessionContext};
use crate::cli::{CLI_MODE, Mode};
use crate::discover::{Metadata, PipelineMetadata};
use crate::helpers::configuration::{Config, PIPELINE_NAME};
use crate::METADATA;
use crate::plugins::athena::{AwsAthena};
use crate::sql::operators::alter_column::alter_column_type;
use crate::sql::operators::drop_column::alter_column_drop;
use crate::sql::operators::dump_schema::dump_schema;
use crate::sql::operators::drop_table::drop_table;
use crate::sql::parser::{PipelineToggle, SParser, Statement};

use chrono::{DateTime};
use datafusion::error::DataFusionError;
use crate::sql::{SqlDocParser, SqlStatementDoc};
use datafusion::prelude::{SessionConfig, ParquetReadOptions};
use datafusion::logical_expr::Volatility;
use datafusion::scalar::ScalarValue;
use datafusion::arrow::array::{Float64Array, RecordBatch};
use datafusion::arrow::datatypes::{DataType as ArrowDataType, Field, Schema as ArrowSchema};
use datafusion::execution::FunctionRegistry;
use object_store::aws::AmazonS3Builder;
use url::Url;
use datafusion::datasource::MemTable;
use datafusion::datasource::view::ViewTable;
use arrow::ipc::reader::StreamReader;
use arrow::util::pretty::pretty_format_batches;
use crate::sql::tui::{LiveTableView, LiveTableViewConfig, QueryEditorView, QueryEditorConfig};
use std::sync::mpsc;
use crate::buffer::segment_file::SegmentFile;
use std::io::{Seek, Read};
use crate::ingest_work::Ingest;
use crate::ARROW_SCHEMA;
use arc_swap::ArcSwap;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser as StdSqlParser;
use sqlparser::ast::{Statement as StdStatement, SetExpr, TableFactor, Query as StdQuery, Select as StdSelect, SelectItem as StdSelectItem, Expr as StdExpr, Ident, DataType as StdDataType, ObjectName, Function, FunctionArg, FunctionArgExpr, ObjectName as SqlObjectName, OrderByExpr, GroupByExpr};
use std::collections::HashSet;
use aws_credential_types::provider::ProvideCredentials;

async fn register_s3_object_store(ctx: &SessionContext, s3_loc: &str) {
    if let Ok(u) = Url::parse(s3_loc) {
        if u.scheme() == "s3" {
            if let Some(bucket) = u.host_str() {
                // Resolve credentials from AWS profiles/env via aws-config
                let conf = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
                let region = conf.region().map(|r| r.as_ref().to_string()).unwrap_or_else(|| std::env::var("AWS_REGION").unwrap_or_default());
                let creds_opt = conf.credentials_provider();
                let (ak, sk, tk) = if let Some(p) = creds_opt {
                    match p.provide_credentials().await {
                        Ok(c) => (c.access_key_id().to_string(), c.secret_access_key().to_string(), c.session_token().map(|t| t.to_string())),
                        Err(_) => (String::new(), String::new(), None)
                    }
                } else { (String::new(), String::new(), None) };

                let mut b = AmazonS3Builder::new().with_bucket_name(bucket);
                if !region.is_empty() { b = b.with_region(region); }
                if !ak.is_empty() && !sk.is_empty() { b = b.with_access_key_id(ak).with_secret_access_key(sk); }
                if let Some(t) = tk { b = b.with_token(t); }
                if let Ok(store) = b.build() {
                    let base = Url::parse(&format!("s3://{}/", bucket)).unwrap_or(u.clone());
                    let _ = ctx.runtime_env().register_object_store(&base, Arc::new(store));
                }
            }
        }
    }
}
// no SQL AST parsing needed; we will register union views for all pipelines

#[allow(dead_code)]
fn datediff(args: &[ArrayRef]) -> Result<ArrayRef, DataFusionError> {
    let start = as_string_array(&args[0])?;
    let end = as_string_array(&args[1])?;

    // println!("Start: {:?}", start);
    // println!("End: {:?}", end);
    //
    // let start_format = AnalyseSchema::is_valid_date(&start.value(0));
    // let end_format = AnalyseSchema::is_valid_date(&end.value(0));

    let result: Int32Array = (0..start.len())
        .map(|i| {
            if start.is_null(i) || end.is_null(i) {
                None
            } else {
                // let start_date = Helpers::parse_date_from_string(&start.value(i), start_format.unwrap());
                let start_date = DateTime::parse_from_rfc3339(start.value(i))
                    .ok()
                    .map(|dt| dt.naive_utc().date());

                // let end_date = Helpers::parse_date_from_string(&end.value(i), end_format.unwrap());
                let end_date = DateTime::parse_from_rfc3339(end.value(i))
                    .ok()
                    .map(|dt| dt.naive_utc().date());

                let _diff = end_date.clone().unwrap() - start_date.clone().unwrap();
                // println!("Diff: {:?}", diff);

                match (start_date, end_date) {
                    (Some(start_date), Some(end_date)) => Some((end_date - start_date).num_days() as i32),
                    // (Ok(start_date), Ok(end_date)) => Some(diff.num_days() as i32),
                    _ => None,
                }
            }
        })
        .collect();

    // println!("Result: {:?}", result);

    Ok(Arc::new(result) as ArrayRef)
}

#[allow(dead_code)]
fn as_string_array(array: &ArrayRef) -> Result<&StringArray, DataFusionError> {
    if let DataType::Utf8 = array.data_type() {
        Ok(array.as_any().downcast_ref::<StringArray>().unwrap())
    } else {
        Err(DataFusionError::Internal("Expected StringArray".to_string()))
    }
}

/// Documents a SQL query, returning information about what it does
#[allow(dead_code)]
pub async fn document_query(sql_str: &str) -> Result<Option<SqlStatementDoc>, String> {
    SqlDocParser::parse_and_document(sql_str)
}

/// Function to explain a SQL query in plain English before executing it
#[allow(dead_code)]
pub async fn explain_query(sql_str: &str) -> String {
    match document_query(sql_str).await {
        Ok(Some(doc)) => {
            format!(
                "This query is a {} statement.\n\n\
                 What it does: {}\n\n\
                 The correct syntax is: {}\n\n\
                 Example usage: {}", 
                doc.name, doc.description, doc.syntax, doc.example
            )
        },
        Ok(None) => {
            "I couldn't identify this type of SQL query. It might be a standard SQL query that's not specifically documented.".to_string()
        },
        Err(e) => {
            format!("Error analyzing query: {}", e)
        }
    }
}

pub async fn query(sql_str: &str) {

    let sql_trim = sql_str.trim();
    // STREAM: WAL-only, 2s refresh, table name equals pipeline name
    if sql_trim.to_uppercase().starts_with("STREAM ") {
        // Support optional WINDOW <seconds> anywhere after the projection; remove just that clause
        let mut raw_after = sql_trim[7..].trim().to_string();
        let mut window_secs_opt: Option<i64> = None;
        {
            let upper = raw_after.to_uppercase();
            if let Some(idx) = upper.find(" WINDOW ") {
                let start = idx + 8; // after ' WINDOW '
                let bytes = raw_after.as_bytes();
                let mut j = start;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() { j += 1; }
                let num_start = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() { j += 1; }
                if j > num_start {
                    if let Ok(sec) = raw_after[num_start..j].parse::<i64>() { window_secs_opt = Some(sec); }
                    // remove the WINDOW <n> segment only, preserving a space boundary
                    let left = raw_after[..idx].trim_end();
                    let right = raw_after[j..].trim_start();
                    raw_after = if right.is_empty() { left.to_string() } else { format!("{} {}", left, right) };
                }
            }
        }
        let mut select_sql = format!("SELECT {}", raw_after);
        let dialect = GenericDialect {};
        let mut table_opt: Option<String> = None;
        if let Ok(ast) = StdSqlParser::parse_sql(&dialect, &select_sql) {
            for stmt in ast {
                if let StdStatement::Query(q) = stmt {
                    match &*q.body {
                        SetExpr::Select(sel) => {
                            if let Some(twj) = sel.from.get(0) {
                                if let TableFactor::Table { name, .. } = &twj.relation { table_opt = Some(name.to_string()); }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Fallback: simple FROM parser if sqlparser fails on WINDOW syntax
        if table_opt.is_none() {
            let up = select_sql.to_uppercase();
            if let Some(fi) = up.find(" FROM ") {
                let rest = &select_sql[fi + 6..];
                let mut name = String::new();
                for ch in rest.chars() {
                    if ch.is_alphanumeric() || ch == '_' || ch == '-' { name.push(ch); } else { break; }
                }
                if !name.is_empty() { table_opt = Some(name); }
            }
        }
        let pipeline = match table_opt { Some(t) => t, None => { println!("STREAM requires a FROM <pipeline_name>"); return; } };

        // If WINDOW provided, append a time predicate using best-known time column
        if let Some(win_s) = window_secs_opt {
            // Determine time column: prefer event_date; else metadata.prcd_micro_time
            let mut time_col = "event_date".to_string();
            if let Some(swap) = ARROW_SCHEMA.get(&pipeline) {
                let schema = swap.load();
                let has_event = schema.fields().iter().any(|f| f.name() == "event_date");
                if !has_event { time_col = "metadata.prcd_micro_time".to_string(); }
            }
            let now_ms = chrono::Utc::now().timestamp_millis();
            let lower_ms = now_ms.saturating_sub(win_s.saturating_mul(1000));
            // naive append; if existing WHERE present, add AND; else add WHERE
            let upper = select_sql.to_uppercase();
            if upper.contains(" WHERE ") {
                select_sql = format!("{} AND {} >= to_timestamp_millis({})", select_sql, time_col, lower_ms);
            } else {
                // place predicate before ORDER/GROUP/LIMIT if present
                let mut insert_idx = select_sql.len();
                for kw in [" GROUP BY ", " ORDER BY ", " LIMIT "] { if let Some(i) = upper.find(kw) { insert_idx = insert_idx.min(i); } }
                if insert_idx < select_sql.len() {
                    let (head, tail) = select_sql.split_at(insert_idx);
                    select_sql = format!("{} WHERE {} >= to_timestamp_millis({}){}", head, time_col, lower_ms, tail);
                } else {
                    select_sql = format!("{} WHERE {} >= to_timestamp_millis({})", select_sql, time_col, lower_ms);
                }
            }
            // window applied; results may be empty if no recent data
        }

        // STREAM editor: editable SQL (STREAM ... or SELECT ...), background refresher pulls WAL-only
        let (tx_req, rx_req) = mpsc::channel::<String>();
        let (tx_res, rx_res) = mpsc::channel::<Vec<RecordBatch>>();
        let initial_stream_sql = select_sql.clone();
        tokio::spawn(async move {
            // helper to build (pipeline, select_sql) from either STREAM ... or SELECT ...
            let build_plan = |input: &str| -> Option<(String, String)> {
                let s = input.trim();
                if s.to_uppercase().starts_with("STREAM ") {
                    // derive FROM and optional WINDOW, then build SELECT
                    let mut raw_after = s[7..].trim().to_string();
                    // WINDOW parsing
                    let upper = raw_after.to_uppercase();
                    if let Some(idx) = upper.find(" WINDOW ") {
                        // remove just the WINDOW clause (value used only to filter by now)
                        let start = idx + 8; let bytes = raw_after.as_bytes(); let mut j = start; while j < bytes.len() && bytes[j].is_ascii_whitespace() { j += 1; }
                        let num_start = j; while j < bytes.len() && bytes[j].is_ascii_digit() { j += 1; }
                        if j > num_start { let left = raw_after[..idx].trim_end().to_string(); let right = raw_after[j..].trim_start().to_string(); raw_after = if right.is_empty() { left } else { format!("{} {}", left, right) }; }
                    }
                    let select_sql = format!("SELECT {}", raw_after);
                    let dialect = GenericDialect {}; let mut table_opt: Option<String> = None;
                    if let Ok(ast) = StdSqlParser::parse_sql(&dialect, &select_sql) { for stmt in ast { if let StdStatement::Query(q) = stmt { if let SetExpr::Select(sel) = &*q.body { if let Some(twj) = sel.from.get(0) { if let TableFactor::Table { name, .. } = &twj.relation { table_opt = Some(name.to_string()); } } } } } }
                    table_opt.map(|p| (p, select_sql))
                } else {
                    // SELECT ...; extract table for WAL scope
                    let dialect = GenericDialect {}; let mut table_opt: Option<String> = None; if let Ok(ast) = StdSqlParser::parse_sql(&dialect, s) { for stmt in ast { if let StdStatement::Query(q) = stmt { if let SetExpr::Select(sel) = &*q.body { if let Some(twj) = sel.from.get(0) { if let TableFactor::Table { name, .. } = &twj.relation { table_opt = Some(name.to_string()); } } } } } }
                    table_opt.map(|p| (p, s.to_string()))
                }
            };

            let mut current_sql = initial_stream_sql.clone();
            loop {
                if let Some((pipeline_name, run_sql)) = build_plan(&current_sql) {
                    let mut session_config = SessionConfig::new();
                    session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
                    session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());
                    let ctx = SessionContext::new_with_config(session_config);
                    PIPELINE_NAME.write().clear(); PIPELINE_NAME.write().push_str(&pipeline_name); Config::init().await;
                    let seg_dir = format!("{}/segment_buffer/segs", Config::get_data_dir());
                    let mut wal_batches: Vec<RecordBatch> = Vec::new();
                    if std::path::Path::new(&seg_dir).exists() {
                        for entry in std::fs::read_dir(&seg_dir).unwrap_or_else(|_| std::fs::read_dir("/").unwrap()) {
                            if let Ok(ent) = entry { let path = ent.path(); if path.extension().and_then(|s| s.to_str()) != Some("seg") { continue; }
                                let seg = SegmentFile { path: path.clone() };
                                if let Ok(meta) = seg.read_metadata() {
                                    for idx in meta.index.iter() {
                                        if idx.key.0 != pipeline_name { continue; }
                                        if let Ok(mut file) = std::fs::OpenOptions::new().read(true).open(&path) {
                                            if file.seek(std::io::SeekFrom::Start(idx.start)).is_ok() {
                                                let mut reader = std::io::BufReader::new(file);
                                                let mut take = reader.take(idx.len);
                                                if let Ok(sr) = StreamReader::try_new(&mut take, None) { for it in sr { if let Ok(b) = it { wal_batches.push(b); } } }
                                            }
                                        }
                                    }
                                }
                            } }
                    }
                    if !wal_batches.is_empty() {
                        let schema = wal_batches[0].schema();
                        let filtered: Vec<RecordBatch> = wal_batches.into_iter().filter(|b| b.schema().as_ref() == schema.as_ref()).collect();
                        if let Ok(mem) = MemTable::try_new(schema.clone(), vec![filtered]).map_err(|e| DataFusionError::Internal(e.to_string())) {
                            let _ = ctx.register_table(&pipeline_name, Arc::new(mem));
                            if let Ok(df) = ctx.sql(&run_sql).await { if let Ok(b) = df.collect().await { let _ = tx_res.send(b); } }
                        }
                    } else { let _ = tx_res.send(Vec::new()); }
                } else { let _ = tx_res.send(Vec::new()); }

                // poll for updated SQL (user edits)
                let mut waited = 0u64; while waited < 2000 { if let Ok(new_sql) = rx_req.try_recv() { current_sql = new_sql; break; } tokio::time::sleep(std::time::Duration::from_millis(200)).await; waited += 200; }
            }
        });

        QueryEditorView::new(&select_sql).run(QueryEditorConfig { title: &format!("STREAM {}", pipeline), footer: Some("Enter:run q:quit"), initial_sql: &select_sql }, rx_res, tx_req);
        return;
    }

    let mut parser = SParser::new(sql_str).unwrap();

    match parser.parse_statement() {
        Ok(Statement::ShowDocs) => {
            // Print SQL documentation
            println!("SQL Documentation:");
            println!("=================\n");
            
            // Group by category for better readability
            let mut schema_cmds = Vec::new();
            let mut pipeline_cmds = Vec::new();
            let mut data_cmds = Vec::new();
            let mut query_cmds = Vec::new();
            
            for doc in SqlDocParser::list_all_statements() {
                if doc.name.contains("SCHEMA") {
                    schema_cmds.push(doc);
                } else if doc.name.contains("PIPELINE") {
                    pipeline_cmds.push(doc);
                } else if doc.name.contains("TABLE") || doc.name.contains("DATABASE") {
                    data_cmds.push(doc);
                } else {
                    query_cmds.push(doc);
                }
            }
            
            if !schema_cmds.is_empty() {
                println!("Schema Operations:");
                println!("-----------------");
                for doc in schema_cmds {
                    println!("  {} - {}", doc.name, doc.description);
                    println!("  Syntax: {}", doc.syntax);
                    println!("  Example: {}\n", doc.example);
                }
            }
            
            if !pipeline_cmds.is_empty() {
                println!("Pipeline Operations:");
                println!("-------------------");
                for doc in pipeline_cmds {
                    println!("  {} - {}", doc.name, doc.description);
                    println!("  Syntax: {}", doc.syntax);
                    println!("  Example: {}\n", doc.example);
                }
            }
            
            if !data_cmds.is_empty() {
                println!("Data Operations:");
                println!("---------------");
                for doc in data_cmds {
                    println!("  {} - {}", doc.name, doc.description);
                    println!("  Syntax: {}", doc.syntax);
                    println!("  Example: {}\n", doc.example);
                }
            }
            
            if !query_cmds.is_empty() {
                println!("Query Operations:");
                println!("----------------");
                for doc in query_cmds {
                    println!("  {} - {}", doc.name, doc.description);
                    println!("  Syntax: {}", doc.syntax);
                    println!("  Example: {}\n", doc.example);
                }
            }
            
            println!("For more detailed documentation, run:");
            println!("  skippr sql-help");
        },
        Ok(Statement::DatabaseDrop(stmt)) => {
            let db_name = stmt.database.clone();

            match AwsAthena::delete_glue_database(&db_name.to_string()).await {
                Ok(_) => {
                    println!("Dropped Database: {}", db_name);
                },
                Err(e) => {
                    println!("Failed to drop database: {}", e);
                }
            }
        }
        Ok(Statement::PipelineDrop(stmt)) => {

            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str(format!("{}", &stmt.pipeline).as_str());
            Config::init().await;

            let data_dir = Config::get_data_dir();
            let pipeline_name = Config::get_pipeline_name();

            println!("Dropping all schemas and data for: {}", pipeline_name);

            match CLI_MODE.read().clone() {
                Mode::Sync(_options) => {
                    let _ = fs::remove_dir_all(&data_dir).expect(format!("Failed to remove dir: {}", data_dir).as_str());
                    Config::delete_metadata().await;
                    println!("Dropped Pipeline");
                },
                Mode::Query(_options) => {
                    match Config::get_metadata().await {
                        Ok(_metadata) => {
                            let mut empty_pipeline_metadata = PipelineMetadata::new();
                            empty_pipeline_metadata.append_sql(sql_str.to_string());

                            Config::set_metadata(&empty_pipeline_metadata, false).await;

                            println!("Done. Pipeline will drop on next sync run");
                        },
                        Err(_e) => {
                            println!("No metadata found for pipeline: {}", pipeline_name);
                        }
                    }
                    
                },
                _ => {}
            }

        },
        Ok(Statement::PipelineReset(stmt)) => {

            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str(format!("{}", &stmt.pipeline).as_str());
            Config::init().await;

            let data_dir = Config::get_data_dir();
            let pipeline_name = Config::get_pipeline_name();

            println!("Resetting offset database and purging WAL files for pipeline: {}, dir: {}", pipeline_name, data_dir);

            let mut metadata = Config::get_metadata().await.expect(format!("No metadata found for pipeline: {}", pipeline_name).as_str());
            
            match CLI_MODE.read().clone() {
                Mode::Sync(_options) => {
                    let mut tries = 15;
                    let mut _delete = true;
                    while _delete {
                        // retries as workaround for https://github.com/rust-lang/rust/issues/29497
                        match fs::remove_dir_all(&data_dir) {
                            Ok(_) => {
                                _delete = false;
                            },
                            Err(e) => {
                                println!("Failed to remove dir: {}.", e);
                                if tries == 0 {
                                    _delete = false;
                                    panic!("Failed to remove dir: {}", data_dir);
                                } else {
                                    tries -= 1;
                                    println!("Retrying in 5 seconds...");
                                    tokio::time::sleep(Duration::from_secs(5)).await;
                                }
                            }
                        }
                            // .expect(format!("Failed to remove dir: {}", data_dir).as_str());
                    }
                    println!("Pipeline reset, on next sync run all data will be re-ingested");
                    
                    // remove the SQL stmt from metadata
                    metadata.sql = None;
                    Config::set_metadata(&metadata, false).await;
                },
                Mode::Query(_options) => {

                    metadata.append_sql(sql_str.to_string());

                    Config::set_metadata(&metadata, false).await;

                    println!("Done. Pipeline will reset on next sync run");
                },
                _ => {}
            }

        },
        Ok(Statement::PipelineToggle(stmt)) => {

            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str(format!("{}", &stmt.pipeline).as_str());
            Config::init().await;

            let mut skippr_metadata = match Config::get_metadata().await {
                Ok(metadata) => {
                    metadata
                }
                Err(_e) => {
                    println!("Pipeline '{}' not found", stmt.pipeline);
                    return;
                }
            };

            skippr_metadata.enabled = match stmt.toggle {
                PipelineToggle::Enable => {
                    true
                },
                PipelineToggle::Disable => {
                    false
                }
            };

            Config::set_metadata(&skippr_metadata, false).await;

            println!("Toggled pipeline '{}' to: {}d", stmt.pipeline, stmt.toggle);
        },
        Ok(Statement::SchemaDrop(stmt)) => {

            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str(format!("{}", &stmt.pipeline).as_str());
            Config::init().await;

            let mut metadata = Config::get_metadata().await.expect(format!("No metadata found for pipeline: {}", stmt.pipeline).as_str());

            let schema = stmt.schema.clone().unwrap_or(stmt.pipeline.clone());

            match metadata.metadata.remove(&format!("{}", schema)) {
                Some(_) => {
                    println!("Dropping schema: '{}' for pipeline: '{}', on next sync schema will be re-discovered", schema, &stmt.pipeline);
                    Config::set_metadata(&metadata, true).await;
                    println!("Dropped Schema, on next sync schema will be re-discovered");
                },
                None => {
                    println!("No schema: '{}' found for pipeline: '{}'", schema, &stmt.pipeline);
                }
            }

        },

        Ok(Statement::SchemaLoad(stmt)) => {

            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str(format!("{}", &stmt.pipeline).as_str());
            Config::init().await;
            let _workspace = Config::get_workspace_name();

            // get current metadata
            let skippr_metadata = match Config::get_metadata().await {
                Ok(metadata) => {
                    metadata
                }
                Err(_e) => {
                    println!("No existing schema for {}", stmt.pipeline);
                    return;
                }
            };

            let mut metadata = skippr_metadata.metadata.get(&format!("{}", &stmt.pipeline)).expect(&format!("Schema not found for table {}", stmt.pipeline));

            // read schema from file
            let data_dir = Config::get_data_dir();
            let metadata_file = format!("{}/{}", data_dir, stmt.source);

            let file = OpenOptions::new()
                .read(true)
                .open(&metadata_file)
                .expect(&format!("Failed to open source schema file {}", &metadata_file));

            let reader = BufReader::new(file);

            let file_content_metadata: Metadata = match serde_json::from_reader(reader) {
                Ok(file_content_metadata) => file_content_metadata,
                Err(err) => {
                    std::panic!("Error loading schema: {}", err);
                }
            };

            // update metadata
            metadata.clone_from(&&file_content_metadata);

            {
                METADATA.store(Arc::new(skippr_metadata.clone()));
            }

            Config::set_metadata(&skippr_metadata, true).await;

            println!("Schema loaded from file.");
        },
        Ok(Statement::SchemaDump(stmt)) => {

            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str(format!("{}", &stmt.pipeline).as_str());
            Config::init().await;
            let _workspace = Config::get_workspace_name();

            let skippr_metadata = match Config::get_metadata().await {
                Ok(metadata) => {
                    metadata
                }
                Err(_e) => {
                    println!("No existing schema for pipeline '{}'", stmt.pipeline);
                    return;
                }
            };

            let schema_name = stmt.schema.clone().unwrap_or(stmt.pipeline.clone());

            let metadata = match skippr_metadata.metadata.get(&format!("{}", schema_name)){
                 Some(metadata) => {
                    metadata
                }
                None => {
                    println!("Schema '{}' not found for pipeline: '{}'", schema_name, &stmt.pipeline);
                    return;
                }
            };

            dump_schema(schema_name, &metadata, &stmt).expect("Failed to drop column");
            
            println!("Schema dumped to '{}'", stmt.target);
        },
        Ok(Statement::AlterSchemaDropColumn(stmt)) => {

            // println!("Alter table drop column: {}", stmt.column_name);

            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str(format!("{}", &stmt.pipeline).as_str());
            Config::init().await;

            let mut skippr_metadata = match Config::get_metadata().await {
                Ok(metadata) => {
                    metadata
                }
                Err(_e) => {
                    println!("No existing schema for {}", &stmt.pipeline);
                    return;
                }
            };

            let schema = stmt.schema.clone().unwrap_or(stmt.pipeline.clone());

            let mut metadata = skippr_metadata.metadata.get_mut(&format!("{}", &schema)).expect(&format!("Schema '{}' not found for pipeline: '{}'", schema, &stmt.pipeline));
            match alter_column_drop(&mut metadata, &stmt) {
                Ok(_) => {}
                Err(e) => {
                    println!("Failed to drop column: {}", e);
                    return;
                }
            }

            {
                METADATA.store(Arc::new(skippr_metadata.clone()));
            }

            Config::set_metadata(&skippr_metadata, true).await;

            println!("Alter schema, dropped column '{}, on pipeline: '{}' of schema '{}'.", stmt.column_name, stmt.pipeline, schema);
        },
        Ok(Statement::AlterSchemaAlterColumnType(stmt)) => {

            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str(format!("{}", &stmt.pipeline).as_str());
            Config::init().await;

            let schema = stmt.schema.clone().unwrap_or(stmt.pipeline.clone());

            let mut skippr_metadata = match Config::get_metadata().await {
                Ok(metadata) => {
                    metadata
                }
                Err(_e) => {
                    println!("No existing schema for {}", stmt.pipeline);
                    return;
                }
            };

            let mut metadata = skippr_metadata.metadata.get_mut(&format!("{}", &schema)).expect(&format!("Schema '{}' not found for pipeline: '{}'", schema, &stmt.pipeline));
            alter_column_type(&mut metadata, &stmt).expect("Failed to alter column type");

            {
                METADATA.store(Arc::new(skippr_metadata.clone()));
            }

            Config::set_metadata(&skippr_metadata, false).await;

            println!("Alter schema: {} column: '{}' type to {}", schema, stmt.column_name, stmt.new_type);
        },
        Ok(Statement::TableDrop(stmt)) => {
            // Get the schema and table names
            let table_str = format!("{}", stmt.table);
            let schema_str = match &stmt.schema {
                Some(schema) => format!("{}", schema),
                None => "".to_string(), // No schema specified
            };

            // Set the pipeline name based on the table or schema
            PIPELINE_NAME.write().clear();
            if schema_str.is_empty() {
                // If no schema specified, use the table name as the pipeline
                PIPELINE_NAME.write().push_str(&table_str);
            } else {
                // Otherwise use the schema name as the pipeline
                PIPELINE_NAME.write().push_str(&schema_str);
            }
            Config::init().await;

            // Get the current metadata
            let mut skippr_metadata = match Config::get_metadata().await {
                Ok(metadata) => metadata,
                Err(_) => {
                    println!("No existing metadata for pipeline");
                    return;
                }
            };

            // Use the drop_table operator to remove the table from metadata
            match drop_table(&mut skippr_metadata, &stmt) {
                Ok(_) => {
                    let metadata_key = if schema_str.is_empty() {
                        table_str.clone()
                    } else {
                        format!("{}.{}", schema_str, table_str)
                    };
                    
                    println!("Dropping table: '{}'", metadata_key);
                    
                    // Update the global metadata
                    {
                        METADATA.store(Arc::new(skippr_metadata.clone()));
                    }
                    
                    // Save the updated metadata
                    Config::set_metadata(&skippr_metadata, false).await; // we don't need to sync the schemas as we are dropping the table below

                    // Delete Glue table
                    match AwsAthena::glue_delete_table(&table_str).await {
                        Ok(_) => {
                            println!("Dropped table: {}", table_str);
                        },
                        Err(e) => {
                            println!("{}", e);
                        }
                    }
                    
                },
                Err(e) => {
                    println!("{}", e);
                }
            }
        },
        // Err(e) => {
        //
        //     println!("Unknown SQL Dialect. {}", e);
        // },
        _ => {
            // Fall back to DataFusion for standard SQL (e.g., SELECT ...)
            // Build a context and register available pipeline table from current data dir
            let mut session_config = SessionConfig::new();
            session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
            session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());

            let ctx = SessionContext::new_with_config(session_config);

            // Register UDFs
            // lateness(ts, ref_ts) -> seconds late (f64)
            let lateness_udf = datafusion::logical_expr::create_udf(
                "lateness",
                vec![ArrowDataType::Timestamp(datafusion::arrow::datatypes::TimeUnit::Millisecond, None), ArrowDataType::Timestamp(datafusion::arrow::datatypes::TimeUnit::Millisecond, None)],
                Arc::new(ArrowDataType::Float64),
                Volatility::Immutable,
                datafusion::physical_plan::functions::make_scalar_function(|args| {
                    let ts = args[0].as_any().downcast_ref::<datafusion::arrow::array::TimestampMillisecondArray>().unwrap();
                    let ref_ts = args[1].as_any().downcast_ref::<datafusion::arrow::array::TimestampMillisecondArray>().unwrap();
                    let mut builder = Float64Array::builder(ts.len());
                    for i in 0..ts.len() {
                        if ts.is_null(i) || ref_ts.is_null(i) { builder.append_null(); } else {
                            let v = (ref_ts.value(i) - ts.value(i)) as f64 / 1000.0;
                            builder.append_value(v);
                        }
                    }
                    Ok(Arc::new(builder.finish()) as ArrayRef)
                })
            );
            ctx.register_udf(lateness_udf);

            // new_session(ts, prev_ts, gap_ms) -> bool (true if ts - prev_ts > gap_ms)
            let new_session_udf = datafusion::logical_expr::create_udf(
                "new_session",
                vec![
                    ArrowDataType::Timestamp(datafusion::arrow::datatypes::TimeUnit::Millisecond, None),
                    ArrowDataType::Timestamp(datafusion::arrow::datatypes::TimeUnit::Millisecond, None),
                    ArrowDataType::Int64,
                ],
                Arc::new(ArrowDataType::Boolean),
                Volatility::Immutable,
                datafusion::physical_plan::functions::make_scalar_function(|args| {
                    let ts = args[0].as_any().downcast_ref::<datafusion::arrow::array::TimestampMillisecondArray>().unwrap();
                    let prev = args[1].as_any().downcast_ref::<datafusion::arrow::array::TimestampMillisecondArray>().unwrap();
                    let gap = args[2].as_any().downcast_ref::<datafusion::arrow::array::Int64Array>().unwrap();
                    let mut builder = datafusion::arrow::array::BooleanBuilder::new();
                    for i in 0..ts.len() {
                        if ts.is_null(i) || prev.is_null(i) || gap.is_null(i) {
                            builder.append_value(true); // treat null prev as new session
                        } else {
                            let d = ts.value(i) - prev.value(i);
                            builder.append_value(d > gap.value(i));
                        }
                    }
                    Ok(Arc::new(builder.finish()) as ArrayRef)
                })
            );
            ctx.register_udf(new_session_udf);

            // zscore(val, mean, std) -> f64; is_outlier_z(val, mean, std, threshold) -> bool
            let zscore_udf = datafusion::logical_expr::create_udf(
                "zscore",
                vec![ArrowDataType::Float64, ArrowDataType::Float64, ArrowDataType::Float64],
                Arc::new(ArrowDataType::Float64),
                Volatility::Immutable,
                datafusion::physical_plan::functions::make_scalar_function(|args| {
                    let v = args[0].as_any().downcast_ref::<Float64Array>().unwrap();
                    let m = args[1].as_any().downcast_ref::<Float64Array>().unwrap();
                    let s = args[2].as_any().downcast_ref::<Float64Array>().unwrap();
                    let mut out = Float64Array::builder(v.len());
                    for i in 0..v.len() {
                        if v.is_null(i) || m.is_null(i) || s.is_null(i) || s.value(i) == 0.0 {
                            out.append_null();
                        } else {
                            out.append_value((v.value(i) - m.value(i)) / s.value(i));
                        }
                    }
                    Ok(Arc::new(out.finish()) as ArrayRef)
                })
            );
            ctx.register_udf(zscore_udf);

            let is_outlier_z_udf = datafusion::logical_expr::create_udf(
                "is_outlier_z",
                vec![ArrowDataType::Float64, ArrowDataType::Float64, ArrowDataType::Float64, ArrowDataType::Float64],
                Arc::new(ArrowDataType::Boolean),
                Volatility::Immutable,
                datafusion::physical_plan::functions::make_scalar_function(|args| {
                    let v = args[0].as_any().downcast_ref::<Float64Array>().unwrap();
                    let m = args[1].as_any().downcast_ref::<Float64Array>().unwrap();
                    let s = args[2].as_any().downcast_ref::<Float64Array>().unwrap();
                    let t = args[3].as_any().downcast_ref::<Float64Array>().unwrap();
                    let mut out = datafusion::arrow::array::BooleanBuilder::new();
                    for i in 0..v.len() {
                        if v.is_null(i) || m.is_null(i) || s.is_null(i) || t.is_null(i) || s.value(i) == 0.0 {
                            out.append_value(false);
                        } else {
                            let z = (v.value(i) - m.value(i)) / s.value(i);
                            out.append_value(z.abs() > t.value(i));
                        }
                    }
                    Ok(Arc::new(out.finish()) as ArrayRef)
                })
            );
            ctx.register_udf(is_outlier_z_udf);

            // Build union views ONLY for tables referenced in this SQL
            let original_pipeline = Config::get_pipeline_name();
            let dialect = GenericDialect {};
            let mut table_names: Vec<String> = Vec::new();
            if let Ok(ast) = StdSqlParser::parse_sql(&dialect, sql_str) {
                for stmt in ast {
                    if let StdStatement::Query(q) = stmt {
                        match &*q.body {
                            SetExpr::Select(sel) => {
                                for twj in &sel.from {
                                    if let TableFactor::Table { name, .. } = &twj.relation { table_names.push(name.to_string()); }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            if table_names.is_empty() { println!("Could not infer table name from query; expected FROM <pipeline_name> or 'deadletters'"); process::exit(1); }
            table_names.sort(); table_names.dedup();

            // Special-case: register 'deadletters' table (Parquet over S3 state bucket)
            let has_deadletters = table_names.iter().any(|t| t == "deadletters");
            if has_deadletters {
                // Ensure configuration is initialized so state bucket is resolved from SKIPPR_CONFIG_FILE
                Config::init().await;
                // Prefer scanning within tenant/workspace to include all pipelines while staying precise
                let bucket = Config::get_skippr_s3_bucket();
                let tenant = Config::get_tenant();
                let workspace = Config::get_workspace_name();
                let pipeline = Config::get_pipeline_name();
                // Writer layout: deadletters/<tenant>/<workspace>/<pipeline>/namespace=<ns>/p_year=.../p_month=.../p_day=.../*.parquet
                // Register at deadletters/<tenant>/<workspace>/ so recursive listing finds all pipelines
                let dl_url = format!("s3://{}/deadletters/{}/{}/{}/", bucket, tenant, workspace, pipeline);
                register_s3_object_store(&ctx, &dl_url).await;
                // Optional debug: verify access and show a few sample objects under the prefix
                // if Config::debug_enabled() {
                    println!("Registering deadletters table at URL: {}", dl_url);
                    let prefix = format!("deadletters/{}/{}/", tenant, workspace);
                    let sample = crate::helpers::s3::list_parquet_keys(&bucket, &prefix, 8).await;
                    if sample.is_empty() {
                        println!("Deadletters debug: No parquet files found under s3://{}/{}", bucket, prefix);
                    } else {
                        println!("Deadletters debug: Found {} parquet objects (showing up to 8):", sample.len());
                        for k in sample.iter().take(8) {
                            println!("  s3://{}/{}", bucket, k);
                        }
                    }
                // }
                // Register as Parquet listing with partition columns for pruning
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
                    file_sort_order: vec![]
                };
                if let Err(e) = ctx.register_parquet("deadletters", &dl_url, opts).await { println!("Failed to register deadletters at {}: {}", dl_url, e); }
            }

            // Remove 'deadletters' from pipeline tables to avoid pipeline processing below
            let mut table_names: Vec<String> = table_names.into_iter().filter(|t| t != "deadletters").collect();
            if table_names.is_empty() {
                // Only deadletters requested; execute query directly
                if let Ok(df) = ctx.sql(sql_str).await { if let Ok(b) = df.collect().await {
                    match pretty_format_batches(&b) { Ok(s) => println!("{}", s), Err(_) => {} }
                    return;
                } }
            }
            for pipeline in table_names {
                // Switch pipeline context for correct local WAL dir and config resolution
                PIPELINE_NAME.write().clear();
                PIPELINE_NAME.write().push_str(&pipeline);
                Config::init().await;

                // Bootstrap METADATA and ARROW_SCHEMA like main sync
                match Config::get_metadata().await {
                    Ok(pm) => {
                        let _keys: Vec<String> = pm.metadata.keys().cloned().collect();
                        METADATA.store(Arc::new(pm.clone()));
                        let flatten = Config::get_transform_flatten_events();
                        if pm.metadata.contains_key(&pipeline) {
                            match Ingest::prepare_arrow_schema_with_metadata(&pipeline, &pm.metadata, flatten) {
                                Ok(schema) => {
                                    ARROW_SCHEMA.insert(pipeline.clone(), ArcSwap::from(schema.clone()));
                                    let field_list: Vec<String> = schema.fields().iter().map(|f| format!("{}:{:?}", f.name(), f.data_type())).collect();
                                    println!("Published ARROW_SCHEMA for '{}' (fields={}, {:?})", pipeline, schema.fields().len(), field_list);
                                },
                                Err(e) => {
                                    println!("Failed to build Arrow schema for '{}': {}", pipeline, e);
                                }
                            }
                        } else {
                            // missing namespace; proceed without schema
                        }
                    },
                    Err(_) => {
                        // no metadata; proceed
                        METADATA.store(Arc::new(PipelineMetadata::new()));
                    }
                }

                // WAL → MemTable
                let seg_dir = format!("{}/segment_buffer/segs", Config::get_data_dir());
                let mut wal_batches: Vec<RecordBatch> = Vec::new();
                let mut wal_seg_files: usize = 0;
                if std::path::Path::new(&seg_dir).exists() {
                    for entry in std::fs::read_dir(&seg_dir).unwrap_or_else(|_| std::fs::read_dir("/").unwrap()) {
                        if let Ok(ent) = entry { let path = ent.path(); if path.extension().and_then(|s| s.to_str()) != Some("seg") { continue; }
                            let seg = SegmentFile { path: path.clone() };
                            if let Ok(meta) = seg.read_metadata() {
                                for idx in meta.index.iter() {
                                    if idx.key.0 != pipeline { continue; }
                                    if let Ok(mut file) = std::fs::OpenOptions::new().read(true).open(&path) {
                                        if file.seek(std::io::SeekFrom::Start(idx.start)).is_ok() {
                                            let mut reader = std::io::BufReader::new(file);
                                            let mut take = reader.take(idx.len);
                                            if let Ok(sr) = StreamReader::try_new(&mut take, None) { for it in sr { if let Ok(b) = it { wal_batches.push(b); } } }
                                            wal_seg_files += 1;
                                        }
                                    }
                                }
                            }
                        } }
                }
                let wal_rows: usize = wal_batches.iter().map(|b| b.num_rows()).sum();
                if !wal_batches.is_empty() { let schema = wal_batches[0].schema(); let filtered: Vec<RecordBatch> = wal_batches.into_iter().filter(|b| b.schema().as_ref() == schema.as_ref()).collect(); if !filtered.is_empty() { let mem = MemTable::try_new(schema.clone(), vec![filtered]).map_err(|e| DataFusionError::Internal(e.to_string())).unwrap(); let _ = ctx.register_table(&format!("{}_wal", &pipeline), Arc::new(mem)); } }

                // S3 parquet (required)
                let s3_loc = match Config::get_output_parquet_s3_location(&pipeline) { Some(loc) => loc, None => { println!("No S3 output configured for pipeline '{}'; SELECT requires S3 + WAL", pipeline); process::exit(1); } };
                register_s3_object_store(&ctx, &s3_loc).await;
                // Prepare Parquet registration paths: prefer latest manifest prefix(es) to avoid schema merge conflicts
                let mut s3_paths: Vec<String> = Vec::new();
                if let Some(man) = Config::read_manifest(&pipeline).await {
                    if let Some(tables) = man.get("tables").and_then(|t| t.as_object()) {
                        if let Some(ns) = tables.get(&pipeline).and_then(|v| v.as_object()) {
                            if let Some(prefixes) = ns.get("prefixes").and_then(|p| p.as_array()) {
                                if let Some(last) = prefixes.last().and_then(|v| v.as_str()) {
                                    if let Ok(u) = Url::parse(&s3_loc) { if let Some(bucket) = u.host_str() {
                                        let mut dir = last.trim_matches('/').to_string();
                                        if !dir.ends_with('/') { dir.push('/'); }
                                        s3_paths.push(format!("s3://{}/{}", bucket, dir));
                                    } }
                                }
                            }
                        }
                    }
                }
                if s3_paths.is_empty() { s3_paths.push(s3_loc.clone()); }
                // Cache: registry of already registered S3 object tables per (namespace, manifest_epoch)
                let manifest_epoch = Config::get_manifest_epoch(&pipeline).await.unwrap_or(0);
                let mut registry = Config::read_registry(&pipeline).await.unwrap_or(serde_json::json!({"epoch": 0u64, "sources": []}));
                let reg_epoch = registry.get("epoch").and_then(|v| v.as_u64()).unwrap_or(0);
                if reg_epoch != manifest_epoch { registry = serde_json::json!({"epoch": manifest_epoch, "sources": []}); }
                let mut sources_cached: Vec<(String, String)> = registry
                    .get("sources")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|v| {
                        let name = v.get("name").and_then(|s| s.as_str()).map(|s| s.to_string());
                        let url = v.get("url").and_then(|s| s.as_str()).map(|s| s.to_string());
                        match (name, url) { (Some(n), Some(u)) => Some((n, u)), _ => None }
                    })
                    .collect();

                // If cached sources exist, register them quickly and skip listing
                if !sources_cached.is_empty() {
                    for (name, url) in &sources_cached {
                        let opts = ParquetReadOptions { schema: None, file_extension: "parquet", table_partition_cols: vec![], parquet_pruning: None, skip_metadata: Some(true), file_sort_order: vec![] };
                        let _ = ctx.register_parquet(name, url, opts).await;
                    }
                }

                // If cache empty, build minimal table set and persist
                if sources_cached.is_empty() {
                    let mut registered: usize = 0;
                    let mut new_sources: Vec<(String, String)> = Vec::new();
                    for (idx, path) in s3_paths.iter().enumerate() {
                    if let Ok(u) = Url::parse(path) { if let Some(bucket) = u.host_str() { let prefix = u.path().trim_start_matches('/');
                        // fetch up to 8 parquet files under this prefix
                        let files = crate::helpers::s3::list_parquet_keys(bucket, prefix, 8).await;
                        if !files.is_empty() {
                            for (j, key) in files.iter().enumerate() {
                                let tname = format!("{}_s3_{}_{}", &pipeline, idx, j);
                                let file_url = format!("s3://{}/{}", bucket, key);
                                let opts = ParquetReadOptions { schema: None, file_extension: "parquet", table_partition_cols: vec![], parquet_pruning: None, skip_metadata: Some(true), file_sort_order: vec![] };
                                if let Err(e) = ctx.register_parquet(&tname, &file_url, opts).await { println!("Failed to register S3 object='{}' for '{}': {}", file_url, pipeline, e); process::exit(1); }
                                registered += 1;
                                    new_sources.push((tname, file_url));
                            }
                            continue;
                        }
                    } }
                    // fallback: register the path (dir) directly
                    let opts = ParquetReadOptions { schema: None, file_extension: "parquet", table_partition_cols: vec![], parquet_pruning: None, skip_metadata: Some(true), file_sort_order: vec![] };
                    let tname = format!("{}_s3_{}", &pipeline, idx);
                    if let Err(e) = ctx.register_parquet(&tname, path, opts).await { println!("Failed to register S3 table path='{}' for '{}': {}", path, pipeline, e); process::exit(1); }
                    registered += 1;
                        new_sources.push((tname, path.clone()));
                }
                    // persist cache
                    let sources_json: Vec<serde_json::Value> = new_sources.iter().map(|(n, u)| serde_json::json!({"name": n, "url": u})).collect();
                    registry = serde_json::json!({"epoch": manifest_epoch, "sources": sources_json});
                    Config::write_registry(&pipeline, &registry).await;
                    sources_cached = new_sources;
                }
                // optional preview removed to reduce noise

                // Union view as pipeline name with projection to cast known top-level timestamp fields
                // Build base DF by unioning all registered S3 path tables
                // Build base DF by unioning all registered S3 object tables
                // Build base DF by unioning cached tables in registry
                let table_names: Vec<String> = sources_cached.iter().map(|(n, _)| n.clone()).collect();
                let mut df_s3_base = ctx.table(&table_names[0]).await.expect("S3 table missing");
                for t in table_names.iter().skip(1) { if let Ok(df_next) = ctx.table(t).await { df_s3_base = df_s3_base.union(df_next).expect("union s3 objs"); } }
                let _s3_fields: Vec<String> = df_s3_base.schema().fields().iter().map(|f| f.name().clone()).collect();
                let df_s3 = {
                    if let Some(swap) = ARROW_SCHEMA.get(&pipeline) {
                        let arrow_schema = swap.load();
                        use datafusion::logical_expr::{col, Expr, lit};
                        if arrow_schema.fields().is_empty() { df_s3_base.clone() } else {
                        // Builder: for each top-level field, rebuild struct fields recursively casting int64 to timestamp where ARROW_SCHEMA says timestamp
                        fn build_expr_for_field(name: &str, dt: &ArrowDataType) -> Expr {
                            match dt {
                                ArrowDataType::Timestamp(_, _) => Expr::Cast(datafusion::logical_expr::expr::Cast { expr: Box::new(col(name)), data_type: ArrowDataType::Timestamp(datafusion::arrow::datatypes::TimeUnit::Millisecond, None) }).alias(name),
                                ArrowDataType::Struct(fields) => {
                                    // For structs, recursively cast child leafs but keep struct unchanged (DataFusion lacks struct builder in SELECT)
                                    // Use the original column; nested leaf access in user queries will be cast on projection below when referenced
                                    col(name)
                                }
                                ArrowDataType::List(field) => {
                                    // Lists of structs/timestamps: leave as-is for now (casting within lists requires explode/transform)
                                    col(name)
                                }
                                _ => col(name),
                            }
                        }
                        let mut exprs: Vec<Expr> = Vec::with_capacity(arrow_schema.fields().len());
                        for f in arrow_schema.fields() {
                            exprs.push(build_expr_for_field(f.name(), f.data_type()));
                        }
                        if exprs.is_empty() { df_s3_base.clone() } else { match df_s3_base.clone().select(exprs) { Ok(dfp) => dfp, Err(_) => df_s3_base.clone() } }
                        }
                    } else { df_s3_base.clone() }
                };
                // Ensure WAL side matches S3 columns count/order: project WAL to S3's schema if present
                let df_wal = match ctx.table(&format!("{}_wal", &pipeline)).await { Ok(df) => df, Err(_) => df_s3.clone().filter(datafusion::logical_expr::lit(false)).unwrap() };
                let df_union = {
                    let left_schema = df_s3.schema();
                    let right_schema = df_wal.schema();
                    let left_cols = left_schema.fields().len();
                    let right_cols = right_schema.fields().len();
                    if left_cols == 0 && right_cols == 0 {
                        // nothing to union; keep S3 side (empty)
                        df_s3.clone()
                    } else if left_cols == 0 {
                        // only WAL has data
                        df_wal.clone()
                    } else if right_cols == 0 {
                        // only S3 has data
                        df_s3.clone()
                    } else if left_cols != right_cols {
                        // try to project WAL to S3's column set
                        use datafusion::logical_expr::col;
                        let mut exprs: Vec<datafusion::logical_expr::Expr> = Vec::new();
                        for f in left_schema.fields() { exprs.push(col(f.name())); }
                        if !exprs.is_empty() {
                            if let Ok(projected) = df_wal.clone().select(exprs) { df_s3.union(projected).expect("union") } else { df_s3.union(df_wal).expect("union") }
                        } else { df_s3.union(df_wal).expect("union") }
                    } else {
                        df_s3.union(df_wal).expect("union")
                    }
                };
                let view = ViewTable::try_new(df_union.into_optimized_plan().expect("optimize"), Some(pipeline.clone())).expect("view");
                ctx.register_table(&pipeline, Arc::new(view)).expect("register union view");
                // union view registered
            }
            // Restore original pipeline context
            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str(&original_pipeline);

            // Execute the query
            // Build whitelist of timestamp paths from ARROW_SCHEMA for all referenced tables
            fn collect_ts_paths(dt: &ArrowDataType, prefix: &str, out: &mut std::collections::HashSet<String>) {
                match dt {
                    ArrowDataType::Timestamp(_, _) => { if !prefix.is_empty() { out.insert(prefix.to_string()); } }
                    ArrowDataType::Struct(fields) => {
                        for f in fields.iter() {
                            let p = if prefix.is_empty() { f.name().to_string() } else { format!("{}.{}", prefix, f.name()) };
                            collect_ts_paths(f.data_type(), &p, out);
                        }
                    }
                    ArrowDataType::List(field) => {
                        // Skip lists for now; safe and avoids complex per-element rewrites
                        let _ = field;
                    }
                    _ => {}
                }
            }
            let mut ts_whitelist: std::collections::HashSet<String> = std::collections::HashSet::new();
            for entry in ARROW_SCHEMA.iter() {
                let schema = entry.value().load();
                for f in schema.fields() { collect_ts_paths(f.data_type(), f.name(), &mut ts_whitelist); }
            }

            // Rewrite SQL: wrap referenced dotted paths in to_timestamp_millis where in whitelist
            fn needs_cast_path(idents: &Vec<Ident>, whitelist: &std::collections::HashSet<String>) -> bool {
                if idents.is_empty() { return false; }
                let path = idents.iter().map(|i| i.value.clone()).collect::<Vec<String>>().join(".");
                whitelist.contains(&path)
            }
            fn rewrite_expr(expr: &mut StdExpr, whitelist: &std::collections::HashSet<String>) {
                match expr {
                    StdExpr::CompoundIdentifier(idents) => {
                        if needs_cast_path(idents, whitelist) {
                            let arg = StdExpr::CompoundIdentifier(idents.clone());
                            *expr = StdExpr::Function(Function {
                                name: SqlObjectName(vec![Ident::new("to_timestamp_millis")]),
                                args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))],
                                over: None,
                                distinct: false,
                                special: false,
                                order_by: vec![],
                                null_treatment: None,
                                filter: None,
                            });
                        }
                    }
                    StdExpr::Identifier(_)
                    | StdExpr::Value(_)
                    | StdExpr::Wildcard => {}
                    StdExpr::BinaryOp { left, right, .. } => { rewrite_expr(left, whitelist); rewrite_expr(right, whitelist); }
                    StdExpr::UnaryOp { expr: inner, .. } => { rewrite_expr(inner, whitelist); }
                    StdExpr::Nested(inner) => { rewrite_expr(inner, whitelist); }
                    StdExpr::Function(f) => {
                        for a in f.args.iter_mut() {
                            if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = a { rewrite_expr(e, whitelist); }
                        }
                    }
                    StdExpr::Cast { expr: inner, .. } => { rewrite_expr(inner, whitelist); }
                    _ => {}
                }
            }
            fn rewrite_statement(stmt: &mut StdStatement, whitelist: &std::collections::HashSet<String>) {
                if let StdStatement::Query(q) = stmt {
                    if let StdQuery { body, order_by, limit, .. } = q.as_mut() {
                        match &mut **body {
                            SetExpr::Select(sel) => {
                                let StdSelect { projection, selection, group_by, having, .. } = sel.as_mut();
                                for item in projection.iter_mut() {
                                    match item {
                                        StdSelectItem::UnnamedExpr(e) => rewrite_expr(e, whitelist),
                                        StdSelectItem::ExprWithAlias { expr, .. } => rewrite_expr(expr, whitelist),
                                        _ => {}
                                    }
                                }
                                if let Some(e) = selection.as_mut() { rewrite_expr(e, whitelist); }
                                // group by
                                match group_by {
                                    GroupByExpr::Expressions(exprs) => {
                                        for e in exprs.iter_mut() { rewrite_expr(e, whitelist); }
                                    }
                                    _ => {}
                                }
                                if let Some(h) = having.as_mut() { rewrite_expr(h, whitelist); }
                            }
                            _ => {}
                        }
                        for ob in order_by.iter_mut() { rewrite_expr(&mut ob.expr, whitelist); }
                        // limit is an Expr in this parser version; nothing to rewrite here
                    }
                }
            }
            let dialect = GenericDialect {};
            let rewritten_sql = if let Ok(mut ast) = StdSqlParser::parse_sql(&dialect, sql_str) {
                for stmt in ast.iter_mut() { rewrite_statement(stmt, &ts_whitelist); }
                let s = ast.into_iter().map(|s| s.to_string()).collect::<Vec<String>>().join("; ");
                s
            } else { sql_str.to_string() };

            // SELECT execution: support --watch for live TUI; else one-shot
            let watch_secs = match CLI_MODE.read().clone() { Mode::Query(opts) => opts.watch, _ => None };
            // Unified SELECT TUI editor: editable SQL, runs on Enter or r; if --watch set, periodic refresh
            let initial_sql = rewritten_sql.clone();
            let (tx_req, rx_req) = mpsc::channel::<String>();
            let (tx_res, rx_res) = mpsc::channel::<Vec<RecordBatch>>();
            let ctx_clone = ctx.clone();
            tokio::spawn(async move {
                let mut current = initial_sql.clone();
                loop {
                    // Decide between STREAM (WAL-only) and SELECT (ctx_clone)
                    let trimmed = current.trim();
                    if trimmed.to_uppercase().starts_with("STREAM ") {
                        // Build SELECT from STREAM and execute against WAL-only context
                        let mut raw_after = trimmed[7..].trim().to_string();
                        // Strip optional WINDOW <n>
                        let upper = raw_after.to_uppercase();
                        if let Some(idx) = upper.find(" WINDOW ") {
                            let start = idx + 8; let bytes = raw_after.as_bytes(); let mut j = start; while j < bytes.len() && bytes[j].is_ascii_whitespace() { j += 1; }
                            let num_start = j; while j < bytes.len() && bytes[j].is_ascii_digit() { j += 1; }
                            if j > num_start { let left = raw_after[..idx].trim_end().to_string(); let right = raw_after[j..].trim_start().to_string(); raw_after = if right.is_empty() { left } else { format!("{} {}", left, right) }; }
                        }
                        let select_sql = format!("SELECT {}", raw_after);
                        // Extract pipeline name
                        let dialect = GenericDialect {}; let mut table_opt: Option<String> = None;
                        if let Ok(ast) = StdSqlParser::parse_sql(&dialect, &select_sql) { for stmt in ast { if let StdStatement::Query(q) = stmt { if let SetExpr::Select(sel) = &*q.body { if let Some(twj) = sel.from.get(0) { if let TableFactor::Table { name, .. } = &twj.relation { table_opt = Some(name.to_string()); } } } } } }
                        if let Some(pipeline_name) = table_opt {
                            let mut session_config = SessionConfig::new();
                            session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
                            session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());
                            let ctx = SessionContext::new_with_config(session_config);
                            PIPELINE_NAME.write().clear(); PIPELINE_NAME.write().push_str(&pipeline_name); Config::init().await;
                            let seg_dir = format!("{}/segment_buffer/segs", Config::get_data_dir());
                            let mut wal_batches: Vec<RecordBatch> = Vec::new();
                            if std::path::Path::new(&seg_dir).exists() {
                                for entry in std::fs::read_dir(&seg_dir).unwrap_or_else(|_| std::fs::read_dir("/").unwrap()) {
                                    if let Ok(ent) = entry { let path = ent.path(); if path.extension().and_then(|s| s.to_str()) != Some("seg") { continue; }
                                        let seg = SegmentFile { path: path.clone() };
                                        if let Ok(meta) = seg.read_metadata() {
                                            for idx in meta.index.iter() {
                                                if idx.key.0 != pipeline_name { continue; }
                                                if let Ok(mut file) = std::fs::OpenOptions::new().read(true).open(&path) {
                                                    if file.seek(std::io::SeekFrom::Start(idx.start)).is_ok() {
                                                        let mut reader = std::io::BufReader::new(file);
                                                        let mut take = reader.take(idx.len);
                                                        if let Ok(sr) = StreamReader::try_new(&mut take, None) { for it in sr { if let Ok(b) = it { wal_batches.push(b); } } }
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
                                if let Ok(mem) = MemTable::try_new(schema.clone(), vec![filtered]).map_err(|e| DataFusionError::Internal(e.to_string())) {
                                    let _ = ctx.register_table(&pipeline_name, Arc::new(mem));
                                    if let Ok(df) = ctx.sql(&select_sql).await { if let Ok(b) = df.collect().await { let _ = tx_res.send(b); } }
                                } else { let _ = tx_res.send(Vec::new()); }
                            } else { let _ = tx_res.send(Vec::new()); }
                        } else { let _ = tx_res.send(Vec::new()); }
                    } else {
                        // SELECT: run against prepared context (S3/union already registered earlier)
                        if let Ok(df) = ctx_clone.sql(&current).await { if let Ok(b) = df.collect().await { let _ = tx_res.send(b); } }
                    }

                    // wait for either watch tick or new request
                    if let Some(w) = watch_secs {
                        let mut waited_ms: u64 = 0; let step = 200u64; let total = w.saturating_mul(1000);
                        while waited_ms < total { if let Ok(new_sql) = rx_req.try_recv() { current = new_sql; break; } tokio::time::sleep(std::time::Duration::from_millis(step)).await; waited_ms = waited_ms.saturating_add(step); }
                    } else {
                        loop { if let Ok(new_sql) = rx_req.try_recv() { current = new_sql; break; } tokio::time::sleep(std::time::Duration::from_millis(200)).await; }
                    }
                }
            });
            let footer = if let Some(w) = watch_secs { format!("select --watch={}s | Enter:run q:quit", w) } else { "Enter:run q:quit".to_string() };
            QueryEditorView::new(&rewritten_sql).run(QueryEditorConfig { title: "SELECT", footer: Some(&footer), initial_sql: &rewritten_sql }, rx_res, tx_req);
        }
    }
}

#[allow(dead_code)]
fn recurse_paths(output_dir: &str, table_name: &str, ctx: &SessionContext, paths: &mut Vec<PathBuf>) {
    // itterate over output_dir and fine any dir paths that include p_year=2023
    for entry in fs::read_dir(&output_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            let path_str = path.to_str().unwrap();
            if path_str.contains("p_year=2023") {
                println!("Found data for table: {} in dir: {}", table_name, path_str);
                paths.push(path);

            } else {
                recurse_paths(&path_str, table_name, ctx, paths);
            }
        }
    }
}