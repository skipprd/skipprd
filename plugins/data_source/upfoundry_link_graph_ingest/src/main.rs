use skippr_plugin_data_source_upfoundry_link_graph_ingest::*;
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
            "skippr-plugin-data-source-upfoundry-link-graph-ingest: {}",
            err
        );
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    let capability = RuntimeSourceCapabilityDescriptor {
        name: "UpfoundryLinkGraphIngest".to_string(),
        guarantee_tier: SourceGuaranteeTier::IncrementalOnly,
        checkpoint_style: SourceCheckpointStyle::CursorBased,
        bootstrap_style: SourceBootstrapStyle::FullScan,
        order_model: SourceOrderModel::UnsupportedForFinalState,
        supports_deletes: false,
        event_id_semantics: EventIdSemantics::None,
        declares_namespace_contracts: true,
    };
    run_append_data_source_main(
        "skippr-plugin-data-source-upfoundry-link-graph-ingest",
        "UpfoundryLinkGraphIngest",
        Some(capability),
        |start| {
            Box::pin(async move {
                start
                    .config
                    .0
                    .expect_plugin("UpfoundryLinkGraphIngest")
                    .map_err(std::io::Error::other)?;
                let cfg: UpfoundryLinkGraphIngestConfig =
                    start.config.0.decode().map_err(std::io::Error::other)?;
                let plugin = UpfoundryLinkGraphIngestPlugin::new(cfg)?;
                Ok(Box::new(plugin) as Box<dyn DataSource + Send>)
            })
        },
    )
    .await
}
