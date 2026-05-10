use skippr_runtime_sdk::plugins::cdc;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_plugin_data_source_mongodb::*;
use skippr_runtime_sdk::append_source_runtime::run_append_data_source_main;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-mongodb: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    run_append_data_source_main(
        "skippr-plugin-data-source-mongodb",
        "Mongodb",
        cdc::source_capabilities::by_name("Mongodb").map(Into::into),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("Mongodb")
                    .map_err(std::io::Error::other)?;
                let cfg: DataSourceMongodbPluginConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceMongodbPlugin::with_runtime_config(cfg);
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
