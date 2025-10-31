use std::collections::HashMap;

pub struct Orchestrator;

impl Orchestrator {
    pub async fn build(namespace: &str) {
        let ctx = crate::catalog::session::SessionFactory::new_context().await;
        crate::catalog::session::SessionFactory::register_s3_and_wal(&ctx, namespace).await;

        // Register S3 tables using registry prefixes; fallback to configured output parquet location
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        let mut prefixes: Vec<String> = Vec::new();
        if let Some(entry) = crate::sql::registry::find_entry(&pipeline, namespace).await {
            prefixes.extend(entry.data_prefixes);
        }
        // Fallback: use the configured s3 parquet base for this namespace/pipeline
        let s3_loc = crate::helpers::configuration::Config::get_output_parquet_s3_location(&pipeline).unwrap_or_default();
        if prefixes.is_empty() && !s3_loc.is_empty() { prefixes.push(s3_loc.clone()); }
        // Normalize to s3://bucket/dir/
        let bucket = url::Url::parse(&s3_loc).ok().and_then(|u| u.host_str().map(|s| s.to_string())).unwrap_or_default();
        println!("META: orchestrator ns='{}' prefixes_before_norm={}", namespace, prefixes.len());
        for p in prefixes.iter_mut() {
            if !p.starts_with("s3://") {
                let mut dir = p.trim_matches('/').to_string();
                if !dir.ends_with('/') { dir.push('/'); }
                *p = format!("s3://{}/{}", bucket, dir);
            }
        }
        for (idx, path) in prefixes.iter().enumerate() {
            let tname = format!("{}_s3_{}", namespace, idx);
            let _ = ctx.register_parquet(&tname, path, datafusion::prelude::ParquetReadOptions::default()).await;
        }
        println!("META: orchestrator ns='{}' registered_sources={}", namespace, prefixes.len());

        // Stats → Catalog (+LLM in build_catalog tail)
        // Full S3 scan for authoritative stats (0 => no limit). For now, apply a small limit for fast runs.
        crate::catalog::stats_builder::StatsBuilder::compute_and_write(&ctx, namespace, 10).await.expect("TODO: panic message");
        crate::catalog::catalog::CatalogBuilder::build_and_write(namespace).await;
        // Defer dataset-level LLM enrichment to the end-of-discover pass
    }

    pub async fn build_all(namespaces: &HashMap<String, crate::discover::Metadata>) {
        // Prefer namespaces from registry to ensure S3-backed list, fallback to metadata keys
        let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
        let mut regs = crate::sql::registry::list_namespaces(&pipeline).await;
        if regs.is_empty() {
            regs = namespaces.keys().cloned().collect();
        }
        for ns in regs { Self::build(&ns).await; }
    }
}


