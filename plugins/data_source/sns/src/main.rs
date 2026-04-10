#[path = "../../../shared/append_source_runtime.rs"]
mod append_source_runtime;

use append_source_runtime::run_append_data_source_main;
use skippr::plugins::cdc;
use skippr::plugins::DataSource;
use skippr_plugin_runtime_link::runtime_plugin_data_sources::runtime_source_sns::DataSourceSnsPlugin;
use skippr_plugin_runtime_link::runtime_plugin_data_sources::runtime_source_sns::DataSourceSnsPluginConfig;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-sns: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    run_append_data_source_main(
        "skippr-plugin-data-source-sns",
        "Sns",
        cdc::source_capabilities::by_name("Sns").map(Into::into),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("Sns")
                    .map_err(std::io::Error::other)?;
                let cfg: DataSourceSnsPluginConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceSnsPlugin::with_runtime_config(cfg).await;
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
