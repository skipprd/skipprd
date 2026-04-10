#[path = "../../../shared/append_source_runtime.rs"]
mod append_source_runtime;

use append_source_runtime::run_append_data_source_main;
use skippr::plugins::cdc;
use skippr::plugins::DataSource;
use skippr_plugin_runtime_link::runtime_plugin_data_sources::runtime_source_http_server::DataSourceHttpServerPlugin;
use skippr_plugin_runtime_link::runtime_plugin_data_sources::runtime_source_http_server::DataSourceHttpServerPluginConfig;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-http-server: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    run_append_data_source_main(
        "skippr-plugin-data-source-http-server",
        "HttpServer",
        cdc::source_capabilities::by_name("HttpServer").map(Into::into),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("HttpServer")
                    .map_err(std::io::Error::other)?;
                let cfg: DataSourceHttpServerPluginConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceHttpServerPlugin::with_runtime_config(cfg);
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
