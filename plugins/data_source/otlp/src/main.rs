use skippr_plugin_data_source_otlp::*;
use skippr_runtime_sdk::append_source_runtime::run_append_data_source_main;
use skippr_runtime_sdk::plugins::cdc;
use skippr_runtime_sdk::plugins::DataSource;

skippr_runtime_sdk::runtime_main!(async {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-otlp: {}", err);
        std::process::exit(1);
    }
});

async fn run() -> std::io::Result<()> {
    run_append_data_source_main(
        "skippr-plugin-data-source-otlp",
        "Otlp",
        cdc::source_capabilities::by_name("Otlp").map(Into::into),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("Otlp")
                    .map_err(std::io::Error::other)?;
                let cfg: OtlpConfig = start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceOtlpPlugin::with_runtime_config(cfg);
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn plugin_name_is_otlp() {
        let manifest = include_str!("../Cargo.toml");
        assert!(manifest.contains("plugin_name = \"Otlp\""));
        assert!(manifest.contains("name = \"skippr-plugin-data-source-otlp\""));
    }
}
