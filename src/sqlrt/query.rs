use std::fs::OpenOptions;
use std::io::BufReader;
use std::{fs, process};
// removed unused Write import
use crate::discover::schema_alter;
use crate::discover::{Metadata, PipelineMetadata, SkipprDataType};
use crate::helpers::configuration::Config;
use crate::sqlrt::docs::{print_categorized_sql_docs, SqlDocListingKind};
use crate::sqlrt::operators::drop_table::drop_table;
use crate::sqlrt::operators::dump_schema::dump_schema;
use crate::sqlrt::parser::{PipelineToggle, SParser, Statement};
use crate::sqlrt::session::collect_user_sql;
use crate::METADATA;
use arrow::array::{Array, ArrayRef, Int32Array, StringArray};
use arrow_schema::DataType;
use datafusion::prelude::SessionContext;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::sqlrt::doc_parser::SqlDocParser;
use crate::sqlrt::docs::SqlStatementDoc;
use chrono::DateTime;
use datafusion::error::DataFusionError;
use datafusion::prelude::SessionConfig;
// removed unused SqlIdent
// no Volatility import needed (UDFs disabled)
// removed unused ScalarValue
use datafusion::arrow::array::RecordBatch;
use datafusion::datasource::MemTable;
// removed unused ViewTable
use crate::ingest_work::Ingest;
use crate::sqlrt::tui::{QueryEditorConfig, QueryEditorView};
use crate::ARROW_SCHEMA;
use arc_swap::ArcSwap;
use datafusion::sql::sqlparser::ast::{
    Expr as StdExpr, Function, FunctionArg, FunctionArgExpr, FunctionArgumentList,
    FunctionArguments, GroupByExpr, Ident, ObjectName as SqlObjectName, OrderByKind,
    Query as StdQuery, Select as StdSelect, SelectItem as StdSelectItem, SetExpr,
    Statement as StdStatement, TableFactor,
};
use datafusion::sql::sqlparser::dialect::GenericDialect;
use datafusion::sql::sqlparser::parser::Parser as StdSqlParser;
use std::sync::mpsc;
// removed unused HashSet
// removed unused ProvideCredentials
// removed unused ArrowRecordBatch
// removed unused ArrowSchema2 / ArrowField
// removed unused Client

// S3 object store registration moved to crate::sql::tables

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryExecutionMode {
    Query,
    Sync,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryExecutionOptions {
    pub mode: QueryExecutionMode,
    pub plain: bool,
    pub watch: Option<u64>,
}

async fn apply_alter_table(
    config: &Config,
    stmt: &crate::sqlrt::parser::AlterTableStatement,
) -> Result<String, String> {
    let mut skippr_metadata = config
        .get_metadata()
        .await
        .map_err(|_| format!("No existing schema for {}", stmt.pipeline))?;

    if skippr_metadata.enabled {
        return Err(format!(
            "Pipeline '{}' must be DISABLED before ALTER TABLE",
            stmt.pipeline
        ));
    }

    let namespace = stmt
        .namespace
        .as_ref()
        .map(|n| format!("{n}"))
        .unwrap_or_else(|| format!("{}", stmt.pipeline));
    let metadata = skippr_metadata
        .metadata
        .get_mut(&namespace)
        .ok_or_else(|| {
            format!(
                "Schema '{namespace}' not found for pipeline '{}'",
                stmt.pipeline
            )
        })?;
    let field_id = schema_alter::field_id_for_op(metadata, &stmt.op).map_err(|e| e.to_string())?;
    schema_alter::apply(metadata, &stmt.op).map_err(|e| e.to_string())?;
    crate::sqlrt::iceberg_alter::commit_schema_alter(
        config,
        &format!("{}", stmt.pipeline),
        &namespace,
        &stmt.op,
        field_id,
    )
    .await?;

    METADATA.store(Arc::new(skippr_metadata.clone()));
    config.persist_pipeline_metadata(&skippr_metadata).await?;
    Ok(format!(
        "ALTER TABLE {}.{namespace} {:?}",
        stmt.pipeline, stmt.op
    ))
}

async fn apply_pipeline_toggle(
    config: &Config,
    stmt: &crate::sqlrt::parser::PipelineToggleStatement,
) -> Result<String, String> {
    let bound = config.bind_pipeline(format!("{}", &stmt.pipeline).as_str());
    bound.init().await;
    let mut metadata = bound
        .get_metadata()
        .await
        .map_err(|_| format!("Pipeline '{}' not found", stmt.pipeline))?;
    metadata.enabled = match stmt.toggle {
        PipelineToggle::Enable => true,
        PipelineToggle::Disable => false,
    };
    METADATA.store(Arc::new(metadata.clone()));
    bound.set_metadata(&metadata, false).await;
    Ok(format!(
        "Toggled pipeline '{}' to: {}d",
        stmt.pipeline, stmt.toggle
    ))
}

pub fn sql_uses_record_batch_collect(sql: &str) -> bool {
    let upper = sql.trim().to_uppercase();
    !(upper.starts_with("SHOW ")
        || upper.starts_with("STREAM ")
        || upper.starts_with("ENABLE ")
        || upper.starts_with("DISABLE ")
        || upper.starts_with("SCHEMA ")
        || upper.starts_with("PIPELINE ")
        || upper.starts_with("DROP ")
        || upper.starts_with("ALTER ")
        || upper.starts_with("RESET ")
        || upper.starts_with("DESCRIBE ")
        || upper.starts_with("DESC ")
        || upper.starts_with("LOAD "))
}

pub fn sql_uses_shared_extension_collect(sql: &str) -> bool {
    let upper = sql.trim().to_uppercase();
    upper.starts_with("ALTER ") || upper.starts_with("ENABLE ") || upper.starts_with("DISABLE ")
}

impl Default for QueryExecutionOptions {
    fn default() -> Self {
        Self {
            mode: QueryExecutionMode::Query,
            plain: false,
            watch: None,
        }
    }
}

// Build a SessionContext and pre-register all pipelines/namespaces so two-part names resolve
pub async fn new_context_all_namespaces(
    config: &Config,
) -> Result<SessionContext, DataFusionError> {
    let ctx = crate::sqlrt::session::build_query_context(SessionConfig::new());
    let pipelines = crate::sqlrt::registry::list_pipelines(&config).await;
    for pipeline in pipelines {
        let mut namespaces = crate::sqlrt::registry::list_namespaces(&config, &pipeline).await;
        namespaces.sort();
        for ns in namespaces {
            // Keep sqlrt self-contained: register DataFusion views directly via sqlrt tables.
            crate::sqlrt::tables::register_namespace_view(&ctx, config, &pipeline, &ns).await?;
        }
        let _ = crate::sqlrt::tables::register_deadletters(&ctx, config, &pipeline).await;
    }
    crate::sqlrt::tables::register_user_namespaces(&ctx, config).await?;
    Ok(ctx)
}

/// Iceberg `namespace.table` only — the lake contract for `skipprd serve` Flight.
/// Does not register `pipeline.namespace` Iceberg ∪ WAL ingest aliases.
pub async fn new_context_iceberg_namespaces(
    config: &Config,
) -> Result<SessionContext, DataFusionError> {
    let ctx = crate::sqlrt::session::build_query_context(SessionConfig::new());
    crate::sqlrt::tables::register_user_namespaces(&ctx, config).await?;
    Ok(ctx)
}

pub async fn register_catalog(config: &Config, ctx: &SessionContext) {
    // Delegate to S3-only registry-backed builder
    crate::sqlrt::metadata::register_catalog(&config, ctx).await;
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
                    (Some(start_date), Some(end_date)) => {
                        Some((end_date - start_date).num_days() as i32)
                    }
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
        Err(DataFusionError::Internal(
            "Expected StringArray".to_string(),
        ))
    }
}

