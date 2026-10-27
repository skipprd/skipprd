use crate::chunker::partition_time::TimePartitioner;
use crate::chunker::BufferChunker;

/// Hive-style object key: `{prefix}/{namespace}/{partition}/{time}/{stem}.{ext}`.
pub fn hive_object_relative_path(
    prefix: &str,
    filename: &str,
    object_stem: &str,
    extension: &str,
    time_partition_granularity: Option<&str>,
    time_partition_prefix: Option<&str>,
) -> String {
    let namespace = BufferChunker::decode_file_namespace(filename);
    let prefix = prefix.trim_matches('/');
    let mut parts = Vec::new();
    if !prefix.is_empty() {
        parts.push(prefix.to_string());
    }
    if !namespace.is_empty() {
        parts.push(namespace);
    }
    let partition_path = BufferChunker::decode_file_partition(filename);
    if !partition_path.is_empty() {
        parts.push(partition_path);
    }
    if let Ok(time_key) = TimePartitioner::new(filename)
        .process_from_layout(time_partition_granularity, time_partition_prefix)
    {
        let time_key = time_key.trim_matches('/');
        if !time_key.is_empty() {
            parts.push(time_key.to_string());
        }
    }
    let extension = extension.trim_start_matches('.');
    let file = if extension.is_empty() {
        object_stem.to_string()
    } else {
        format!("{object_stem}.{extension}")
    };
    parts.push(file);
    parts.join("/")
}
