use std::{fs, process};
use std::fs::OpenOptions;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use arrow::array::{Array, ArrayRef, Date32Array, Int32Array, StringArray};
use arrow_schema::DataType;
use aws_config::meta::region::RegionProviderChain;
use aws_config::profile::ProfileFileCredentialsProvider;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl};
use datafusion::logical_expr::{ColumnarValue, create_udf, Partitioning, ScalarUDF, Signature, Volatility};
use datafusion::prelude::{ParquetReadOptions, SessionConfig, SessionContext};
use sqlparser::ast::{Ident, ObjectName};
use crate::cli::{CLI_MODE, Mode};
use crate::discover::{AnalyseSchema, Metadata, PipelineMetadata};
use crate::helpers::configuration::{Config, PIPELINE_NAME};
use crate::{ARROW_SCHEMA, METADATA};
use crate::plugins::athena::{AwsAthena, DataOutputAwsAthenaPlugin};
use crate::sql::operators::alter_column::alter_column_type;
use crate::sql::operators::drop_column::alter_column_drop;
use crate::sql::operators::dump_schema::dump_schema;
use crate::sql::operators::drop_table::drop_table;
use crate::sql::parser::{PipelineToggle, SParser, Statement};

use chrono::{DateTime, NaiveDate};
use datafusion::common::cast::as_date32_array;
use datafusion::datasource::object_store::DefaultObjectStoreRegistry;
use datafusion::error::DataFusionError;
use datafusion::physical_plan::functions::make_scalar_function;
use object_store::aws::{AmazonS3, AmazonS3Builder, AmazonS3ConfigKey};
use object_store::{ObjectStore, parse_url};
use url::Url;
use crate::helpers::Helpers;
use crate::ingest_work::Ingest;
use crate::sql::{SqlDocParser, SqlStatementDoc};


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

                let diff = end_date.clone().unwrap() - start_date.clone().unwrap();
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

fn as_string_array(array: &ArrayRef) -> Result<&StringArray, DataFusionError> {
    if let DataType::Utf8 = array.data_type() {
        Ok(array.as_any().downcast_ref::<StringArray>().unwrap())
    } else {
        Err(DataFusionError::Internal("Expected StringArray".to_string()))
    }
}

/// Documents a SQL query, returning information about what it does
pub async fn document_query(sql_str: &str) -> Result<Option<SqlStatementDoc>, String> {
    SqlDocParser::parse_and_document(sql_str)
}

