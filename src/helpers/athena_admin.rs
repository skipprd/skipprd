use std::io;

use aws_sdk_glue::Client as GlueClient;
use serde::Deserialize;

use crate::helpers::configuration::Config;

#[derive(Debug, Clone, Deserialize)]
pub struct AthenaAdminConfig {
    #[serde(default)]
    pub glue_database_name: String,
}

pub fn output_athena_admin_config() -> Result<AthenaAdminConfig, String> {
    match Config::get_pipeline_output_plugin_config() {
        Ok(config) if config.plugin_name == "Athena" => config.deserialize(),
        Ok(config) => Err(format!(
            "pipeline output plugin '{}' is not Athena",
            config.plugin_name
        )),
        Err(_) => Ok(AthenaAdminConfig {
            glue_database_name: Config::getenv("SCHEMA_OUTPUT_GLUE_DATABASE_NAME", ""),
        }),
    }
}

pub async fn delete_glue_database(database_name: &str) -> Result<bool, String> {
    loop {
        println!(
            "Are you sure you want to drop the database? To confirm, please type the database name ('{}'). Type 'exit' or ctrl+c to cancel:",
            database_name
        );

        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap_or_default();

        if input.trim() == database_name {
            println!("Dropping database '{}'", database_name);

            let mut timeout = 10;
            println!(
                "Waiting {} seconds before dropping database '{}', ctrl+c to cancel",
                timeout, database_name
            );

            while timeout > 0 {
                tokio::time::sleep(tokio::time::Duration::from_secs(timeout)).await;
                timeout -= 1;
            }
            break;
        } else if input.trim().eq_ignore_ascii_case("exit") {
            return Err(format!(
                "Drop canceled. Exiting without dropping database '{}'.",
                database_name
            ));
        } else {
            return Err(format!(
                "Incorrect database name entered: '{}'.",
                input.trim()
            ));
        }
    }

    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let glue_client = GlueClient::new(&aws_config);

    let output = glue_client
        .get_tables()
        .database_name(database_name)
        .send()
        .await
        .map_err(|err| err.into_service_error().to_string())?;

    if let Some(tables) = output.table_list {
        if tables.is_empty() {
            println!("No tables found in database '{}'", database_name);
        }

        println!("Deleting tables in database '{}'", database_name);

        for table in tables {
            let table_name = table.name;
            println!("Deleting table '{}'", table_name);

            let mut next_token = String::new();
            while let Ok(partitions) = glue_client
                .get_partitions()
                .database_name(database_name)
                .table_name(&table_name)
                .max_results(25)
                .next_token(next_token.clone())
                .send()
                .await
            {
                if let Some(partitions) = partitions.partitions {
                    if partitions.is_empty() {
                        break;
                    }

                    println!(
                        "Deleting {} partitions in table '{}'",
                        partitions.len(),
                        table_name
                    );

                    for partition in partitions {
                        glue_client
                            .delete_partition()
                            .database_name(database_name)
                            .table_name(&table_name)
                            .set_partition_values(partition.values)
                            .send()
                            .await
                            .map_err(|err| err.into_service_error().to_string())?;
                    }
                }

                if partitions.next_token.is_none() {
                    break;
                }
                next_token = partitions.next_token.unwrap_or_default();
            }

            let mut next_token = String::new();
            while let Ok(table_versions) = glue_client
                .get_table_versions()
                .database_name(database_name)
                .table_name(&table_name)
                .max_results(25)
                .next_token(next_token.clone())
                .send()
                .await
            {
                if let Some(versions) = table_versions.table_versions {
                    if versions.is_empty() {
                        break;
                    }

                    for version in versions {
                        if let Some(version_id) = version.version_id {
                            glue_client
                                .delete_table_version()
                                .database_name(database_name)
                                .table_name(&table_name)
                                .version_id(version_id)
                                .send()
                                .await
                                .map_err(|err| err.into_service_error().to_string())?;
                        }
                    }
                }

                if table_versions.next_token.is_none() {
                    break;
                }
                next_token = table_versions.next_token.unwrap_or_default();
            }

            glue_client
                .delete_table()
                .database_name(database_name)
                .name(&table_name)
                .send()
                .await
                .map_err(|err| err.into_service_error().to_string())?;
        }
    }

    glue_client
        .delete_database()
        .name(database_name)
        .send()
        .await
        .map_err(|err| err.into_service_error().to_string())?;

    Ok(true)
}

pub async fn glue_delete_table(
    config: &AthenaAdminConfig,
    namespace: &str,
) -> Result<bool, String> {
    if config.glue_database_name.is_empty() {
        return Err("Athena Glue database name is not configured".to_string());
    }

    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let glue_client = GlueClient::new(&aws_config);

    glue_client
        .delete_table()
        .database_name(&config.glue_database_name)
        .name(namespace)
        .send()
        .await
        .map(|_| true)
        .map_err(|err| err.into_service_error().to_string())
}
