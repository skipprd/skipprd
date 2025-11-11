use crate::discover::{PipelineMetadata};
use crate::sql::parser::TableDropStatement;

/// Removes a table from the metadata and deletes local/S3 artifacts (stats, semantic, catalog, data)
pub async fn drop_table(pipeline_metadata: &mut PipelineMetadata, stmt: &TableDropStatement) -> Result<(), String> {
    // Determine pipeline name from statement
    let table_str = format!("{}", stmt.table);
    let pipeline = match &stmt.schema {
        Some(schema) => format!("{}", schema),
        None => table_str.clone(),
    };

    // Remove from in-memory metadata
    pipeline_metadata.metadata.remove(&table_str);

    let ns = &table_str; // namespace typically equals table

    // S3 cleanup if online
    {
        let tenant = crate::helpers::configuration::Config::get_tenant();
        let workspace = crate::helpers::configuration::Config::get_workspace_name();
        let prefix_root = format!("{}/{}/{}", tenant, workspace, pipeline);
        // Guard against unsafe deletions
        if !tenant.is_empty() && !workspace.is_empty() && !pipeline.is_empty() && !prefix_root.starts_with('/') && prefix_root.contains('/') {
            // Delete stats/semantic/catalog objects
            if !ns.is_empty() && ns != "/" {
                let _ = crate::helpers::s3::delete_prefix(&format!("{}/stats/{}", prefix_root, ns)).await;
                let _ = crate::helpers::s3::delete_prefix(&format!("{}/semantic/{}", prefix_root, ns)).await;
                let _ = crate::helpers::s3::delete_prefix(&format!("{}/catalog/{}", prefix_root, ns)).await;
                // Delete parquet data under manifest-defined prefixes for this namespace
                if let Some(man) = crate::helpers::configuration::Config::read_manifest(ns).await {
                    if let Some(tables) = man.get("tables").and_then(|t| t.as_object()) {
                        if let Some(ns_obj) = tables.get(ns).and_then(|v| v.as_object()) {
                            if let Some(prefixes) = ns_obj.get("prefixes").and_then(|p| p.as_array()) {
                                for v in prefixes {
                                    if let Some(s3_url) = v.as_str() {
                                        if let Some(rest) = s3_url.strip_prefix("s3://") {
                                            if let Some((bucket, prefix)) = rest.split_once('/') {
                                                let mut p = prefix.to_string();
                                                if !p.ends_with('/') { p.push('/'); }
                                                let _ = crate::helpers::s3::delete_prefix_in_bucket(bucket, &p).await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Delete deadletters for this namespace if configured
                if let Ok(plugin) = crate::helpers::configuration::Config::get_pipline_plugin_config("deadletter") {
                    if let crate::helpers::configuration::PluginConfig::S3(conf) = plugin {
                        let mut p = conf.s3_prefix.trim_matches('/').to_string();
                        if p.is_empty() { p = ns.to_string(); } else { p = format!("{}/{}", p, ns); }
                        if !p.ends_with('/') { p.push('/'); }
                        if !conf.s3_bucket.is_empty() { let _ = crate::helpers::s3::delete_prefix_in_bucket(&conf.s3_bucket, &p).await; }
                    }
                }
            }
            // Delete pipeline data prefixes if present
            let _ = crate::helpers::s3::delete_prefix(&format!("{}/wal/", prefix_root)).await;
            let _ = crate::helpers::s3::delete_prefix(&format!("{}/parquet/", prefix_root)).await;
        }
    }

    Ok(())
} 