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

pub async fn ensure_minimal_project(pipeline: &str) -> Result<(), String> {
    // Ensure a minimal dbt_project.yml exists under <base>/dbt_project.yml
    let base = dbt_base_prefix(pipeline);
    let project_key = format!("{}/dbt_project.yml", base);
    match crate::helpers::s3::head_etag(&project_key).await {
        Ok(Some(_)) => return Ok(()), // exists
        Ok(None) => {},
        Err(_) => {},
    }
    let name = format!("{}_project", pipeline.replace('/', "_"));
    let y = format!(
        "name: {name}\nversion: '1.0'\nprofile: '{pipeline}'\nmodel-paths: ['models']\nseed-paths: ['seeds']\nmacro-paths: ['macros']\ntarget-path: 'target'\n",
        name=name, pipeline=pipeline
    );
    crate::helpers::s3::put_bytes(&project_key, y.as_bytes(), "text/yaml").await.map_err(|e| format!("{:?}", e))?;
    info!("DBT minimal project created: s3://{}/{}", crate::helpers::configuration::Config::get_skippr_s3_bucket(), project_key);
    Ok(())
}


