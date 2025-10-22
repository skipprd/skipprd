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

    // Compute local cache file paths
    let ns = &table_str; // namespace typically equals table
    let stats_path = crate::helpers::configuration::Config::get_stats_local_path(ns);
    let sem_path = crate::helpers::configuration::Config::get_semantic_local_path(ns);
    let cat_path = crate::helpers::configuration::Config::get_catalog_local_path(ns);

    // Best-effort local deletions
    let _ = std::fs::remove_file(&stats_path);
    let _ = std::fs::remove_file(&sem_path);
    let _ = std::fs::remove_file(&cat_path);

    // Best-effort local data dir deletion: wal and parquet for pipeline
    let base = crate::helpers::configuration::Config::get_pipeline_data_dir();
    let wal_dir = format!("{}/wal/{}", base, pipeline);
    let parquet_dir = format!("{}/parquet/{}", base, pipeline);
    let _ = std::fs::remove_dir_all(&wal_dir);
    let _ = std::fs::remove_dir_all(&parquet_dir);

    // Remove the pipeline-scoped local directory ./data/<workspace>_<pipeline>
    // Compute path without creating it (avoid get_data_dir which ensures creation)
    let ws_pl = crate::helpers::configuration::Config::get_full_namespace_name();
    let mut base_clean = base.clone(); while base_clean.ends_with('/') { base_clean.pop(); }
    if !ws_pl.is_empty() && !base_clean.is_empty() {
        let local_root = format!("{}/{}", base_clean, ws_pl);
        // Safety: ensure we don't accidentally delete the base directory itself or root
        if local_root.starts_with(&base_clean)
            && local_root.len() > base_clean.len() + 1
            && local_root != base_clean
            && local_root != "/" && local_root != "." && local_root != ".." {
            let _ = std::fs::remove_dir_all(&local_root);
        }
    }

    // S3 cleanup if online
    if crate::helpers::configuration::Config::truth_value(&crate::helpers::configuration::Config::getenv("SKIPPR_OFFLINE", "false")) == false {
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
                // Delete parquet data under data_outputs (Athena) bucket/prefix for this namespace
                if let Some(s3_url) = crate::helpers::configuration::Config::get_output_parquet_s3_location(ns) {
                    if let Some(rest) = s3_url.strip_prefix("s3://") {
                        if let Some((bucket, prefix)) = rest.split_once('/') {
                            let mut p = prefix.to_string();
                            // Ensure we only delete the namespace directory within the prefix
                            if !p.ends_with('/') { p.push('/'); }
                            let _ = crate::helpers::s3::delete_prefix_in_bucket(bucket, &p).await;
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