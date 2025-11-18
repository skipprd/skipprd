use tracing::info;

fn dbt_base_prefix(pipeline: &str) -> String {
    let tenant = crate::helpers::configuration::Config::get_tenant();
    let workspace = crate::helpers::configuration::Config::get_workspace_name();
    format!("{}/{}/{}/dbt", tenant, workspace, pipeline)
}

pub async fn write_model_sql(pipeline: &str, namespace: &str, name: &str, sql: &str) -> Result<String, String> {
    let key = format!("{}/models/{}/{}.sql", dbt_base_prefix(pipeline), namespace, name);
    crate::helpers::s3::put_bytes(&key, sql.as_bytes(), "text/sql").await.map_err(|e| format!("{:?}", e))?;
    info!("DBT model written: s3://{}/{}", crate::helpers::configuration::Config::get_skippr_s3_bucket(), key);
    Ok(key)
}

pub async fn write_metricflow_yaml(pipeline: &str, namespace: &str, name: &str, yaml_text: &str) -> Result<String, String> {
    let key = format!("{}/metrics/{}/{}.yaml", dbt_base_prefix(pipeline), namespace, name);
    crate::helpers::s3::put_bytes(&key, yaml_text.as_bytes(), "text/yaml").await.map_err(|e| format!("{:?}", e))?;
    info!("MetricFlow YAML written: s3://{}/{}", crate::helpers::configuration::Config::get_skippr_s3_bucket(), key);
    Ok(key)
}


