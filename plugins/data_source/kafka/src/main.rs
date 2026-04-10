#[path = "../../../shared/append_source_runtime.rs"]
mod append_source_runtime;

use append_source_runtime::run_append_data_source_main;
use skippr::plugins::cdc;
use skippr::plugins::DataSource;
use skippr_plugin_runtime_link::runtime_plugin_data_sources::runtime_source_kafka::DataSourceKafkaPlugin;
use skippr_plugin_runtime_link::runtime_plugin_data_sources::runtime_source_kafka::DataSourceKafkaPluginConfig;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-kafka: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    run_append_data_source_main(
        "skippr-plugin-data-source-kafka",
        "Kafka",
        cdc::source_capabilities::by_name("Kafka").map(Into::into),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("Kafka")
                    .map_err(std::io::Error::other)?;
                let cfg: DataSourceKafkaPluginConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceKafkaPlugin::with_runtime_config(cfg);
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
