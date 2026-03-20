use crate::discover::PipelineMetadata;
use crate::sqlrt::parser::TableDropStatement;

/// Removes a table from the metadata and deletes local/S3 artifacts (stats, semantic, catalog, data)
pub async fn drop_table(
    pipeline_metadata: &mut PipelineMetadata,
    stmt: &TableDropStatement,
) -> Result<(), String> {
    // Determine pipeline name from statement
    let table_str = format!("{}", stmt.table);
    let pipeline = match &stmt.schema {
        Some(schema) => format!("{}", schema),
        None => table_str.clone(),
    };

    // Remove from in-memory metadata
    pipeline_metadata.metadata.remove(&table_str);

    let ns = &table_str; // namespace typically equals table

    {
        let tenant = crate::helpers::configuration::Config::get_tenant();
        let workspace = crate::helpers::configuration::Config::get_workspace_name();
        let prefix_root = format!("{}/{}/{}", tenant, workspace, pipeline);
        if !tenant.is_empty()
            && !workspace.is_empty()
            && !pipeline.is_empty()
            && !prefix_root.starts_with('/')
            && prefix_root.contains('/')
        {
            let storage = crate::adapters::storage::get_storage();
            if !ns.is_empty() && ns != "/" {
                let _ = storage
                    .delete_prefix(&format!("{}/stats/{}", prefix_root, ns))
                    .await;
                let _ = storage
                    .delete_prefix(&format!("{}/semantic/{}", prefix_root, ns))
                    .await;
                let _ = storage
                    .delete_prefix(&format!("{}/catalog/{}", prefix_root, ns))
                    .await;
                // Delete parquet data under manifest-defined prefixes (S3-only data plane)
                if let Some(man) = crate::helpers::manifest::Manifest::read(ns).await {
                    if let Some(tables) = man.get("tables").and_then(|t| t.as_object()) {
                        if let Some(ns_obj) = tables.get(ns).and_then(|v| v.as_object()) {
                            if let Some(prefixes) =
                                ns_obj.get("prefixes").and_then(|p| p.as_array())
                            {
                                for v in prefixes {
                                    if let Some(s3_url) = v.as_str() {
                                        if let Some(rest) = s3_url.strip_prefix("s3://") {
                                            if let Some((bucket, prefix)) = rest.split_once('/') {
                                                let mut p = prefix.to_string();
                                                if !p.ends_with('/') {
                                                    p.push('/');
                                                }
                                                let _ =
                                                    crate::helpers::s3::delete_prefix_in_bucket(
                                                        bucket, &p,
                                                    )
                                                    .await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let _ = storage
                .delete_prefix(&format!("{}/wal/", prefix_root))
                .await;
            let _ = storage
                .delete_prefix(&format!("{}/parquet/", prefix_root))
                .await;
        }
    }

    Ok(())
}
