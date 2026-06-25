use skippr_plugin_data_source_upfoundry_link_graph_wat_index::*;
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
            "skippr-plugin-data-source-upfoundry-link-graph-wat-index: {}",
            err
        );
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    let capability = RuntimeSourceCapabilityDescriptor {
        name: "UpfoundryLinkGraphWatIndex".to_string(),
        guarantee_tier: SourceGuaranteeTier::IncrementalOnly,
        checkpoint_style: SourceCheckpointStyle::CursorBased,
        bootstrap_style: SourceBootstrapStyle::FullScan,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::None,
        declares_namespace_contracts: true,
    };
    run_append_data_source_main(
        "skippr-plugin-data-source-upfoundry-link-graph-wat-index",
        "UpfoundryLinkGraphWatIndex",
        Some(capability),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("UpfoundryLinkGraphWatIndex")
                    .map_err(std::io::Error::other)?;
                let cfg: UpfoundryLinkGraphWatIndexConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = UpfoundryLinkGraphWatIndexPlugin::new(cfg)?;
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
