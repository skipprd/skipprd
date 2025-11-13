use std::{fs, process};
use std::fs::OpenOptions;
use std::io::{BufReader};
// removed unused Write import
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use arrow::array::{Array, ArrayRef, Int32Array, StringArray};
use arrow_schema::DataType;
use datafusion::prelude::{SessionContext};
use crate::cli::{CLI_MODE, Mode, QueryOptions};
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
use datafusion::prelude::{SessionConfig};
// removed unused SqlIdent
// no Volatility import needed (UDFs disabled)
// removed unused ScalarValue
use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::DataType as ArrowDataType;
// removed unused FunctionRegistry
// removed unused AmazonS3Builder
// removed unused Url
use datafusion::datasource::MemTable;
// removed unused ViewTable
use arrow::ipc::reader::StreamReader;
use datafusion::arrow::util::pretty::pretty_format_batches;
use crate::sql::tui::{QueryEditorView, QueryEditorConfig};
use std::sync::mpsc;
use crate::buffer::segment_file::SegmentFile;
use std::io::{Seek, Read};
use crate::ingest_work::Ingest;
use crate::ARROW_SCHEMA;
use arc_swap::ArcSwap;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser as StdSqlParser;
use sqlparser::ast::{Statement as StdStatement, SetExpr, TableFactor, Query as StdQuery, Select as StdSelect, SelectItem as StdSelectItem, Expr as StdExpr, Ident, Function, FunctionArg, FunctionArgExpr, ObjectName as SqlObjectName, GroupByExpr};
// removed unused HashSet
// removed unused ProvideCredentials
// removed unused ArrowRecordBatch
// removed unused ArrowSchema2 / ArrowField
// removed unused Client

// S3 object store registration moved to crate::sql::tables

pub async fn register_catalog(ctx: &SessionContext) {
    // Delegate to S3-only registry-backed builder
    crate::sql::metadata::register_catalog(ctx).await;
}

// WalReader abstraction handles WAL batch loading; helpers removed.

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

