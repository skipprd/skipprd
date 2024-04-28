use std::{fs, process};
use std::fs::OpenOptions;
use std::io::{BufReader, BufWriter};
use datafusion::prelude::{ParquetReadOptions, SessionConfig, SessionContext};
use sqlparser::ast::{Ident, ObjectName};
use crate::cli::{CLI_MODE, Mode};
use crate::discover::{Metadata, PipelineMetadata};
use crate::helpers::configuration::{Config, PIPELINE_NAME};
use crate::METADATA;
use crate::plugins::athena::{AwsAthena, DataOutputAwsAthenaPlugin};
use crate::sql::operators::alter_column::alter_column_type;
use crate::sql::operators::drop_column::alter_column_drop;
use crate::sql::operators::dump_schema::dump_schema;
use crate::sql::parser::{PipelineToggle, SParser, Statement};

pub async fn query(sql_str: &str) {

    let mut parser = SParser::new(sql_str).unwrap();

    match parser.parse_statement() {
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
                Mode::Sync(options) => {
                    let _ = fs::remove_dir_all(&data_dir).expect(format!("Failed to remove dir: {}", data_dir).as_str());
                    Config::delete_metadata().await;
                    println!("Dropped Pipeline");
                },
                Mode::Query(options) => {
                    match Config::get_metadata().await {
                        Ok(metadata) => {
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
                Mode::Sync(options) => {
                    let _ = fs::remove_dir_all(&data_dir).expect(format!("Failed to remove dir: {}", data_dir).as_str());
                    println!("Pipeline reset, on next sync run all data will be re-ingested");
                    
                    // remove the SQL stmt from metadata
                    metadata.sql = None;
                    Config::set_metadata(&metadata, false).await;
                },
                Mode::Query(options) => {

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
            let workspace = Config::get_workspace_name();

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
            let workspace = Config::get_workspace_name();

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
        Err(e) => {

            println!("Unknown SQL Dialect. {}", e);
        },
        // _ => {
        //
        //     let mut table_name = "".to_string();
        //
        //     if sql_str.to_lowercase().split("from").collect::<Vec<&str>>().len() > 1 {
        //         // println!("Invalid query, must be in the format: SELECT * FROM <table_name>");
        //         // process::exit(1);
        //
        //         table_name = sql_str.to_lowercase().split("from").collect::<Vec<&str>>()[1].split(" ").collect::<Vec<&str>>()[1].trim().replace(";", "");
        //
        //     }
        //     // else {
        //     //     table_name = "bike_hire".to_string();
        //     // }
        //
        //
        //     PIPELINE_NAME.write().clear();
        //     PIPELINE_NAME.write().push_str(&table_name);
        //     Config::init().await;
        //     let workspace = Config::get_workspace_name();
        //
        //     let data_dir = Config::get_data_dir();
        //     let output_dir = format!("{}/output_buffer", data_dir);
        //
        //     println!("Querying data dir: {}", output_dir);
        //
        //     let mut session_config = SessionConfig::new();
        //     session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
        //     session_config = session_config.set("datafusion.catalog.default_catalog", "skippr".into());
        //     session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());
        //
        //     let ctx = SessionContext::with_config(session_config);
        //
        //     match ctx.register_parquet(&table_name, &output_dir, ParquetReadOptions::default()).await {
        //         Ok(_) => {}
        //         Err(e) => {
        //             println!("Can't find data for table: {} in dir: {}. Error: {:?}", table_name, output_dir, e);
        //             process::exit(1);
        //         }
        //     }
        //
        //     let df = match ctx.sql(sql_str).await {
        //         Ok(df) => df,
        //         Err(e) => {
        //             println!("Error: {}", e);
        //             process::exit(1);
        //         }
        //     };
        //
        //     match df.show().await {
        //         Ok(res) => {
        //             res
        //         }
        //         Err(e) => {
        //             println!("Error: {}", e);
        //             process::exit(1);
        //         }
        //     }
        // }
    }
}