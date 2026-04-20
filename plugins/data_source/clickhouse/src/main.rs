use skippr_core::plugins::cdc;
use skippr_core::plugins::DataSource;
use skippr_plugin_data_source_clickhouse::*;
use skippr_runtime_sdk::append_source_runtime::run_append_data_source_main;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-clickhouse: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    run_append_data_source_main(
        "skippr-plugin-data-source-clickhouse",
        "Clickhouse",
        cdc::source_capabilities::by_name("Clickhouse").map(Into::into),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("Clickhouse")
                    .map_err(std::io::Error::other)?;
                let cfg: DataSourceClickhousePluginConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceClickhousePlugin::with_runtime_config(cfg);
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
