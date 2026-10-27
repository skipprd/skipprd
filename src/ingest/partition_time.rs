use crate::helpers::configuration::Config;
use std::io;

pub use skippr_runtime_sdk::chunker::partition_time::TimePartitioner;
pub use skippr_runtime_sdk::object_layout::hive_object_relative_path;

pub fn process_time_partition(filename: &str, config: &Config) -> Result<String, io::Error> {
    TimePartitioner::new(filename).process_from_layout(
        Some(&config.get_transform_batch_time_unit()),
        config.get_time_partition_prefix().as_deref(),
    )
}