fn print_sql_error_plain<E: std::fmt::Display>(err: &E) {
    let msg = err.to_string();
    // DataFusion commonly reports: SchemaError(FieldNotFound { field: Column { relation: None, name: "time" }, valid_fields: [...] })
    if msg.contains("FieldNotFound") && msg.contains("valid_fields") {
        // Try to extract missing field name
        let missing = if let Some(start) = msg.find("name: \"") {
            let s = start + 7;
            if let Some(end) = msg[s..].find("\"") {
                &msg[s..s + end]
            } else {
                ""
            }
        } else {
            ""
        };
        // Extract valid field names within brackets
        let hint = if let Some(vs) = msg.find("valid_fields: [") {
            let s = vs + 15;
            if let Some(end) = msg[s..].find("]") {
                msg[s..s + end].to_string()
            } else {
                String::new()
            }
        } else {
            String::new()
        };
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
                        if let Some(endq) = trimmed[1..].find("\"") {
                            cols.push(trimmed[1..1 + endq].to_string());
                        }
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

pub async fn query(config: &Config, sql_str: &str) {
    query_with_options(&config, sql_str, QueryExecutionOptions::default()).await;
}

fn extension_message_batch(message: &str) -> Vec<arrow::array::RecordBatch> {
    let schema = Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new(
        "message",
        DataType::Utf8,
        false,
    )]));
    let batch = arrow::record_batch::RecordBatch::try_new(
        schema,
        vec![Arc::new(StringArray::from(vec![message.to_string()])) as ArrayRef],
    )
    .expect("extension message batch");
    vec![batch]
}

pub async fn query_collect(
    config: &Config,
    sql_str: &str,
) -> std::io::Result<Vec<arrow::array::RecordBatch>> {
    config.init().await;
    if !sql_uses_record_batch_collect(sql_str) {
        let mut parser = SParser::new(sql_str).map_err(|e| std::io::Error::other(e.to_string()))?;
        match parser
            .parse_statement()
            .map_err(|e| std::io::Error::other(e.to_string()))?
        {
            Statement::AlterTable(stmt) => {
                let bound = config.bind_pipeline(format!("{}", &stmt.pipeline).as_str());
                bound.init().await;
                let message = apply_alter_table(&bound, &stmt)
                    .await
                    .map_err(std::io::Error::other)?;
                return Ok(extension_message_batch(&message));
            }
            Statement::PipelineToggle(stmt) => {
                let message = apply_pipeline_toggle(config, &stmt)
                    .await
                    .map_err(std::io::Error::other)?;
                return Ok(extension_message_batch(&message));
            }
            other => {
                return Err(std::io::Error::other(format!(
                    "Session.query does not run {other:?}; use skipprd query for this statement"
                )));
            }
        }
    }
    let ctx = new_context_all_namespaces(&config)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let df = ctx
        .sql(sql_str)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    collect_user_sql(df)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
}

