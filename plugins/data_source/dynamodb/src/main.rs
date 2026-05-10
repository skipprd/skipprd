use skippr_runtime_sdk::plugins::cdc;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_plugin_data_source_dynamodb::*;
use skippr_runtime_sdk::append_source_runtime::run_append_data_source_main;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-dynamodb: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    run_append_data_source_main(
        "skippr-plugin-data-source-dynamodb",
        "Dynamodb",
        cdc::source_capabilities::by_name("Dynamodb").map(Into::into),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("Dynamodb")
                    .map_err(std::io::Error::other)?;
                let cfg: DataSourceDynamodbPluginConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceDynamodbPlugin::with_runtime_config(cfg).await;
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
