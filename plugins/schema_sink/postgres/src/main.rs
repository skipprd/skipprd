use std::io;

use clap::Parser;
use skippr_plugin_data_sink_postgres::{DataSinkPostgresPlugin, DataSinkPostgresPluginConfig};
use skippr_runtime_sdk::discover::schema_alter::SchemaAlterOp;
use skippr_runtime_sdk::discover::OutputMetadata;
use skippr_runtime_sdk::plugins::{AlterSupport, SchemaSink};
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_schema_sink_plugin,
};

#[derive(Debug, Parser)]
struct PostgresSchemaRuntimePluginCli {}

struct PostgresSchemaRuntimePlugin {
    inner: DataSinkPostgresPlugin,
}

skippr_runtime_sdk::declare_schema_sink_spec!(
    PostgresSchemaSinkSpec,
    PostgresSchemaRuntimePlugin,
    "Postgres",
    skippr_runtime_sdk::plugins::AlterSupport::Native
);

#[async_trait::async_trait]
impl SchemaSink for PostgresSchemaRuntimePlugin {
    fn alter_support(&self) -> AlterSupport {
        AlterSupport::Native
    }

    async fn apply_alter(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
        op: &SchemaAlterOp,
        field_id: i32,
    ) -> Result<(), io::Error> {
        self.inner
            .apply_alter(namespace, metadata, op, field_id)
            .await
    }

    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), io::Error> {
        self.inner.sync_schema(namespace, metadata).await
    }
}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = PostgresSchemaRuntimePluginCli::parse();
    if let Err(err) = run_runtime_schema_sink_plugin(
        "Postgres",
        "Postgres",
        "skippr-plugin-schema-sink-postgres",
        |install| async move {
            let config: DataSinkPostgresPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            Ok(PostgresSchemaRuntimePlugin {
                inner: DataSinkPostgresPlugin::new_with_config(
                    buffer_name_for_runtime_binding(install.binding),
                    config,
                )
                .await,
            })
        },
    )
    .await
    {
        eprintln!("skippr-plugin-schema-sink-postgres: {}", err);
        std::process::exit(1);
    }
});
