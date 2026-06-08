use skippr_plugin_data_source_revolut_business::*;
use skippr_runtime_sdk::append_source_runtime::run_append_data_source_main;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::protocol::RuntimeSourceCapabilityDescriptor;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("skippr-plugin-data-source-revolut-business: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    let capability = RuntimeSourceCapabilityDescriptor {
        name: "RevolutBusiness".to_string(),
        guarantee_tier: skippr_runtime_sdk::plugins::cdc::SourceGuaranteeTier::IncrementalOnly,
        checkpoint_style: skippr_runtime_sdk::plugins::cdc::SourceCheckpointStyle::CursorBased,
        bootstrap_style: skippr_runtime_sdk::plugins::cdc::SourceBootstrapStyle::FullScan,
        order_model: skippr_runtime_sdk::plugins::cdc::SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: true,
        event_id_semantics: skippr_runtime_sdk::plugins::cdc::EventIdSemantics::None,
        declares_namespace_contracts: true,
    };
    run_append_data_source_main(
        "skippr-plugin-data-source-revolut-business",
        "RevolutBusiness",
        Some(capability),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("RevolutBusiness")
                    .map_err(std::io::Error::other)?;
                let cfg: DataSourceRevolutBusinessPluginConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = DataSourceRevolutBusinessPlugin::new(cfg)?;
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
