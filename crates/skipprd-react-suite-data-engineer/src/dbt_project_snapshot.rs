use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectFileFacts {
    #[serde(default)]
    pub sql_models: BTreeMap<String, ProjectSqlModel>,
    #[serde(default)]
    pub schema_models: BTreeMap<String, ProjectSchemaModel>,
    #[serde(default)]
    pub manifest_models: BTreeMap<String, ProjectManifestModel>,
    #[serde(default)]
    pub manifest_available: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectSqlModel {
    pub path: String,
    pub model_name: String,
    #[serde(default)]
    pub columns: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectSchemaModel {
    pub path: String,
    pub model_name: String,
    #[serde(default)]
    pub columns: Vec<ProjectColumn>,
    pub in_staging_dir: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectManifestModel {
    pub path: Option<String>,
    pub model_name: String,
    #[serde(default)]
    pub columns: Vec<ProjectColumn>,
    #[serde(default)]
    pub relation_fqn: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectColumn {
    pub name: String,
    #[serde(default)]
    pub data_type: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

pub(crate) async fn scan_project_files(
    ctx: &react_core::agent::AgentCtx,
) -> Result<ProjectFileFacts, String> {
    let mut facts = ProjectFileFacts::default();
    facts.sql_models = scan_sql_models(ctx).await?;
    facts.schema_models = scan_schema_models(ctx).await?;
    facts.manifest_models = scan_manifest_models(ctx).await?;
    facts.manifest_available = crate::project_fs::project_file_exists(ctx, "target/manifest.json")
        .await
        .unwrap_or(false);
    Ok(facts)
}

async fn scan_sql_models(
    ctx: &react_core::agent::AgentCtx,
) -> Result<BTreeMap<String, ProjectSqlModel>, String> {
    let rels = crate::project_fs::list_project_files(ctx, "models/").await?;
    let mut out = BTreeMap::new();
    for rel in rels {
        if !rel.ends_with(".sql") || rel.contains("/_versions/") {
            continue;
        }
        let Some(model_name) = model_name_from_path(&rel) else {
            continue;
        };
        let Some(text) = crate::project_fs::read_project_file_text(ctx, &rel).await? else {
            continue;
        };
        let columns = crate::tools::files_tool::extract_final_select_output_columns(&text)
            .map(|columns| columns.into_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        out.insert(
            model_name.clone(),
            ProjectSqlModel {
                path: rel,
                model_name,
                columns,
            },
        );
    }
    Ok(out)
}

async fn scan_schema_models(
    ctx: &react_core::agent::AgentCtx,
) -> Result<BTreeMap<String, ProjectSchemaModel>, String> {
    let rels = crate::project_fs::list_project_files(ctx, "models/").await?;
    let mut out = BTreeMap::new();
    for rel in rels {
        if !(rel.ends_with(".yml") || rel.ends_with(".yaml")) || rel.contains("/_versions/") {
            continue;
        }
        let Some(text) = crate::project_fs::read_project_file_text(ctx, &rel).await? else {
            continue;
        };
        let root: serde_yaml::Value = match serde_yaml::from_str(&text) {
            Ok(root) => root,
            Err(_) => continue,
        };
        let Some(models) = root
            .as_mapping()
            .and_then(|mapping| mapping.get(serde_yaml::Value::String("models".to_string())))
            .and_then(|value| value.as_sequence())
        else {
            continue;
        };
        for model in models {
            let Some(mapping) = model.as_mapping() else {
                continue;
            };
            let Some(model_name) = yaml_str(mapping, "name")
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|name| name.to_string())
            else {
                continue;
            };
            let key = schema_model_key(&rel, &model_name);
            out.insert(
                key,
                ProjectSchemaModel {
                    path: rel.clone(),
                    model_name,
                    columns: yaml_columns(mapping),
                    in_staging_dir: rel.starts_with("models/staging/"),
                },
            );
        }
    }
    Ok(out)
}

async fn scan_manifest_models(
    ctx: &react_core::agent::AgentCtx,
) -> Result<BTreeMap<String, ProjectManifestModel>, String> {
    let Some(text) = crate::project_fs::read_project_file_text(ctx, "target/manifest.json").await?
    else {
        return Ok(BTreeMap::new());
    };
    let manifest: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("failed to parse target/manifest.json: {e}"))?;
    let mut out = BTreeMap::new();
    let Some(nodes) = manifest.get("nodes").and_then(|value| value.as_object()) else {
        return Ok(out);
    };
    for node in nodes.values() {
        let resource_type = node
            .get("resource_type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if resource_type != "model" {
            continue;
        }
        let Some(model_name) = node
            .get("name")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| name.to_string())
        else {
            continue;
        };
        let path = node
            .get("original_file_path")
            .or_else(|| node.get("path"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| path.to_string());
        out.insert(
            model_name.clone(),
            ProjectManifestModel {
                path,
                model_name,
                columns: manifest_columns(node.get("columns")),
                relation_fqn: build_manifest_relation_fqn(node),
            },
        );
    }
    Ok(out)
}

fn model_name_from_path(rel: &str) -> Option<String> {
    std::path::Path::new(rel)
        .file_stem()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string())
}

fn schema_model_key(path: &str, model_name: &str) -> String {
    format!("{}:{}", path.trim(), model_name.trim())
}

fn yaml_str<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a str> {
    mapping
        .get(serde_yaml::Value::String(key.to_string()))
        .and_then(|value| value.as_str())
}

fn yaml_columns(mapping: &serde_yaml::Mapping) -> Vec<ProjectColumn> {
    let Some(columns) = mapping
        .get(serde_yaml::Value::String("columns".to_string()))
        .and_then(|value| value.as_sequence())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for column in columns {
        let Some(column_map) = column.as_mapping() else {
            continue;
        };
        let Some(name) = yaml_str(column_map, "name")
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        out.push(ProjectColumn {
            name: name.to_string(),
            data_type: yaml_str(column_map, "data_type")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string()),
            description: yaml_str(column_map, "description")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string()),
        });
    }
    out.sort_by(|left, right| left.name.cmp(&right.name));
    out
}

fn manifest_columns(value: Option<&serde_json::Value>) -> Vec<ProjectColumn> {
    let Some(columns) = value.and_then(|value| value.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (fallback_name, column) in columns {
        let name = column
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or(fallback_name.as_str())
            .trim();
        if name.is_empty() {
            continue;
        }
        out.push(ProjectColumn {
            name: name.to_string(),
            data_type: column
                .get("data_type")
                .or_else(|| column.get("type"))
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string()),
            description: column
                .get("description")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string()),
        });
    }
    out.sort_by(|left, right| left.name.cmp(&right.name));
    out
}

