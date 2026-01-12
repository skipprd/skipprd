use tracing::info;

fn dbt_base_prefix(pipeline: &str) -> String {
    let tenant = crate::helpers::configuration::Config::get_tenant();
    let workspace = crate::helpers::configuration::Config::get_workspace_name();
    format!("{}/{}/{}/dbt", tenant, workspace, pipeline)
}

pub async fn scaffold_full_project(pipeline: &str, namespaces: &[String]) -> Result<Vec<String>, String> {
    // Ensure minimal project file exists
    ensure_minimal_project(pipeline).await?;
    let base = dbt_base_prefix(pipeline);
    let mut written: Vec<String> = Vec::new();

    // Write sources (schema.yml) enumerating all namespaces for this pipeline
    let sources_yaml = make_sources_yaml(pipeline, namespaces);
    let sources_key = format!("{}/models/schema.yml", base);
    crate::helpers::s3::put_bytes(&sources_key, sources_yaml.as_bytes(), "text/yaml")
        .await
        .map_err(|e| format!("{:?}", e))?;
    written.push(sources_key);

    // For each namespace, create a simple staging model selecting from the dbt source
    for ns in namespaces {
        let model_name = format!("stg_{}", ns);
        let sql_text = make_staging_model_sql(pipeline, ns);
        let key = format!("{}/models/{}/{}.sql", base, ns, model_name);
        crate::helpers::s3::put_bytes(&key, sql_text.as_bytes(), "text/sql")
            .await
            .map_err(|e| format!("{:?}", e))?;
        written.push(key);
    }

    info!("DBT scaffolding complete for pipeline '{}' ({} file(s))", pipeline, written.len());
    Ok(written)
}

fn make_sources_yaml(pipeline: &str, namespaces: &[String]) -> String {
    // Minimal version:2 sources block listing all namespaces as tables under one source
    let mut out = String::new();
    out.push_str("version: 2\n\n");
    out.push_str("sources:\n");
    out.push_str(&format!("  - name: {}\n", pipeline));
    out.push_str("    tables:\n");
    for ns in namespaces {
        out.push_str(&format!("      - name: {}\n", ns));
    }
    out
}

fn make_staging_model_sql(pipeline: &str, namespace: &str) -> String {
    format!(
        r#"{{{{ config(materialized="view") }}}}

select *
from {{{{ source('{pipeline}','{namespace}') }}}}
"#,
        pipeline = pipeline,
        namespace = namespace
    )
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


