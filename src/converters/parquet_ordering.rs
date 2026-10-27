pub use skippr_runtime_sdk::converters::parquet_ordering::*;

use arrow::datatypes::SchemaRef;

use crate::helpers::configuration::Config;

fn configured_order_fields(config: &Config) -> Vec<String> {
    let raw = config.get_transform_batch_order_fields();
    if raw.is_empty() {
        return vec![];
    }
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn resolve_effective_order(config: &Config, schema: &SchemaRef) -> Vec<String> {
    let configured = configured_order_fields(config);
    resolve_effective_order_from_fields(schema, &configured)
}

pub fn log_unmatched_order_fields(config: &Config) {
    skippr_runtime_sdk::converters::parquet_ordering::log_unmatched_order_fields(
        &configured_order_fields(config),
    );
}
