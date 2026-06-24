use skippr_plugin_data_source_upfoundry_link_graph_compact::*;
use skippr_runtime_sdk::append_source_runtime::run_append_data_source_main;
use skippr_runtime_sdk::plugins::cdc::{
    EventIdSemantics, SourceBootstrapStyle, SourceCheckpointStyle, SourceGuaranteeTier,
    SourceOrderModel,
};
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::protocol::RuntimeSourceCapabilityDescriptor;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!(
            "skippr-plugin-data-source-upfoundry-link-graph-compact: {}",
            err
        );
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    let capability = RuntimeSourceCapabilityDescriptor {
        name: "UpfoundryLinkGraphCompact".to_string(),
        guarantee_tier: SourceGuaranteeTier::IncrementalOnly,
        checkpoint_style: SourceCheckpointStyle::CursorBased,
        bootstrap_style: SourceBootstrapStyle::FullScan,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::None,
        declares_namespace_contracts: true,
    };
    run_append_data_source_main(
        "skippr-plugin-data-source-upfoundry-link-graph-compact",
        "UpfoundryLinkGraphCompact",
        Some(capability),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("UpfoundryLinkGraphCompact")
                    .map_err(std::io::Error::other)?;
                let cfg: UpfoundryLinkGraphCompactConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = UpfoundryLinkGraphCompactPlugin::new(cfg)?;
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