fn print_sql_error_plain<E: std::fmt::Display>(err: &E) {
    let msg = err.to_string();
    // DataFusion commonly reports: SchemaError(FieldNotFound { field: Column { relation: None, name: "time" }, valid_fields: [...] })
    if msg.contains("FieldNotFound") && msg.contains("valid_fields") {
        // Try to extract missing field name
        let missing = if let Some(start) = msg.find("name: \"") { let s = start + 7; if let Some(end) = msg[s..].find("\"") { &msg[s..s+end] } else { "" } } else { "" };
        // Extract valid field names within brackets
        let hint = if let Some(vs) = msg.find("valid_fields: [") {
            let s = vs + 15; if let Some(end) = msg[s..].find("]") { msg[s..s+end].to_string() } else { String::new() }
        } else { String::new() };
        if !missing.is_empty() {
            eprintln!("ERROR: column \"{}\" does not exist", missing);
        } else {
            eprintln!("ERROR: column does not exist");
        }
        if !hint.is_empty() {
            // Simplify hint list: pull out names=... substrings
            let mut cols: Vec<String> = Vec::new();
            for seg in hint.split("Column ") {
                if let Some(npos) = seg.find("name: ") {
                    let part = &seg[npos + 6..];
                    let trimmed = part.trim();
                    if trimmed.starts_with("\"") {
                        if let Some(endq) = trimmed[1..].find("\"") { cols.push(trimmed[1..1+endq].to_string()); }
                    }
                }
            }
            if !cols.is_empty() {
                eprintln!("HINT: available columns are: {}", cols.join(", "));
            }
        }
    } else {
        eprintln!("ERROR: {}", msg);
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
                    let session_config = SessionConfig::new();
                    let ctx = SessionContext::new_with_config(session_config);
                    // Set pipeline context BEFORE any config that might create dirs
                    PIPELINE_NAME.write().clear(); PIPELINE_NAME.write().push_str(&pipeline_name); Config::init().await;
                    register_catalog(&ctx).await;
                    // Unified WAL reader (local disk or S3 based on manifest/env)
                    let reader = crate::buffer::wal_store::WalReaderFactory::for_pipeline_async(&pipeline_name).await;
                    let wal_batches: Vec<RecordBatch> = reader.load_committed_batches(&pipeline_name, 64).unwrap_or_default();

                    if !wal_batches.is_empty() {
                        let schema = wal_batches[0].schema();
                        let filtered: Vec<RecordBatch> = wal_batches.into_iter().filter(|b| b.schema().as_ref() == schema.as_ref()).collect();
                        if let Ok(mem) = MemTable::try_new(schema.clone(), vec![filtered]).map_err(|e| DataFusionError::Internal(e.to_string())) {
                            let _ = ctx.register_table(&pipeline_name, Arc::new(mem));
                            if let Ok(df) = ctx.sql(&run_sql).await { if let Ok(b) = df.collect().await { let _ = tx_res.send(b); } }
                        } else { let _ = tx_res.send(Vec::new()); }
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
        Ok(Statement::ShowStats { pipeline, namespace }) => {
            let ctx = SessionContext::new();
            let pipeline = pipeline.replace('"', "");
            let _ = show_stats(&ctx, &pipeline, namespace.as_deref()).await;
            return;
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
            match drop_table(&mut skippr_metadata, &stmt).await {
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
            let session_config = SessionConfig::new();

            // Special-case: SHOW STATS FOR <pipeline>
            {
                let trimmed = sql_str.trim();
                let upper = trimmed.to_uppercase();
                if upper.starts_with("SHOW STATS FOR ") {
                    let name = trimmed["SHOW STATS FOR ".len()..].trim();
                    let ns = name.trim_matches('`').trim_matches('"');
                    // Legacy stats removed; use SHOW CATALOG/SHOW SEMANTIC instead
                    return;
                }
            }

            let ctx = SessionContext::new_with_config(session_config);

            // UDFs omitted in this build

            // Register catalog tables after pipeline context is established below

            // Early handle SHOW SEMANTIC / SHOW CATALOG without requiring FROM inference
            if let Ok(mut sp) = SParser::new(sql_str) {
                if let Ok(stmt) = sp.parse_statement() {
                    match stmt {
                        Statement::ShowStats { pipeline, namespace } => {
                            // Establish pipeline context; default namespace to pipeline if not provided
                            PIPELINE_NAME.write().clear(); PIPELINE_NAME.write().push_str(&pipeline);
                            Config::init().await;
                            let ns = namespace.clone().unwrap_or_else(|| pipeline.clone());
                            println!("Stats are embedded in catalog; use SHOW CATALOG or query catalog table.");
                            return;
                        }
                        Statement::ShowSemantic { pipeline, namespace } => {
                            // Establish pipeline context (pipeline is the dataset); namespace may further scope
                            PIPELINE_NAME.write().clear(); PIPELINE_NAME.write().push_str(&pipeline);
                            Config::init().await;
                            register_catalog(&ctx).await;
                            let _ = show_semantic(&ctx, namespace.as_deref()).await; return;
                        }
                        Statement::ShowCatalog { pipeline, namespace } => {
                            PIPELINE_NAME.write().clear(); PIPELINE_NAME.write().push_str(&pipeline);
                            Config::init().await;
                            register_catalog(&ctx).await;
                            let _ = show_catalog(&ctx, namespace.as_deref()).await; return;
                        }
                        _ => {}
                    }
                }
            }

            // Build union views ONLY for tables referenced in this SQL
            let original_pipeline = Config::get_pipeline_name();
            let dialect = GenericDialect {};
            #[derive(Clone)]
            struct TableRef { pipeline: String, namespace: String }
            fn split_pipeline_ns(name: &sqlparser::ast::ObjectName, _default_pipeline: &str) -> TableRef {
                let parts: Vec<String> = name.0.iter().map(|id| id.value.clone()).collect();
                match parts.as_slice() {
                    // Fully-qualified: pipeline.namespace
                    [p, n] => TableRef { pipeline: p.clone(), namespace: n.clone() },
                    // Unqualified: will be rejected by validation later; use placeholders
                    [single] => TableRef { pipeline: String::new(), namespace: single.clone() },
                    _ => {
                        let s = name.to_string();
                        TableRef { pipeline: String::new(), namespace: s }
                    }
                }
            }
            let mut table_refs: Vec<TableRef> = Vec::new();
            if let Ok(ast) = StdSqlParser::parse_sql(&dialect, sql_str) {
                for stmt in ast {
                    if let StdStatement::Query(q) = stmt {
                        match &*q.body {
                            SetExpr::Select(sel) => {
                                for twj in &sel.from {
                                    if let TableFactor::Table { name, .. } = &twj.relation {
                                        let t = split_pipeline_ns(name, &original_pipeline);
                                        table_refs.push(t);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            if table_refs.is_empty() { println!("Could not infer table name from query; expected FROM <pipeline>.<namespace>"); process::exit(1); }
            // Enforce fully-qualified references only
            if table_refs.iter().any(|t| t.pipeline.is_empty()) {
                println!("ERROR: Unqualified table name detected. Use fully-qualified <pipeline>.<namespace> (e.g., picnic.screen).");
                process::exit(1);
            }
            // dedup
            table_refs.sort_by(|a,b| a.pipeline.cmp(&b.pipeline).then(a.namespace.cmp(&b.namespace)));
            table_refs.dedup_by(|a,b| a.pipeline==b.pipeline && a.namespace==b.namespace);
            // Log resolved table refs
            println!("Resolved table refs: [{}]", table_refs.iter().map(|t| format!("{}.{}", t.pipeline, t.namespace)).collect::<Vec<_>>().join(", "));

            // Strict mode: no special-cases (e.g., deadletters). All tables must be fully-qualified and pre-registered.
            let mut first = true;
            for TableRef { pipeline, namespace } in table_refs {
                // Switch pipeline context for correct local WAL dir and config resolution
                PIPELINE_NAME.write().clear();
                PIPELINE_NAME.write().push_str(&pipeline);
                Config::init().await;
                println!("Context: pipeline='{}' namespace='{}'", pipeline, namespace);
                if first { register_catalog(&ctx).await; first = false; }

                // Bootstrap METADATA and ARROW_SCHEMA like main sync
                match Config::get_metadata().await {
                    Ok(pm) => {
                        let _keys: Vec<String> = pm.metadata.keys().cloned().collect();
                        METADATA.store(Arc::new(pm.clone()));
                        let flatten = Config::get_transform_flatten_events();
                        if pm.metadata.contains_key(&namespace) {
                            match Ingest::prepare_arrow_schema_with_metadata_for_query(&namespace, &pm.metadata, flatten) {
                                Ok(schema) => {
                                    ARROW_SCHEMA.insert(namespace.clone(), ArcSwap::from(schema.clone()));
                                    let _field_list: Vec<String> = schema.fields().iter().map(|f| format!("{}:{:?}", f.name(), f.data_type())).collect();
                                    // println!("Published ARROW_SCHEMA for '{}' (fields={}, {:?})", pipeline, schema.fields().len(), field_list);
                                    println!("Arrow schema ready for namespace='{}' fields={}", namespace, schema.fields().len());
                                },
                                Err(e) => {
                                    println!("Failed to build Arrow schema for '{}': {}", namespace, e);
                                }
                            }
                        } else {
                            // missing namespace; proceed without schema
                            println!("Metadata missing for namespace='{}' (continuing)", namespace);
                        }
                    },
                    Err(_) => {
                        // no metadata; proceed
                        METADATA.store(Arc::new(PipelineMetadata::new()));
                        println!("No pipeline metadata found; proceeding without schema");
                    }
                }

                let _ = crate::sql::tables::register_namespace_view(&ctx, &pipeline, &namespace).await.map_err(|e| {
                    println!("Failed to register namespace view for {}.{}: {}", pipeline, namespace, e);
                    e
                });
            }
            // Restore original pipeline context
            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str(&original_pipeline);

            // Execute the query
            // Build whitelist of timestamp paths from ARROW_SCHEMA for all referenced tables
            let _ts_whitelist: std::collections::HashSet<String> = std::collections::HashSet::new();

            // Rewrite SQL: wrap referenced dotted paths in to_timestamp_millis where in whitelist
            #[allow(dead_code)]
            fn needs_cast_path(idents: &Vec<Ident>, whitelist: &std::collections::HashSet<String>) -> bool {
                if idents.is_empty() { return false; }
                let path = idents.iter().map(|i| i.value.clone()).collect::<Vec<String>>().join(".");
                whitelist.contains(&path)
            }
            #[allow(dead_code)]
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
            #[allow(dead_code)]
            fn rewrite_statement(stmt: &mut StdStatement, whitelist: &std::collections::HashSet<String>) {
                if let StdStatement::Query(q) = stmt {
                    if let StdQuery { body, order_by, limit, .. } = q.as_mut() {
                        match &mut **body {
                            SetExpr::Select(sel) => {
                                let StdSelect { projection, selection, group_by, having, from, .. } = sel.as_mut();
                                // Rewrite two-part table names (pipeline.namespace) -> namespace
                                for twj in from.iter_mut() {
                                    if let TableFactor::Table { name, .. } = &mut twj.relation {
                                        if name.0.len() > 1 {
                                            if let Some(last) = name.0.last().cloned() {
                                                name.0 = vec![last];
                                            }
                                        }
                                    }
                                }
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
            // Use SQL as-is (no legacy rewrite). Tables must be fully-qualified and registered accordingly.
            let rewritten_sql = sql_str.to_string();

            // Short-circuit for non-TUI plain mode
            let plain = match CLI_MODE.read().clone() { Mode::Query(QueryOptions { plain, .. }) => plain, _ => false };
            if plain {
                match ctx.sql(&rewritten_sql).await {
                    Ok(df) => match df.collect().await {
                        Ok(res) => {
                            for batch in &res {
                                let schema = batch.schema();
                                let headers: Vec<String> = schema.fields().iter().map(|f| f.name().to_string()).collect();
                                println!("{}", headers.join(","));
                                let cols = batch.columns().len();
                                for row in 0..batch.num_rows() {
                                    let mut parts: Vec<String> = Vec::with_capacity(cols);
                                    for col in 0..cols {
                                        parts.push(crate::sql::tui::value_to_string(batch.column(col).as_ref(), row));
                                    }
                                    println!("{}", parts.join(","));
                                }
                            }
                        }
                        Err(e) => { print_sql_error_plain(&e); }
                    },
                    Err(e) => { print_sql_error_plain(&e); }
                }
                return;
            }

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
                            let session_config = SessionConfig::new();
                            let ctx = SessionContext::new_with_config(session_config);
                            register_catalog(&ctx).await;
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
                        if let Ok(df) = ctx_clone.sql(&current).await {
                            match df.collect().await {
                                Ok(b) => {
                                    let rows: usize = b.iter().map(|rb| rb.num_rows()).sum();
                                    println!("SELECT collected: batches={} rows={}", b.len(), rows);
                                    let _ = tx_res.send(b);
                                }
                                Err(e) => {
                                    println!("SELECT failed to collect: {}", e);
                                    let _ = tx_res.send(Vec::new());
                                }
                            }
                        } else {
                            let _ = tx_res.send(Vec::new());
                        }
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

            // TUI mode already handled earlier when --watch/editor is active; here we simply pretty print if not plain
            // Nothing else to do; run already printed results in TUI
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

async fn show_stats(ctx: &SessionContext, pipeline: &str, namespace: Option<&str>) -> Result<(), DataFusionError> {
    let ns = pipeline.replace('"', "");
    let sql = match namespace { Some(n) if !n.is_empty() => format!("SHOW STATS FOR {}.{}", ns, n), _ => format!("SHOW STATS FOR \"{}\"", ns) };
    let df = ctx.sql(&sql).await?;
    let batches = df.collect().await?;
    println!("Stats for pipeline '{}':", pipeline);
    for b in &batches { print_batches_plain(b); }
    Ok(())
}

async fn show_semantic(ctx: &SessionContext, namespace: Option<&str>) -> Result<(), DataFusionError> {
    let ns = namespace.unwrap_or("").replace('"', "");
    let sql = if ns.is_empty() {
        "SELECT namespace, field, role FROM semantic ORDER BY namespace, field".to_string()
    } else {
        format!("SELECT namespace, field, role FROM semantic WHERE namespace='{}' ORDER BY field", ns)
    };
    let df = ctx.sql(&sql).await?;
    let batches = df.collect().await?;
    if ns.is_empty() { println!("Semantic data:"); } else { println!("Semantic data for namespace '{}':", ns); }
    for b in &batches { print_batches_plain(&b); }
    Ok(())
}

async fn show_catalog(ctx: &SessionContext, namespace: Option<&str>) -> Result<(), DataFusionError> {
    let ns = namespace.unwrap_or("").replace('"', "");
    // Fetch description from S3 catalog JSON if present
    if !ns.is_empty() {
        let pipeline = Config::get_pipeline_name();
        if let Some(entry) = crate::sql::registry::find_entry(&pipeline, &ns).await {
            if let Ok(val) = crate::helpers::s3::get_json(&entry.catalog_key).await {
                if let Some(d) = val.get("description").and_then(|x| x.as_str()) { if !d.trim().is_empty() { println!("Description: {}", d); } }
            }
        }
    }
    // Show dataset description (if present) and fields
    let sql = if ns.is_empty() {
        "SELECT namespace, entity, field, coalesce(description,'') AS description, coalesce(synonyms,'') AS synonyms FROM catalog ORDER BY namespace, field".to_string()
    } else {
        format!("SELECT namespace, entity, field, coalesce(description,'') AS description, coalesce(synonyms,'') AS synonyms FROM catalog WHERE namespace='{}' ORDER BY field", ns)
    };
    let df = ctx.sql(&sql).await?;
    let batches = df.collect().await?;
    if ns.is_empty() { println!("Catalog data:"); } else { println!("Catalog data for namespace '{}':", ns); }
    for b in &batches { print_batches_plain(&b); }
    Ok(())
}

fn print_batches_plain(batch: &RecordBatch) {
    let schema = batch.schema();
    let headers: Vec<String> = schema.fields().iter().map(|f| f.name().to_string()).collect();
    println!("{}", headers.join(","));
    for row in 0..batch.num_rows() {
        let mut parts: Vec<String> = Vec::with_capacity(headers.len());
        for col in 0..headers.len() {
            parts.push(crate::sql::tui::value_to_string(batch.column(col).as_ref(), row));
        }
        println!("{}", parts.join(","));
    }
}
