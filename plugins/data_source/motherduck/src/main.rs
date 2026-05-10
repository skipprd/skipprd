use skippr_runtime_sdk::plugins::cdc;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_plugin_data_source_motherduck::*;
use skippr_runtime_sdk::append_source_runtime::run_append_data_source_main;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-motherduck: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    run_append_data_source_main(
        "skippr-plugin-data-source-motherduck",
        "Motherduck",
        cdc::source_capabilities::by_name("Motherduck").map(Into::into),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("Motherduck")
                    .map_err(std::io::Error::other)?;
                let cfg: DataSourceMotherduckPluginConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceMotherduckPlugin::with_runtime_config(cfg);
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
