use std::{fs, process};
use std::fs::OpenOptions;
use std::io::{BufReader};
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
                METADATA.write().clone_from(&skippr_metadata);
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

            dump_schema(&metadata, &stmt).expect("Failed to drop column");
            
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
                METADATA.write().clone_from(&skippr_metadata);
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
                METADATA.write().clone_from(&skippr_metadata);
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
                        METADATA.write().clone_from(&skippr_metadata);
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

            // Print the sql parser.parse_statement() and exit
            println!("SQL syntax not found: {:?}", parser.parse_statement());
            process::exit(1);
            
            // Unreachable code below - removing
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