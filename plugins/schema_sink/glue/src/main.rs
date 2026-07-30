use std::io;

use async_trait::async_trait;
use clap::Parser;
use skippr_plugin_data_sink_athena::*;
use skippr_runtime_sdk::discover::OutputMetadata;
use skippr_runtime_sdk::plugins::source_contract::namespace_source_contract;
use skippr_runtime_sdk::plugins::{SchemaSink, SchemaSyncRequest};
use skippr_runtime_sdk::protocol::{RuntimeBinding, RuntimeExecutionContext};
use skippr_runtime_sdk::sink_runtime_entry::run_runtime_schema_sink_plugin;

struct GlueAthenaSchemaSync {
    context: RuntimeExecutionContext,
    binding: RuntimeBinding,
    config: DataSinkAthenaPluginConfig,
}

skippr_runtime_sdk::declare_schema_sink_spec!(GlueSchemaSinkSpec, GlueAthenaSchemaSync, "Glue");

#[async_trait]
impl SchemaSink for GlueAthenaSchemaSync {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), io::Error> {
        self.sync_schema_request(
            SchemaSyncRequest {
                namespace,
                compaction_id: "",
                source_contract: None,
            },
            metadata,
        )
        .await
    }

    async fn sync_schema_request(
        &self,
        request: SchemaSyncRequest<'_>,
        metadata: &OutputMetadata,
    ) -> Result<(), io::Error> {
        let source_contract = request
            .source_contract
            .cloned()
            .or_else(|| namespace_source_contract(request.namespace));
        AwsAthena::create_or_update_schema_with_config(
            request.namespace,
            metadata,
            &self.context,
            self.binding,
            self.config.clone(),
            source_contract.as_ref(),
        )
        .await
    }
}

#[derive(Debug, Parser)]
struct Cli {}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = Cli::parse();
    if let Err(err) = run_runtime_schema_sink_plugin(
        "Athena",
        "Glue",
        "skippr-plugin-schema-sink-glue",
        |install| async move {
            let cfg: DataSinkAthenaPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            Ok(GlueAthenaSchemaSync {
                context: install.context,
                binding: install.binding,
                config: cfg,
            })
        },
    )
    .await
    {
        eprintln!("skippr-plugin-schema-sink-glue: {}", err);
        std::process::exit(1);
    }
});
