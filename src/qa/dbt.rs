use serde_json::Value;
use tracing::info;

fn dbt_base_prefix(pipeline: &str) -> String {
    let tenant = crate::helpers::configuration::Config::get_tenant();
    let workspace = crate::helpers::configuration::Config::get_workspace_name();
    format!("{}/{}/{}/dbt", tenant, workspace, pipeline)
}

pub async fn write_model_sql(pipeline: &str, namespace: &str, name: &str, sql: &str) -> Result<String, String> {
    let key = format!("{}/models/{}/{}.sql", dbt_base_prefix(pipeline), namespace, name);
    let v = Value::String(sql.to_string());
    crate::helpers::s3::put_json(&key, &v).await.map_err(|e| format!("{:?}", e))?;
    info!("DBT model written: s3://{}/{}", crate::helpers::configuration::Config::get_skippr_s3_bucket(), key);
    Ok(key)
}

pub async fn write_metricflow_yaml(pipeline: &str, namespace: &str, name: &str, yaml_text: &str) -> Result<String, String> {
    let key = format!("{}/metrics/{}/{}.yaml", dbt_base_prefix(pipeline), namespace, name);
    // Store YAML as JSON string to reuse S3 helper; reader will convert back as needed
    let v = Value::String(yaml_text.to_string());
    crate::helpers::s3::put_json(&key, &v).await.map_err(|e| format!("{:?}", e))?;
    info!("MetricFlow YAML written: s3://{}/{}", crate::helpers::configuration::Config::get_skippr_s3_bucket(), key);
    Ok(key)
}


