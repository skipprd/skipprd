use skippr_core::plugins::cdc;
use skippr_core::plugins::DataSource;
use skippr_plugin_data_source_sqs::*;
use skippr_runtime_sdk::append_source_runtime::run_append_data_source_main;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-sqs: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    run_append_data_source_main(
        "skippr-plugin-data-source-sqs",
        "Sqs",
        cdc::source_capabilities::by_name("Sqs").map(Into::into),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("Sqs")
                    .map_err(std::io::Error::other)?;
                let cfg: DataSourceSqsPluginConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceSqsPlugin::with_runtime_config(cfg).await;
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
