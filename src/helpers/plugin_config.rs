pub use skippr_runtime_sdk::helpers::plugin_config::*;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::serdes::output_format::OutputFormat;

    use super::PluginConfigEntry;

    #[test]
    fn version_reads_optional_plugin_field() {
        let entry = PluginConfigEntry {
            plugin_name: "Athena".to_string(),
            config: json!({
                "version": "0.1.1",
                "s3_bucket": "bucket"
            }),
        };

        assert_eq!(entry.version().as_deref(), Some("0.1.1"));
    }

    #[test]
    fn version_ignores_blank_values() {
        let entry = PluginConfigEntry {
            plugin_name: "Athena".to_string(),
            config: json!({
                "version": "   "
            }),
        };

        assert_eq!(entry.version(), None);
    }

    #[test]
    fn skipprlake_does_not_accept_format() {
        let src = include_str!("plugin_config.rs");
        assert!(
            !src.contains("is_plugin_name(name) => \"parquet\""),
            "SkipprLake must not default format to parquet"
        );
        let entry = PluginConfigEntry {
            plugin_name: skippr_iceberg_catalog::SkipprLakeConfig::PLUGIN_NAME.to_string(),
            config: serde_json::json!({
                "warehouse": "file:///tmp/warehouse",
                "catalog_table": "cat",
                "object_store": { "type": "file" },
                "table_namespace": "bronze"
            }),
        };
        assert_eq!(
            entry
                .resolved_output_format(crate::pipeline_type::PipelineType::Ingest, None)
                .unwrap(),
            None
        );
        assert_eq!(entry.format(crate::pipeline_type::PipelineType::Ingest), "");
    }

    #[test]
    fn athena_defaults_parquet_and_rejects_jsonl() {
        let entry = PluginConfigEntry {
            plugin_name: crate::plugins::cdc::sink_capabilities::ATHENA
                .name
                .to_string(),
            config: serde_json::json!({
                "s3_bucket": "output",
                "s3_prefix": "warehouse",
                "athena_workgroup_name": "primary",
                "athena_results_s3_bucket": "results"
            }),
        };
        assert_eq!(
            entry
                .resolved_output_format(crate::pipeline_type::PipelineType::Ingest, None)
                .unwrap(),
            Some(OutputFormat::Parquet)
        );

        let parquet = PluginConfigEntry {
            plugin_name: crate::plugins::cdc::sink_capabilities::ATHENA
                .name
                .to_string(),
            config: serde_json::json!({ "format": "parquet" }),
        };
        assert_eq!(
            parquet
                .resolved_output_format(crate::pipeline_type::PipelineType::Ingest, None)
                .unwrap(),
            Some(OutputFormat::Parquet)
        );

        let rejected = PluginConfigEntry {
            plugin_name: crate::plugins::cdc::sink_capabilities::ATHENA
                .name
                .to_string(),
            config: serde_json::json!({ "format": "jsonl" }),
        };
        let err = rejected
            .resolved_output_format(crate::pipeline_type::PipelineType::Ingest, None)
            .unwrap_err();
        assert!(err.contains("does not support format 'jsonl'"));
    }

    #[test]
    fn athena_iceberg_does_not_accept_format() {
        let entry = PluginConfigEntry {
            plugin_name: crate::plugins::cdc::sink_capabilities::ATHENA_ICEBERG
                .name
                .to_string(),
            config: serde_json::json!({
                "warehouse": "s3://lake/",
                "glue_database_name": "analytics",
                "athena_workgroup_name": "primary",
                "athena_results_s3_bucket": "results"
            }),
        };
        assert_eq!(
            entry
                .resolved_output_format(crate::pipeline_type::PipelineType::Ingest, None)
                .unwrap(),
            None
        );
        assert_eq!(entry.format(crate::pipeline_type::PipelineType::Ingest), "");
    }

    #[test]
    fn duckdb_does_not_accept_format() {
        let entry = PluginConfigEntry {
            plugin_name: crate::plugins::cdc::sink_capabilities::DUCKDB
                .name
                .to_string(),
            config: serde_json::json!({
                "warehouse": "file:///tmp/lake",
                "table_namespace": "bronze"
            }),
        };
        assert_eq!(
            entry
                .resolved_output_format(crate::pipeline_type::PipelineType::Ingest, None)
                .unwrap(),
            None
        );
        assert_eq!(entry.format(crate::pipeline_type::PipelineType::Ingest), "");
    }

    #[test]
    fn file_omitted_format_is_parquet() {
        let entry = PluginConfigEntry {
            plugin_name: "File".to_string(),
            config: json!({}),
        };
        assert_eq!(
            entry
                .resolved_output_format(crate::pipeline_type::PipelineType::Ingest, None)
                .unwrap(),
            Some(OutputFormat::Parquet)
        );
    }

    #[test]
    fn file_accepts_jsonl() {
        let entry = PluginConfigEntry {
            plugin_name: "File".to_string(),
            config: json!({ "format": "jsonl" }),
        };
        assert_eq!(
            entry
                .resolved_output_format(crate::pipeline_type::PipelineType::Ingest, None)
                .unwrap(),
            Some(OutputFormat::Jsonl)
        );
    }

    #[test]
    fn file_copy_csv_format_follows_pipeline_type() {
        let entry = PluginConfigEntry {
            plugin_name: "File".to_string(),
            config: json!({ "format": "csv" }),
        };
        assert_eq!(
            entry.format(crate::pipeline_type::PipelineType::Copy),
            "csv"
        );
        assert_eq!(entry.format(crate::pipeline_type::PipelineType::Ingest), "");
        assert_eq!(
            entry
                .resolved_output_format(crate::pipeline_type::PipelineType::Copy, None)
                .unwrap(),
            Some(OutputFormat::Csv)
        );
    }

    #[test]
    fn postgres_rejects_configured_format() {
        let entry = PluginConfigEntry {
            plugin_name: "Postgres".to_string(),
            config: json!({ "format": "jsonl" }),
        };
        let err = entry
            .resolved_output_format(crate::pipeline_type::PipelineType::Ingest, None)
            .unwrap_err();
        assert!(err.contains("writes rows into tables"));
        assert!(err.contains("Remove `format: jsonl`"));
    }
}
