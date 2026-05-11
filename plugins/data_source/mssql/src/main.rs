use skippr_plugin_data_source_mssql::*;
use skippr_runtime_sdk::append_source_runtime::run_append_data_source_main;
use skippr_runtime_sdk::plugins::cdc;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::runtime_main::run_runtime_main;

fn main() {
    run_runtime_main("mssql-source-main", async {
        if let Err(err) = run().await {
            eprintln!("skippr-plugin-data-source-mssql: {}", err);
            std::process::exit(1);
        }
    });
}

async fn run() -> std::io::Result<()> {
    run_append_data_source_main(
        "skippr-plugin-data-source-mssql",
        "Mssql",
        cdc::source_capabilities::by_name("Mssql").map(Into::into),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("Mssql")
                    .map_err(std::io::Error::other)?;
                let cfg: DataSourceMssqlPluginConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceMssqlPlugin::with_runtime_config(cfg);
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