pub async fn query_with_options(
    config: &Config,
    sql_str: &str,
    query_options: QueryExecutionOptions,
) {
    let mut config = config.clone();
    let sql_trim = sql_str.trim();
    if sql_uses_shared_extension_collect(sql_trim) {
        match query_collect(&config, sql_trim).await {
            Ok(batches) => match record_batches_to_plain_query(&batches) {
                Ok(doc) => {
                    if let Some(message) = doc.rows.first().and_then(|row| row.first()) {
                        println!("{message}");
                    }
                }
                Err(err) => println!("{err}"),
            },
            Err(err) => {
                println!("{err}");
                process::exit(1);
            }
        }
        return;
    }
    // Enforce fully-qualified table names: require <pipeline>.<namespace>, forbid default.*
    {
        let is_extension = !sql_uses_record_batch_collect(sql_trim);
        if !is_extension {
            let lower = sql_trim.to_lowercase();
            if lower.contains(" default.") {
                println!("Error: default.* schema is not allowed. Use <pipeline>.<namespace> (e.g., picnic.screen).");
                return;
            }
            // Require at least one fully-qualified <pipeline>.<namespace> present
            let mut allowed: Vec<String> = Vec::new();
            let pipes = crate::sqlrt::registry::list_pipelines(&config).await;
            for p in pipes {
                let nss = crate::sqlrt::registry::list_namespaces(&config, &p).await;
                for ns in nss {
                    allowed.push(format!("{}.{}", p, ns));
                }
            }
            let has_any_allowed = allowed.iter().any(|fqn| lower.contains(fqn));
            if !has_any_allowed {
                println!("Error: All tables must be referenced as <pipeline>.<namespace> (e.g., picnic.screen).\nAllowed datasets: {}", allowed.join(", "));
                return;
            }
        }
    }
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
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let num_start = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > num_start {
                    if let Ok(sec) = raw_after[num_start..j].parse::<i64>() {
                        window_secs_opt = Some(sec);
                    }
                    // remove the WINDOW <n> segment only, preserving a space boundary
                    let left = raw_after[..idx].trim_end();
                    let right = raw_after[j..].trim_start();
                    raw_after = if right.is_empty() {
                        left.to_string()
                    } else {
                        format!("{} {}", left, right)
                    };
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
                                if let TableFactor::Table { name, .. } = &twj.relation {
                                    table_opt = Some(name.to_string());
                                }
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
                    if ch.is_alphanumeric() || ch == '_' || ch == '-' {
                        name.push(ch);
                    } else {
                        break;
                    }
                }
                if !name.is_empty() {
                    table_opt = Some(name);
                }
            }
        }
        let pipeline = match table_opt {
            Some(t) => t,
            None => {
                println!("STREAM requires a FROM <pipeline_name>");
                return;
            }
        };

        // If WINDOW provided, append a time predicate using best-known time column
        if let Some(win_s) = window_secs_opt {
            // Determine time column: prefer event_date; else metadata.prcd_micro_time
            let mut time_col = "event_date".to_string();
            if let Some(swap) = ARROW_SCHEMA.get(&pipeline) {
                let schema = swap.load();
                let has_event = schema.fields().iter().any(|f| f.name() == "event_date");
                if !has_event {
                    time_col = "metadata.prcd_micro_time".to_string();
                }
            }
            let now_ms = chrono::Utc::now().timestamp_millis();
            let lower_ms = now_ms.saturating_sub(win_s.saturating_mul(1000));
            // naive append; if existing WHERE present, add AND; else add WHERE
            let upper = select_sql.to_uppercase();
            if upper.contains(" WHERE ") {
                select_sql = format!(
                    "{} AND {} >= to_timestamp_millis({})",
                    select_sql, time_col, lower_ms
                );
            } else {
                // place predicate before ORDER/GROUP/LIMIT if present
                let mut insert_idx = select_sql.len();
                for kw in [" GROUP BY ", " ORDER BY ", " LIMIT "] {
                    if let Some(i) = upper.find(kw) {
                        insert_idx = insert_idx.min(i);
                    }
                }
                if insert_idx < select_sql.len() {
                    let (head, tail) = select_sql.split_at(insert_idx);
                    select_sql = format!(
                        "{} WHERE {} >= to_timestamp_millis({}){}",
                        head, time_col, lower_ms, tail
                    );
                } else {
                    select_sql = format!(
                        "{} WHERE {} >= to_timestamp_millis({})",
                        select_sql, time_col, lower_ms
                    );
                }
            }
            // window applied; results may be empty if no recent data
        }

        // STREAM editor: editable SQL (STREAM ... or SELECT ...), background refresher pulls WAL-only
        let (tx_req, rx_req) = mpsc::channel::<String>();
        let (tx_res, rx_res) = mpsc::channel::<Vec<RecordBatch>>();
        let initial_stream_sql = select_sql.clone();
        let stream_config = config.clone();
        tokio::spawn(async move {
            let mut config = stream_config;
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
                        let start = idx + 8;
                        let bytes = raw_after.as_bytes();
                        let mut j = start;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        let num_start = j;
                        while j < bytes.len() && bytes[j].is_ascii_digit() {
                            j += 1;
                        }
                        if j > num_start {
                            let left = raw_after[..idx].trim_end().to_string();
                            let right = raw_after[j..].trim_start().to_string();
                            raw_after = if right.is_empty() {
                                left
                            } else {
                                format!("{} {}", left, right)
                            };
                        }
                    }
                    let select_sql = format!("SELECT {}", raw_after);
                    let dialect = GenericDialect {};
                    let mut table_opt: Option<String> = None;
                    if let Ok(ast) = StdSqlParser::parse_sql(&dialect, &select_sql) {
                        for stmt in ast {
                            if let StdStatement::Query(q) = stmt {
                                if let SetExpr::Select(sel) = &*q.body {
                                    if let Some(twj) = sel.from.get(0) {
                                        if let TableFactor::Table { name, .. } = &twj.relation {
                                            table_opt = Some(name.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    table_opt.map(|p| (p, select_sql))
                } else {
                    // SELECT ...; extract table for WAL scope
                    let dialect = GenericDialect {};
                    let mut table_opt: Option<String> = None;
                    if let Ok(ast) = StdSqlParser::parse_sql(&dialect, s) {
                        for stmt in ast {
                            if let StdStatement::Query(q) = stmt {
                                if let SetExpr::Select(sel) = &*q.body {
                                    if let Some(twj) = sel.from.get(0) {
                                        if let TableFactor::Table { name, .. } = &twj.relation {
                                            table_opt = Some(name.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    table_opt.map(|p| (p, s.to_string()))
                }
            };

            let mut current_sql = initial_stream_sql.clone();
            loop {
                if let Some((pipeline_name, run_sql)) = build_plan(&current_sql) {
                    let ctx = crate::sqlrt::session::build_query_context(SessionConfig::new());
                    // Set pipeline context BEFORE any config that might create dirs
                    config = config.bind_pipeline(&pipeline_name);
                    config.init().await;
                    register_catalog(&config, &ctx).await;
                    // Unified WAL reader (local disk or S3 based on manifest/env)
                    let reader = crate::sqlrt::wal_reader::WalReaderFactory::for_pipeline_async(
                        &config,
                        &pipeline_name,
                    )
                    .await;
                    let wal_batches: Vec<RecordBatch> = reader
                        .load_committed_batches(&pipeline_name, usize::MAX)
                        .unwrap_or_default();

                    if !wal_batches.is_empty() {
                        let schema = wal_batches[0].schema();
                        let filtered: Vec<RecordBatch> = wal_batches
                            .into_iter()
                            .filter(|b| b.schema().as_ref() == schema.as_ref())
                            .collect();
                        if let Ok(mem) = MemTable::try_new(schema.clone(), vec![filtered])
                            .map_err(|e| DataFusionError::Internal(e.to_string()))
                        {
                            let _ = ctx.register_table(&pipeline_name, Arc::new(mem));
                            if let Ok(df) = ctx.sql(&run_sql).await {
                                if let Ok(b) = df.collect().await {
                                    let _ = tx_res.send(b);
                                }
                            }
                        } else {
                            let _ = tx_res.send(Vec::new());
                        }
                    } else {
                        let _ = tx_res.send(Vec::new());
                    }
                } else {
                    let _ = tx_res.send(Vec::new());
                }

                // poll for updated SQL (user edits)
                let mut waited = 0u64;
                while waited < 2000 {
                    if let Ok(new_sql) = rx_req.try_recv() {
                        current_sql = new_sql;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    waited += 200;
                }
            }
        });

        QueryEditorView::new(&select_sql).run(
            &config,
            QueryEditorConfig {
                title: &format!("STREAM {}", pipeline),
                footer: Some("Enter:run q:quit"),
                initial_sql: &select_sql,
            },
            rx_res,
            tx_req,
        );
        return;
    }

    let mut parser = SParser::new(sql_str).unwrap();

    match parser.parse_statement() {
        Ok(Statement::ShowDocs) => {
            // Print SQL documentation
            println!("SQL Documentation:");
            println!("=================\n");
            print_categorized_sql_docs(SqlDocListingKind::Full);
            println!("For more detailed documentation, run:");
            println!("  skipprd sql-help");
        }
        Ok(Statement::DatabaseDrop(stmt)) => {
            let db_name = stmt.database.clone();
            config.init().await;
            match crate::sqlrt::operators::drop_table::drop_database(&config, &db_name.to_string())
                .await
            {
                Ok(()) => {
                    println!("Dropped Database: {}", db_name);
                }
                Err(e) => {
                    println!("Failed to drop database: {}", e);
                }
            }
        }
        Ok(Statement::ShowStats {
            pipeline,
            namespace,
        }) => {
            let pipeline = pipeline.replace('"', "");
            config = config.bind_pipeline(&pipeline);
            config.init().await;
            let ns = namespace.as_deref().unwrap_or(&pipeline);
            show_stats(&config, &pipeline, ns).await;
            return;
        }
        Ok(Statement::PipelineDrop(stmt)) => {
            config = config.bind_pipeline(format!("{}", &stmt.pipeline).as_str());
            config.init().await;

            let data_dir = config.get_data_dir();
            let pipeline_name = config.get_pipeline_name();

            println!("Dropping all schemas and data for: {}", pipeline_name);

            match query_options.mode {
                QueryExecutionMode::Sync => {
                    let _ = fs::remove_dir_all(&data_dir)
                        .expect(format!("Failed to remove dir: {}", data_dir).as_str());
                    config.delete_metadata().await;
                    println!("Dropped Pipeline");
                }
                QueryExecutionMode::Query => match config.get_metadata().await {
                    Ok(_metadata) => {
                        let mut empty_pipeline_metadata = PipelineMetadata::new(&config);
                        empty_pipeline_metadata.append_sql(sql_str.to_string());

                        config.set_metadata(&empty_pipeline_metadata, false).await;

                        println!("Done. Pipeline will drop on next sync run");
                    }
                    Err(_e) => {
                        println!("No metadata found for pipeline: {}", pipeline_name);
                    }
                },
            }
        }
        Ok(Statement::PipelineReset(stmt)) => {
            config = config.bind_pipeline(format!("{}", &stmt.pipeline).as_str());
            config.init().await;

            let data_dir = config.get_data_dir();
            let pipeline_name = config.get_pipeline_name();

            println!(
                "Resetting offset database and purging WAL files for pipeline: {}, dir: {}",
                pipeline_name, data_dir
            );

            let mut metadata = config
                .get_metadata()
                .await
                .expect(format!("No metadata found for pipeline: {}", pipeline_name).as_str());

            match query_options.mode {
                QueryExecutionMode::Sync => {
                    let mut tries = 15;
                    let mut _delete = true;
                    while _delete {
                        // retries as workaround for https://github.com/rust-lang/rust/issues/29497
                        match fs::remove_dir_all(&data_dir) {
                            Ok(_) => {
                                _delete = false;
                            }
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
                    config.set_metadata(&metadata, false).await;
                }
                QueryExecutionMode::Query => {
                    metadata.append_sql(sql_str.to_string());

                    config.set_metadata(&metadata, false).await;

                    println!("Done. Pipeline will reset on next sync run");
                }
            }
        }
        Ok(Statement::PipelineToggle(_)) => {
            unreachable!("ENABLE/DISABLE routed through query_collect");
        }
        Ok(Statement::SchemaDrop(stmt)) => {
            config = config.bind_pipeline(format!("{}", &stmt.pipeline).as_str());
            config.init().await;

            let mut metadata = config
                .get_metadata()
                .await
                .expect(format!("No metadata found for pipeline: {}", stmt.pipeline).as_str());

            let schema = stmt.schema.clone().unwrap_or(stmt.pipeline.clone());

            match metadata.metadata.remove(&format!("{}", schema)) {
                Some(_) => {
                    println!("Dropping schema: '{}' for pipeline: '{}', on next sync schema will be re-discovered", schema, &stmt.pipeline);
                    METADATA.store(Arc::new(metadata.clone()));
                    config.set_metadata(&metadata, true).await;
                    println!("Dropped Schema, on next sync schema will be re-discovered");
                }
                None => {
                    println!(
                        "No schema: '{}' found for pipeline: '{}'",
                        schema, &stmt.pipeline
                    );
                }
            }
        }

        Ok(Statement::SchemaLoad(stmt)) => {
            config = config.bind_pipeline(format!("{}", &stmt.pipeline).as_str());
            config.init().await;

            let mut skippr_metadata = match config.get_metadata().await {
                Ok(metadata) => metadata,
                Err(_e) => PipelineMetadata::new(&config),
            };

            let source_path = if std::path::Path::new(&stmt.source).is_absolute() {
                stmt.source.clone()
            } else {
                let data_dir = config.get_data_dir();
                format!("{}/{}", data_dir, stmt.source)
            };

            let file = match OpenOptions::new().read(true).open(&source_path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("Failed to open schema file '{}': {}", source_path, e);
                    return;
                }
            };
            let reader = BufReader::new(file);
            let schema_file: serde_json::Value = match serde_json::from_reader(reader) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Failed to parse schema JSON: {}", e);
                    return;
                }
            };

            let tables = match schema_file.get("tables").and_then(|t| t.as_array()) {
                Some(t) => t,
                None => {
                    eprintln!("Schema JSON missing 'tables' array");
                    return;
                }
            };

            for table in tables {
                let namespace = match table.get("skippr_namespace").and_then(|n| n.as_str()) {
                    Some(n) => n.to_string(),
                    None => {
                        eprintln!("Table entry missing 'skippr_namespace'");
                        continue;
                    }
                };
                let columns = match table.get("columns").and_then(|c| c.as_array()) {
                    Some(c) => c,
                    None => {
                        eprintln!("Table '{}' missing 'columns' array", namespace);
                        continue;
                    }
                };

                let mut ns_metadata = Metadata::new().unwrap();
                for col in columns {
                    let col_name = match col.get("name").and_then(|n| n.as_str()) {
                        Some(n) => n.to_string(),
                        None => continue,
                    };
                    let col_type_str = col
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("VARCHAR");
                    let skippr_type = map_destination_type_to_skippr(col_type_str);
                    let field_meta = Metadata::new_with_type(skippr_type, &col_name);
                    ns_metadata.set_field(&col_name, field_meta);
                }

                skippr_metadata
                    .metadata
                    .insert(namespace.clone(), ns_metadata);
            }

            METADATA.store(Arc::new(skippr_metadata.clone()));
            config.set_metadata(&skippr_metadata, true).await;

            let count = tables.len();
            println!("Schema loaded: {} namespace(s) updated.", count);
        }
        Ok(Statement::SchemaDump(stmt)) => {
            config = config.bind_pipeline(format!("{}", &stmt.pipeline).as_str());
            config.init().await;
            let _workspace = config.get_workspace_name();

            let skippr_metadata = match config.get_metadata().await {
                Ok(metadata) => metadata,
                Err(_e) => {
                    println!("No existing schema for pipeline '{}'", stmt.pipeline);
                    return;
                }
            };

            let schema_name = stmt.schema.clone().unwrap_or(stmt.pipeline.clone());
            if skippr_metadata
                .metadata
                .get(&format!("{}", schema_name))
                .is_none()
            {
                println!(
                    "Schema '{}' not found for pipeline: '{}'",
                    schema_name, &stmt.pipeline
                );
                return;
            }

            let pipeline = format!("{}", stmt.pipeline);
            let namespace = format!("{}", schema_name);
            let ctx = SessionContext::new();
            if let Err(err) =
                crate::sqlrt::tables::register_namespace_view(&ctx, &config, &pipeline, &namespace)
                    .await
            {
                println!("SCHEMA DUMP failed: {err}");
                return;
            }
            match dump_schema(&ctx, &pipeline, &namespace, &stmt).await {
                Ok(()) => println!("Schema dumped to '{}'", stmt.target),
                Err(err) => println!("SCHEMA DUMP failed: {err}"),
            }
        }
        Ok(Statement::AlterTable(_)) => {
            unreachable!("ALTER TABLE routed through query_collect");
        }
        Ok(Statement::TableDrop(stmt)) => {
            // Get the schema and table names
            let table_str = format!("{}", stmt.table);
            let schema_str = match &stmt.schema {
                Some(schema) => format!("{}", schema),
                None => "".to_string(), // No schema specified
            };

            // Set the pipeline name based on the table or schema
            if schema_str.is_empty() {
                config = config.bind_pipeline(&table_str);
            } else {
                config = config.bind_pipeline(&schema_str);
            }
            config.init().await;

            // Get the current metadata
            let mut skippr_metadata = match config.get_metadata().await {
                Ok(metadata) => metadata,
                Err(_) => {
                    println!("No existing metadata for pipeline");
                    return;
                }
            };

            // Use the drop_table operator to remove the table from metadata
            match drop_table(&config, &mut skippr_metadata, &stmt).await {
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

                    config.set_metadata(&skippr_metadata, false).await;
                    println!("Dropped table: {}", table_str);
                }
                Err(e) => {
                    println!("{}", e);
                }
            }
        }
        // Err(e) => {
        //
        //     println!("Unknown SQL Dialect. {}", e);
        // },
        _ => {
            // Fall back to DataFusion for standard SQL (e.g., SELECT ...)
            // Build a context and register available pipeline table from current data dir
            let ctx = crate::sqlrt::session::build_query_context(SessionConfig::new());

            // Register catalog tables after pipeline context is established below

            // Early handle SHOW SEMANTIC / SHOW CATALOG without requiring FROM inference
            if let Ok(mut sp) = SParser::new(sql_str) {
                if let Ok(stmt) = sp.parse_statement() {
                    match stmt {
                        Statement::ShowStats {
                            pipeline,
                            namespace,
                        } => {
                            let pipeline = pipeline.replace('"', "");
                            config = config.bind_pipeline(&pipeline);
                            config.init().await;
                            let ns = namespace.unwrap_or_else(|| pipeline.clone());
                            show_stats(&config, &pipeline, &ns).await;
                            return;
                        }
                        Statement::ShowSemantic {
                            pipeline,
                            namespace,
                        } => {
                            // Establish pipeline context (pipeline is the dataset); namespace may further scope
                            config = config.bind_pipeline(&pipeline);
                            config.init().await;
                            register_catalog(&config, &ctx).await;
                            let _ = show_semantic(&ctx, namespace.as_deref()).await;
                            return;
                        }
                        Statement::ShowCatalog {
                            pipeline,
                            namespace,
                        } => {
                            config = config.bind_pipeline(&pipeline);
                            config.init().await;
                            register_catalog(&config, &ctx).await;
                            let _ = show_catalog(&config, &ctx, namespace.as_deref()).await;
                            return;
                        }
                        Statement::ShowPipeline { pipeline } => {
                            let pipeline = pipeline.replace('"', "");
                            config = config.bind_pipeline(&pipeline);
                            config.init().await;
                            show_pipeline(&config, &pipeline).await;
                            return;
                        }
                        _ => {}
                    }
                }
            }

            // Build union views ONLY for tables referenced in this SQL
            let original_pipeline = config.get_pipeline_name();
            let dialect = GenericDialect {};
            #[derive(Clone)]
            struct TableRef {
                pipeline: String,
                namespace: String,
            }
            fn split_pipeline_ns(name: &SqlObjectName, _default_pipeline: &str) -> TableRef {
                let parts: Vec<String> = name
                    .0
                    .iter()
                    .filter_map(|part| part.as_ident().map(|id| id.value.clone()))
                    .collect();
                match parts.as_slice() {
                    // Fully-qualified: pipeline.namespace
                    [p, n] => TableRef {
                        pipeline: p.clone(),
                        namespace: n.clone(),
                    },
                    // Unqualified: will be rejected by validation later; use placeholders
                    [single] => TableRef {
                        pipeline: String::new(),
                        namespace: single.clone(),
                    },
                    _ => {
                        let s = name.to_string();
                        TableRef {
                            pipeline: String::new(),
                            namespace: s,
                        }
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
            if table_refs.is_empty() {
                println!(
                    "Could not infer table name from query; expected FROM <pipeline>.<namespace>"
                );
                process::exit(1);
            }
            // Enforce fully-qualified references only
            if table_refs.iter().any(|t| t.pipeline.is_empty()) {
                println!("ERROR: Unqualified table name detected. Use fully-qualified <pipeline>.<namespace> (e.g., picnic.screen).");
                process::exit(1);
            }
            // dedup
            table_refs.sort_by(|a, b| {
                a.pipeline
                    .cmp(&b.pipeline)
                    .then(a.namespace.cmp(&b.namespace))
            });
            table_refs.dedup_by(|a, b| a.pipeline == b.pipeline && a.namespace == b.namespace);
            let plain = query_options.plain;
            if !plain {
                println!(
                    "Resolved table refs: [{}]",
                    table_refs
                        .iter()
                        .map(|t| format!("{}.{}", t.pipeline, t.namespace))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }

            // Strict mode: no special-cases (e.g., deadletters). All tables must be fully-qualified and pre-registered.
            let mut first = true;
            for TableRef {
                pipeline,
                namespace,
            } in table_refs
            {
                // Switch pipeline context for correct local WAL dir and config resolution
                config = config.bind_pipeline(&pipeline);
                config.init().await;
                if !plain {
                    println!("Context: pipeline='{}' namespace='{}'", pipeline, namespace);
                }
                if first {
                    register_catalog(&config, &ctx).await;
                    first = false;
                }

                // Bootstrap METADATA and ARROW_SCHEMA like main sync
                match config.get_metadata().await {
                    Ok(pm) => {
                        let _keys: Vec<String> = pm.metadata.keys().cloned().collect();
                        METADATA.store(Arc::new(pm.clone()));
                        let flatten = config.get_transform_flatten_events();
                        if pm.metadata.contains_key(&namespace) {
                            match Ingest::prepare_arrow_schema_with_metadata_for_query(
                                &namespace,
                                &pm.metadata,
                                flatten,
                            ) {
                                Ok(schema) => {
                                    ARROW_SCHEMA
                                        .insert(namespace.clone(), ArcSwap::from(schema.clone()));
                                    let _field_list: Vec<String> = schema
                                        .fields()
                                        .iter()
                                        .map(|f| format!("{}:{:?}", f.name(), f.data_type()))
                                        .collect();
                                    // println!("Published ARROW_SCHEMA for '{}' (fields={}, {:?})", pipeline, schema.fields().len(), field_list);
                                    if !plain {
                                        println!(
                                            "Arrow schema ready for namespace='{}' fields={}",
                                            namespace,
                                            schema.fields().len()
                                        );
                                    }
                                }
                                Err(e) => {
                                    if !plain {
                                        println!(
                                            "Failed to build Arrow schema for '{}': {}",
                                            namespace, e
                                        );
                                    }
                                }
                            }
                        } else {
                            // missing namespace; proceed without schema
                            if !plain {
                                println!(
                                    "Metadata missing for namespace='{}' (continuing)",
                                    namespace
                                );
                            }
                        }
                    }
                    Err(_) => {
                        // no metadata; proceed
                        METADATA.store(Arc::new(PipelineMetadata::new(&config)));
                        if !plain {
                            println!("No pipeline metadata found; proceeding without schema");
                        }
                    }
                }

                if let Err(e) = crate::sqlrt::tables::register_namespace_view(
                    &ctx, &config, &pipeline, &namespace,
                )
                .await
                {
                    if !plain {
                        println!(
                            "Failed to register namespace view for {}.{}: {}",
                            pipeline, namespace, e
                        );
                    }
                    return;
                }
            }
            // Restore original pipeline context
            config = config.bind_pipeline(&original_pipeline);

            // Execute the query
            // Build whitelist of timestamp paths from ARROW_SCHEMA for all referenced tables
            let _ts_whitelist: std::collections::HashSet<String> = std::collections::HashSet::new();

            // Rewrite SQL: wrap referenced dotted paths in to_timestamp_millis where in whitelist
            #[allow(dead_code)]
            fn needs_cast_path(
                idents: &Vec<Ident>,
                whitelist: &std::collections::HashSet<String>,
            ) -> bool {
                if idents.is_empty() {
                    return false;
                }
                let path = idents
                    .iter()
                    .map(|i| i.value.clone())
                    .collect::<Vec<String>>()
                    .join(".");
                whitelist.contains(&path)
            }
            #[allow(dead_code)]
            fn rewrite_expr(expr: &mut StdExpr, whitelist: &std::collections::HashSet<String>) {
                match expr {
                    StdExpr::CompoundIdentifier(idents) => {
                        if needs_cast_path(idents, whitelist) {
                            let arg = StdExpr::CompoundIdentifier(idents.clone());
                            *expr = StdExpr::Function(Function {
                                name: SqlObjectName::from(Ident::new("to_timestamp_millis")),
                                uses_odbc_syntax: false,
                                parameters: FunctionArguments::None,
                                args: FunctionArguments::List(FunctionArgumentList {
                                    duplicate_treatment: None,
                                    args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))],
                                    clauses: vec![],
                                }),
                                filter: None,
                                null_treatment: None,
                                over: None,
                                within_group: vec![],
                            });
                        }
                    }
                    StdExpr::Identifier(_) | StdExpr::Value(_) | StdExpr::Wildcard(_) => {}
                    StdExpr::BinaryOp { left, right, .. } => {
                        rewrite_expr(left, whitelist);
                        rewrite_expr(right, whitelist);
                    }
                    StdExpr::UnaryOp { expr: inner, .. } => {
                        rewrite_expr(inner, whitelist);
                    }
                    StdExpr::Nested(inner) => {
                        rewrite_expr(inner, whitelist);
                    }
                    StdExpr::Function(f) => {
                        if let FunctionArguments::List(list) = &mut f.args {
                            for a in list.args.iter_mut() {
                                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = a {
                                    rewrite_expr(e, whitelist);
                                }
                            }
                        }
                    }
                    StdExpr::Cast { expr: inner, .. } => {
                        rewrite_expr(inner, whitelist);
                    }
                    _ => {}
                }
            }
            #[allow(dead_code)]
            fn rewrite_statement(
                stmt: &mut StdStatement,
                whitelist: &std::collections::HashSet<String>,
            ) {
                if let StdStatement::Query(q) = stmt {
                    let StdQuery { body, order_by, .. } = q.as_mut();
                    {
                        match &mut **body {
                            SetExpr::Select(sel) => {
                                let StdSelect {
                                    projection,
                                    selection,
                                    group_by,
                                    having,
                                    from,
                                    ..
                                } = sel.as_mut();
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
                                        StdSelectItem::ExprWithAlias { expr, .. } => {
                                            rewrite_expr(expr, whitelist)
                                        }
                                        _ => {}
                                    }
                                }
                                if let Some(e) = selection.as_mut() {
                                    rewrite_expr(e, whitelist);
                                }
                                // group by
                                match group_by {
                                    GroupByExpr::Expressions(exprs, _) => {
                                        for e in exprs.iter_mut() {
                                            rewrite_expr(e, whitelist);
                                        }
                                    }
                                    _ => {}
                                }
                                if let Some(h) = having.as_mut() {
                                    rewrite_expr(h, whitelist);
                                }
                            }
                            _ => {}
                        }
                        if let Some(order_by) = order_by.as_mut() {
                            if let OrderByKind::Expressions(exprs) = &mut order_by.kind {
                                for ob in exprs.iter_mut() {
                                    rewrite_expr(&mut ob.expr, whitelist);
                                }
                            }
                        }
                        // limit is an Expr in this parser version; nothing to rewrite here
                    }
                }
            }
            // Use SQL as-is (no legacy rewrite). Tables must be fully-qualified and registered accordingly.
            let rewritten_sql = sql_str.to_string();

            // Short-circuit for non-TUI plain mode
            if plain {
                match ctx.sql(&rewritten_sql).await {
                    Ok(df) => match collect_user_sql(df).await {
                        Ok(res) => print_query_plain_json(&res),
                        Err(e) => {
                            print_sql_error_plain(&e);
                        }
                    },
                    Err(e) => {
                        print_sql_error_plain(&e);
                    }
                }
                return;
            }

            // SELECT execution: support --watch for live TUI; else one-shot
            let watch_secs = query_options.watch;
            // Unified SELECT TUI editor: editable SQL, runs on Enter or r; if --watch set, periodic refresh
            let initial_sql = rewritten_sql.clone();
            let (tx_req, rx_req) = mpsc::channel::<String>();
            let (tx_res, rx_res) = mpsc::channel::<Vec<RecordBatch>>();
            let ctx_clone = ctx.clone();
            let select_config = config.clone();
            tokio::spawn(async move {
                let mut config = select_config;
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
                            let start = idx + 8;
                            let bytes = raw_after.as_bytes();
                            let mut j = start;
                            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                                j += 1;
                            }
                            let num_start = j;
                            while j < bytes.len() && bytes[j].is_ascii_digit() {
                                j += 1;
                            }
                            if j > num_start {
                                let left = raw_after[..idx].trim_end().to_string();
                                let right = raw_after[j..].trim_start().to_string();
                                raw_after = if right.is_empty() {
                                    left
                                } else {
                                    format!("{} {}", left, right)
                                };
                            }
                        }
                        let select_sql = format!("SELECT {}", raw_after);
                        // Extract pipeline name
                        let dialect = GenericDialect {};
                        let mut table_opt: Option<String> = None;
                        if let Ok(ast) = StdSqlParser::parse_sql(&dialect, &select_sql) {
                            for stmt in ast {
                                if let StdStatement::Query(q) = stmt {
                                    if let SetExpr::Select(sel) = &*q.body {
                                        if let Some(twj) = sel.from.get(0) {
                                            if let TableFactor::Table { name, .. } = &twj.relation {
                                                table_opt = Some(name.to_string());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(pipeline_name) = table_opt {
                            let ctx =
                                crate::sqlrt::session::build_query_context(SessionConfig::new());
                            register_catalog(&config, &ctx).await;
                            config = config.bind_pipeline(&pipeline_name);
                            config.init().await;
                            let reader =
                                crate::sqlrt::wal_reader::WalReaderFactory::for_pipeline_async(
                                    &config,
                                    &pipeline_name,
                                )
                                .await;
                            let wal_batches: Vec<RecordBatch> = reader
                                .load_committed_batches(&pipeline_name, usize::MAX)
                                .unwrap_or_default();
                            if !wal_batches.is_empty() {
                                let schema = wal_batches[0].schema();
                                let filtered: Vec<RecordBatch> = wal_batches
                                    .into_iter()
                                    .filter(|b| b.schema().as_ref() == schema.as_ref())
                                    .collect();
                                if let Ok(mem) = MemTable::try_new(schema.clone(), vec![filtered])
                                    .map_err(|e| DataFusionError::Internal(e.to_string()))
                                {
                                    let _ = ctx.register_table(&pipeline_name, Arc::new(mem));
                                    if let Ok(df) = ctx.sql(&select_sql).await {
                                        if let Ok(b) = df.collect().await {
                                            let _ = tx_res.send(b);
                                        }
                                    }
                                } else {
                                    let _ = tx_res.send(Vec::new());
                                }
                            } else {
                                let _ = tx_res.send(Vec::new());
                            }
                        } else {
                            let _ = tx_res.send(Vec::new());
                        }
                    } else {
                        // SELECT: run against prepared context (S3/union already registered earlier)
                        if let Ok(df) = ctx_clone.sql(&current).await {
                            match collect_user_sql(df).await {
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
                        let mut waited_ms: u64 = 0;
                        let step = 200u64;
                        let total = w.saturating_mul(1000);
                        while waited_ms < total {
                            if let Ok(new_sql) = rx_req.try_recv() {
                                current = new_sql;
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(step)).await;
                            waited_ms = waited_ms.saturating_add(step);
                        }
                    } else {
                        loop {
                            if let Ok(new_sql) = rx_req.try_recv() {
                                current = new_sql;
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                    }
                }
            });
            let footer = if let Some(w) = watch_secs {
                format!("select --watch={}s | Enter:run q:quit", w)
            } else {
                "Enter:run q:quit".to_string()
            };
            QueryEditorView::new(&rewritten_sql).run(
                &config,
                QueryEditorConfig {
                    title: "SELECT",
                    footer: Some(&footer),
                    initial_sql: &rewritten_sql,
                },
                rx_res,
                tx_req,
            );

            // TUI mode already handled earlier when --watch/editor is active; here we simply pretty print if not plain
            // Nothing else to do; run already printed results in TUI
        }
    }
}

#[allow(dead_code)]
fn recurse_paths(
    output_dir: &str,
    table_name: &str,
    ctx: &SessionContext,
    paths: &mut Vec<PathBuf>,
) {
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

async fn show_stats(config: &Config, pipeline: &str, namespace: &str) {
    match config.read_namespace_stats_async(namespace).await {
        Some(val) => {
            println!("{}", serde_json::to_string(&val).unwrap_or_default());
        }
        None => {
            eprintln!(
                "No stats found for namespace '{}' in pipeline '{}'",
                namespace, pipeline
            );
            std::process::exit(1);
        }
    }
}

async fn show_semantic(
    ctx: &SessionContext,
    namespace: Option<&str>,
) -> Result<(), DataFusionError> {
    let ns = namespace.unwrap_or("").replace('"', "");
    let sql = if ns.is_empty() {
        "SELECT namespace, field, role FROM semantic ORDER BY namespace, field".to_string()
    } else {
        format!(
            "SELECT namespace, field, role FROM semantic WHERE namespace='{}' ORDER BY field",
            ns
        )
    };
    let df = ctx.sql(&sql).await?;
    let batches = df.collect().await?;
    print_query_plain_json(&batches);
    Ok(())
}

async fn show_catalog(
    config: &Config,
    ctx: &SessionContext,
    namespace: Option<&str>,
) -> Result<(), DataFusionError> {
    let ns = namespace.unwrap_or("").replace('"', "");
    // Fetch description from S3 catalog JSON if present
    if !ns.is_empty() {
        let pipeline = config.get_pipeline_name();
        if let Some(entry) = crate::sqlrt::registry::find_entry(&config, &pipeline, &ns).await {
            if let Ok(Some(val)) = crate::adapters::storage::get_storage(&config)
                .get_json_opt(&entry.catalog_key)
                .await
            {
                if let Some(d) = val.get("description").and_then(|x| x.as_str()) {
                    if !d.trim().is_empty() {
                        eprintln!("Description: {}", d);
                    }
                }
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
    print_query_plain_json(&batches);
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlainQueryDocument {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

pub fn record_batches_to_plain_query(
    batches: &[RecordBatch],
) -> Result<PlainQueryDocument, String> {
    let Some(first) = batches.first() else {
        return Ok(PlainQueryDocument {
            header: Vec::new(),
            rows: Vec::new(),
        });
    };
    let header: Vec<String> = first
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    let mut rows = Vec::new();
    for batch in batches {
        let names: Vec<String> = batch
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().to_string())
            .collect();
        if names != header {
            return Err(format!(
                "plain query schema mismatch: expected {header:?}, got {names:?}"
            ));
        }
        for row in 0..batch.num_rows() {
            let mut parts: Vec<String> = Vec::with_capacity(header.len());
            for col in 0..header.len() {
                parts.push(crate::sqlrt::tui::value_to_string(
                    batch.column(col).as_ref(),
                    row,
                ));
            }
            rows.push(parts);
        }
    }
    Ok(PlainQueryDocument { header, rows })
}

pub fn print_query_plain_json(batches: &[RecordBatch]) {
    match record_batches_to_plain_query(batches) {
        Ok(doc) => match serde_json::to_string(&doc) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                eprintln!("failed to encode skipprd --plain query JSON: {e}");
                process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("{e}");
            process::exit(1);
        }
    }
}

fn map_destination_type_to_skippr(type_str: &str) -> SkipprDataType {
    match type_str.to_uppercase().as_str() {
        "VARCHAR" | "STRING" | "TEXT" | "NVARCHAR" | "CHAR" | "NCHAR" | "NTEXT" => {
            SkipprDataType::String
        }
        "NUMBER" | "INT" | "INTEGER" | "BIGINT" | "SMALLINT" | "TINYINT" => SkipprDataType::Long,
        "DOUBLE" | "FLOAT" | "REAL" | "NUMERIC" | "DECIMAL" | "MONEY" | "SMALLMONEY" => {
            SkipprDataType::Double
        }
        "BOOLEAN" | "BOOL" | "BIT" => SkipprDataType::Boolean,
        "DATE" => SkipprDataType::Date,
        "TIMESTAMP" | "TIMESTAMP_NTZ" | "TIMESTAMP_LTZ" | "TIMESTAMP_TZ" | "DATETIME"
        | "DATETIME2" | "SMALLDATETIME" | "DATETIMEOFFSET" => SkipprDataType::Timestamp,
        "VARIANT" | "OBJECT" | "ARRAY" => SkipprDataType::String,
        other => {
            tracing::warn!(
                "Unrecognized destination type '{}', defaulting to String",
                other
            );
            SkipprDataType::String
        }
    }
}

async fn show_pipeline(config: &Config, pipeline_name: &str) {
    let pipeline_metadata = match config.get_metadata().await {
        Ok(m) => m,
        Err(_) => {
            let result = serde_json::json!({
                "pipeline": pipeline_name,
                "status": "not_found",
                "namespaces": [],
                "offsets": {},
            });
            println!("{}", serde_json::to_string_pretty(&result).unwrap());
            return;
        }
    };

    let status = if pipeline_metadata.enabled {
        "active"
    } else {
        "disabled"
    };

    let mut namespaces = Vec::new();
    for (ns_name, ns_metadata) in pipeline_metadata.metadata.iter() {
        let fields: Vec<serde_json::Value> = ns_metadata
            .fields
            .iter()
            .map(|(key, field_metadata)| {
                let name = if field_metadata.out_field_name.is_empty() {
                    key.clone()
                } else {
                    field_metadata.out_field_name.clone()
                };
                let source_field_name = if field_metadata.source_field_name.is_empty() {
                    key.clone()
                } else {
                    field_metadata.source_field_name.clone()
                };
                let type_name =
                    if field_metadata.determined_type == crate::discover::SkipprDataType::Unknown {
                        "Unknown".to_string()
                    } else {
                        field_metadata.determined_type.as_str().to_string()
                    };
                serde_json::json!({
                    "name": name,
                    "out_field_name": name,
                    "source_field_name": source_field_name,
                    "type": type_name,
                    "nullable": field_metadata.nullable,
                    "field_id": field_metadata.field_id,
                    "lineage_id": field_metadata.lineage_id,
                })
            })
            .collect();
        namespaces.push(serde_json::json!({
            "name": ns_name,
            "enabled": true,
            "fields": fields,
        }));
    }

    let metadata_location =
        if config.get_storage_mode() == crate::helpers::wal_storage::ElStorageMode::Local {
            format!(
                "{}/{}/{}/{}/metadata/metadata.json",
                config.get_data_dir(),
                config.get_tenant(),
                config.get_workspace_name(),
                pipeline_name,
            )
        } else {
            format!(
                "s3://{}/{}/{}/{}/metadata/metadata.json",
                config.get_skippr_s3_bucket(),
                config.get_tenant(),
                config.get_workspace_name(),
                pipeline_name,
            )
        };

    let result = serde_json::json!({
        "pipeline": pipeline_name,
        "status": status,
        "namespaces": namespaces,
        "offsets": {},
        "metadata_location": metadata_location,
    });

    println!("{}", serde_json::to_string_pretty(&result).unwrap());
}

#[cfg(test)]
mod plain_query_document_tests {
    use super::{record_batches_to_plain_query, PlainQueryDocument};
    use arrow::array::{ArrayRef, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    use datafusion::arrow::array::RecordBatch;
    use std::sync::Arc;

    fn batch(names: &[&str], rows: &[&[&str]]) -> RecordBatch {
        let fields: Vec<Field> = names
            .iter()
            .map(|n| Field::new(*n, DataType::Utf8, false))
            .collect();
        let columns: Vec<ArrayRef> = (0..names.len())
            .map(|col| {
                let values: Vec<&str> = rows.iter().map(|row| row[col]).collect();
                Arc::new(StringArray::from(values)) as ArrayRef
            })
            .collect();
        RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).expect("batch")
    }

    #[test]
    fn plain_query_document_keeps_commas_inside_cells() {
        let batches = [batch(
            &["id", "name"],
            &[&["1", "alice,bob"], &["2", "carol"]],
        )];
        let doc = record_batches_to_plain_query(&batches).expect("doc");
        assert_eq!(
            doc,
            PlainQueryDocument {
                header: vec!["id".into(), "name".into()],
                rows: vec![
                    vec!["1".into(), "alice,bob".into()],
                    vec!["2".into(), "carol".into()]
                ],
            }
        );
        let json = serde_json::to_string(&doc).expect("json");
        assert!(json.contains("alice,bob"));
        assert!(!json.contains("alice,bob\n"));
        let round: PlainQueryDocument = serde_json::from_str(&json).expect("roundtrip");
        assert_eq!(round, doc);
    }

    #[test]
    fn plain_query_document_concatenates_batches_with_one_header() {
        let batches = [batch(&["id"], &[&["1"]]), batch(&["id"], &[&["2"]])];
        let doc = record_batches_to_plain_query(&batches).expect("doc");
        assert_eq!(doc.header, vec!["id"]);
        assert_eq!(doc.rows, vec![vec!["1".to_string()], vec!["2".to_string()]]);
    }

    #[test]
    fn dialect_sql_does_not_use_record_batch_collect() {
        assert!(!super::sql_uses_record_batch_collect(
            "SHOW PIPELINE orders_el"
        ));
        assert!(!super::sql_uses_record_batch_collect(
            "LOAD SCHEMA x INTO y"
        ));
        assert!(!super::sql_uses_record_batch_collect(
            "ALTER TABLE bikehire.trips MERGE COLUMN price_string INTO price"
        ));
        assert!(!super::sql_uses_record_batch_collect(
            "DISABLE PIPELINE bikehire"
        ));
        assert!(super::sql_uses_record_batch_collect(
            "SELECT 1 FROM pipe.ns"
        ));
        assert!(super::sql_uses_shared_extension_collect(
            "ALTER TABLE bikehire.trips MERGE COLUMN price_string INTO price"
        ));
        assert!(super::sql_uses_shared_extension_collect(
            "DISABLE PIPELINE bikehire"
        ));
        assert!(!super::sql_uses_shared_extension_collect(
            "SHOW PIPELINE bikehire"
        ));
    }

    #[test]
    fn query_with_options_does_not_apply_alter_directly() {
        let src = include_str!("query.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let start = prod
            .find("pub async fn query_with_options")
            .expect("query_with_options");
        let body = &prod[start..];
        let body = body
            .split("pub fn print_query_plain_json")
            .next()
            .expect("body before print_query_plain_json");
        assert!(
            body.contains("sql_uses_shared_extension_collect"),
            "CLI ALTER/ENABLE/DISABLE must enter query_collect"
        );
        assert!(
            !body.contains("apply_alter_table("),
            "query_with_options must not call apply_alter_table"
        );
        assert!(
            !body.contains("apply_pipeline_toggle("),
            "query_with_options must not call apply_pipeline_toggle"
        );
    }

    #[test]
    fn alter_table_fails_closed_on_enabled_or_non_iceberg() {
        let src = include_str!("query.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let start = prod
            .find("async fn apply_alter_table")
            .expect("apply_alter_table");
        let body = prod[start..]
            .split("async fn ")
            .nth(1)
            .expect("function body after signature");
        assert!(
            body.contains("must be DISABLED before ALTER TABLE"),
            "enabled pipelines must be rejected"
        );
        assert!(
            include_str!("iceberg_alter.rs").contains("ALTER TABLE requires an Iceberg sink"),
            "non-Iceberg sinks must be rejected by QueryBackend"
        );
        assert!(
            body.contains("persist_pipeline_metadata"),
            "Iceberg commit must be followed by fail-closed metadata persist"
        );
    }

    #[test]
    fn namespace_register_errors_are_not_swallowed() {
        let src = include_str!("query.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let start = prod
            .find("pub async fn new_context_all_namespaces")
            .expect("new_context_all_namespaces");
        let body = prod[start..]
            .split("pub async fn ")
            .nth(1)
            .expect("function body after signature");
        assert!(
            !body
                .contains("let _ =\n                crate::sqlrt::tables::register_namespace_view"),
            "Iceberg catalog/scan failure must fail the query, not leave tables unregistered"
        );
        assert!(
            body.contains("register_namespace_view(&ctx, config, &pipeline, &ns).await?"),
            "query context must propagate Iceberg catalog register errors"
        );
        assert!(
            body.contains("register_user_namespaces(&ctx, config).await?"),
            "user Iceberg namespaces must register as DataFusion schemas"
        );
    }

    #[test]
    fn iceberg_namespace_context_skips_wal_ingest_alias() {
        let src = include_str!("query.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let start = prod
            .find("pub async fn new_context_iceberg_namespaces")
            .expect("new_context_iceberg_namespaces");
        let body = prod[start..]
            .split("pub async fn ")
            .nth(1)
            .expect("function body after signature");
        assert!(
            body.contains("register_user_namespaces(&ctx, config).await?"),
            "serve Flight must register Iceberg namespace.table"
        );
        assert!(
            !body.contains("register_namespace_view"),
            "serve Flight must not register pipeline.namespace Iceberg ∪ WAL"
        );
        assert!(
            !body.contains("register_deadletters"),
            "serve Flight must not register WAL deadletters"
        );
    }
}