/// Function to explain a SQL query in plain English before executing it
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
                    let mut tries = 15;
                    let mut delete = true;
                    while delete {
                        // retries as workaround for https://github.com/rust-lang/rust/issues/29497
                        match fs::remove_dir_all(&data_dir) {
                            Ok(_) => {
                                delete = false;
                            },
                            Err(e) => {
                                println!("Failed to remove dir: {}.", e);
                                if tries == 0 {
                                    delete = false;
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

            let mut table_name = "".to_string();

            if sql_str.to_lowercase().split("from").collect::<Vec<&str>>().len() > 1 {
                // println!("Invalid query, must be in the format: SELECT * FROM <table_name>");
                // process::exit(1);

                table_name = sql_str.to_lowercase().split("from").collect::<Vec<&str>>()[1].split(" ").collect::<Vec<&str>>()[1].trim().replace(";", "");

                if table_name.contains(".") {
                    println!("Table name: {}", table_name);
                    table_name = table_name.split(".").collect::<Vec<&str>>()[1].to_string();
                    println!("Table name: {}", table_name);
                }

            }
            // else {
            //     table_name = "bike_hire".to_string();
            // }


            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str("metrics");
            Config::init().await;
            let workspace = Config::get_workspace_name();

            let pipeline_metadata = match Config::get_metadata().await {
                Ok(mut pipeline_metadata) => {
                    println!("Found existing Skippr metadata");
                    pipeline_metadata
                }
                Err(_e) => {
                    println!("No existing Skippr metadata.");
                    return;
                }
            };

            METADATA.write().clone_from(&pipeline_metadata);

            let flatten = Config::get_transform_flatten_events();

            for (namespace, _metadata) in pipeline_metadata.metadata.iter() {
                match Ingest::prepare_arrow_schema_with_metadata(&namespace, &pipeline_metadata.metadata, flatten) {
                    Ok(_t) => {}
                    Err(e) => {
                        println!("Failed to prepare arrow schema: {}", e);
                        return;
                    }
                }
            }

            let data_dir = Config::get_data_dir();
            let output_dir = format!("{}/output_buffer", data_dir);
            // let output_dir = format!("{}/output_buffer/*/*/*/p_year=2023", data_dir);

            let mut session_config = SessionConfig::new();
            session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
            session_config = session_config.set("datafusion.catalog.default_catalog", "skippr".into());
            session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());

            let ctx = SessionContext::new_with_config(session_config);

            // let mut paths = Vec::new();

            // recurse_paths(&output_dir, &table_name, &ctx, &mut paths);
            // println!("Found paths: {:?}", paths);

            // for path in paths {
            //     match ctx.register_parquet(&table_name, &path.to_str().unwrap(), ParquetReadOptions::default()).await {
            //         Ok(_) => {}
            //         Err(e) => {
            //             println!("Can't find data for table: {} in dir: {}. Error: {:?}", table_name, path.to_str().unwrap(), e);
            //             process::exit(1);
            //         }
            //     }
            // }


            // custom function
            let datediff = make_scalar_function(datediff);
            let datediff_udf = create_udf(
                "datediff",
                vec![DataType::Utf8, DataType::Utf8],
                Arc::new(DataType::Int32),
                // vec![DataType::Date32, DataType::Date32],
                // Arc::new(DataType::Date32),
                Volatility::Immutable,
                datediff,
            );
            ctx.register_udf(datediff_udf.clone());


            let table_partition_cols = vec![
                // ("p_tenant_id".to_string(), DataType::Utf8),
                // ("p_source_type".to_string(), DataType::Utf8),
                ("year".to_string(), DataType::Utf8),
                ("month".to_string(), DataType::Utf8),
                ("day".to_string(), DataType::Utf8),

            ];
            //
            // let listing_options = ListingOptions::new(Arc::new(
            //     ParquetFormat::default()
            // ))
            //     .with_table_partition_cols(table_partition_cols);

            /**
             * Register local data
             */
            // let table_dir = format!("{}/{}/", output_dir, &table_name);
            //
            // println!("Querying data dir: {}", table_dir);
            //
            // // ctx.register_listing_table(&table_name, table_dir, listing_options, None, None).await.unwrap();
            //
            // match ctx.register_parquet(&table_name, &table_dir, ParquetReadOptions::default()).await {
            //     Ok(_) => {}
            //     Err(_e) => {
            //         println!("Can't find data for table: {} in dir: {}", &table_name, table_dir);
            //         process::exit(1);
            //     }
            // }
            
            
            /**
            * Register Glue Catalog
            */
            // Get table metadata from AWS Glue Catalog
            // let aws_config = aws_config::from_env().load().await;

            let aws_config = aws_config::from_env().load().await;
            // let athena_client = AthenaClient::new(&aws_config);

            // use aws_config::default_provider::credentials::De
            // faultCredentialsChain;
            // let credentials_provider = DefaultCredentialsChain::builder().build();
            //
            // use aws_config::BehaviorVersion;
            // let aws_config = aws_config::defaults(BehaviorVersion::v2024_03_28())
            //     .region("us-east-1")
            //     .profile_name("skippr-prod")
            //     .credentials_provider(credentials_provider.await)
            //     .load()
            //     .await;


            let glue_client = aws_sdk_glue::Client::new(&aws_config);

            let db_name = "datalake";
            let table_name = "metric";
            let table = glue_client.get_table()
                .database_name(db_name)
                .name(table_name)
                .send()
                .await
                .expect("Failed to get table from Glue Catalog")
                .table
                .expect("Table not found");

            // Get table location from Glue Catalog
            let location = table
                .storage_descriptor
                .expect("Storage descriptor not found")
                .location
                .expect("Table location not found");

            // Register the table with DataFusion
            let format = ParquetFormat::default();
            // let file_schema = Arc::new(format.infer_schema(&mut ctx.state().runtime_env(), &location, None).await?);
            let schema = ARROW_SCHEMA.read().clone();
            
            let options = ListingOptions {
                format: Arc::new(format),
                table_partition_cols: table_partition_cols,
                collect_stat: true,
                file_extension: "".to_string(),
                target_partitions: 8,
                file_sort_order: vec![],
            };
            // let table_uri = ListingTableUrl::parse(&location).unwrap();
            // let config = ListingTableConfig::new(table_uri).with_listing_options(options).with_schema(schema.get(&table_name.to_string()).unwrap().clone());
            // let table = ListingTable::try_new(config).unwrap();


            let store: AmazonS3 = AmazonS3Builder::new()
                .with_bucket_name("skippr-prod-datalake")
                .with_config("aws_access_key_id".parse().unwrap(), "AKIAR5UHZSQHNETZJLHH")
                .with_config("aws_secret_access_key".parse().unwrap(), "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")
                .with_config(AmazonS3ConfigKey::DefaultRegion, "us-east-1")
                .build()
                .unwrap();
            // Alternatively can create an ObjectStore from an S3 URL
            // println!("Location: {}", location);
            let url = Url::parse(&location).unwrap();
            // let (store, path) = parse_url(&url).unwrap();


            let store: Arc<dyn ObjectStore> = Arc::new(store);

            ctx.runtime_env().register_object_store(&url, store);
            
            ctx.register_listing_table(
                table_name,
                &location,
                options,
                Some(schema.get(&table_name.to_string()).unwrap().clone()),
                None
            ).await.unwrap();

            let df = match ctx.sql(sql_str).await {
                Ok(df) => df,
                Err(e) => {
                    println!("Error: {}", e);
                    process::exit(1);
                }
            };

            match df.show().await {
                Ok(res) => {
                    res
                }
                Err(e) => {
                    println!("Error: {}", e);
                    process::exit(1);
                }
            }
        }
    }
}

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