use skippr_plugin_data_source_delta_lake::*;
use skippr_runtime_sdk::append_source_runtime::run_append_data_source_main;
use skippr_runtime_sdk::plugins::cdc;
use skippr_runtime_sdk::plugins::DataSource;

skippr_runtime_sdk::runtime_main!(async {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-delta-lake: {}", err);
        std::process::exit(1);
    }
});

async fn run() -> std::io::Result<()> {
    run_append_data_source_main(
        "skippr-plugin-data-source-delta-lake",
        "DeltaLake",
        cdc::source_capabilities::by_name("DeltaLake").map(Into::into),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("DeltaLake")
                    .map_err(std::io::Error::other)?;
                let cfg: DataSourceDeltaLakePluginConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceDeltaLakePlugin::with_runtime_config(cfg);
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