fn build_manifest_relation_fqn(node: &serde_json::Value) -> Option<String> {
    let database = node
        .get("database")
        .and_then(|value| value.as_str())?
        .trim();
    let schema = node.get("schema").and_then(|value| value.as_str())?.trim();
    let alias = node
        .get("alias")
        .or_else(|| node.get("name"))
        .and_then(|value| value.as_str())?
        .trim();
    if database.is_empty() || schema.is_empty() || alias.is_empty() {
        return None;
    }
    Some(format!("{database}.{schema}.{alias}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use react_core::storage::StorageAdapter;
    use react_module_storage_memory::InMemoryStorageAdapter;
    use std::sync::Arc;

    #[tokio::test]
    async fn scan_project_files_reads_sql_yaml_and_manifest() {
        let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::default());
        let ctx = crate::project_fs::test_helpers::make_ctx(storage.clone(), None);
        storage
            .put_bytes(
                &crate::project_fs::join_storage_key(
                    &ctx,
                    "models/staging/stg_raw_orders.sql",
                ),
                b"select\n  order_id,\n  cast(created_at as timestamp) as created_at\nfrom {{ source('RAW','ORDERS') }}",
                "text/sql",
            )
            .await
            .expect("write sql");
        storage
            .put_bytes(
                &crate::project_fs::join_storage_key(
                    &ctx,
                    "models/staging/stg_raw_orders.yml",
                ),
                b"version: 2\nmodels:\n  - name: stg_raw_orders\n    columns:\n      - name: order_id\n        data_type: text\n      - name: created_at\n        data_type: timestamp\n",
                "text/yaml",
            )
            .await
            .expect("write yaml");
        storage
            .put_bytes(
                &crate::project_fs::join_storage_key(&ctx, "target/manifest.json"),
                br#"{"nodes":{"model.p.stg_raw_orders":{"resource_type":"model","name":"stg_raw_orders","original_file_path":"models/staging/stg_raw_orders.sql","database":"ANALYTICS","schema":"SILVER","alias":"stg_raw_orders","columns":{"order_id":{"name":"order_id","data_type":"text"}}}}}"#,
                "application/json",
            )
            .await
            .expect("write manifest");

        let facts = scan_project_files(&ctx).await.expect("scan facts");
        assert!(facts.manifest_available);
        assert_eq!(
            facts
                .sql_models
                .get("stg_raw_orders")
                .expect("sql model")
                .columns,
            vec!["created_at".to_string(), "order_id".to_string()]
        );
        assert!(facts
            .schema_models
            .values()
            .any(|model| model.model_name == "stg_raw_orders"
                && model.in_staging_dir
                && model.columns.len() == 2));
        assert_eq!(
            facts
                .manifest_models
                .get("stg_raw_orders")
                .and_then(|model| model.relation_fqn.as_deref()),
            Some("ANALYTICS.SILVER.stg_raw_orders")
        );
    }
}
