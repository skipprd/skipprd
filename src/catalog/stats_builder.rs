use datafusion::prelude::SessionContext;

pub struct StatsBuilder;

impl StatsBuilder {
    pub async fn compute_and_write(ctx: &SessionContext, namespace: &str, limit_n: i64) -> Result<(), String> {
        // Build union of S3 and WAL already registered by SessionFactory
        // Prefer registering S3 table names via manifest in caller; fallback use namespace as view
        // Attempt to union all registered S3 tables matching pattern "<ns>_s3_*"
        let mut sources: Vec<String> = Vec::new();
        for i in 0..64 {
            let t = format!("{}_s3_{}", namespace, i);
            if ctx.table(&t).await.is_ok() { sources.push(t); } else { break; }
        }
        if sources.is_empty() {
            // try single path registration fallback: namespace table already registered by caller
            // noop here; sampling may fail gracefully
        }
        if sources.is_empty() { return Ok(()); }
        let first = ctx.table(&sources[0]).await.map_err(|e| e.to_string())?;
        let mut acc = first;
        for t in sources.iter().skip(1) {
            if let Ok(df_next) = ctx.table(t).await { if let Ok(u) = acc.clone().union(df_next) { acc = u; } }
        }
        // Union with WAL
        let df_wal = match ctx.table(&format!("{}_wal", &namespace)).await { Ok(df) => df, Err(_) => acc.clone().filter(datafusion::logical_expr::lit(false)).unwrap() };
        let df_union = acc.union(df_wal).map_err(|e| e.to_string())?;
        // Register view for sampling
        if let Ok(plan) = df_union.into_optimized_plan() { if let Ok(view) = datafusion::datasource::view::ViewTable::try_new(plan, Some(namespace.to_string())) { let _ = ctx.register_table(namespace, std::sync::Arc::new(view)); } }
        let sql = format!("SELECT * FROM {} LIMIT {}", namespace, limit_n);
        if let Ok(df) = ctx.sql(&sql).await { if let Ok(batches) = df.collect().await {
            let mut ns_stats = crate::discover::stats::NamespaceStats::new(namespace);
            for b in batches {
                let schema = b.schema();
                for (i, f) in schema.fields().iter().enumerate() {
                    let top = f.name().clone();
                    let col = b.column(i);
                    for r in 0..b.num_rows() {
                        let cell = crate::sql::tui::array_cell_to_json(col.as_ref(), r);
                        fn update_recursive(ns_stats: &mut crate::discover::stats::NamespaceStats, prefix: &str, v: &serde_json::Value) {
                            match v {
                                serde_json::Value::Object(map) => {
                                    for (k, vv) in map.iter() { let next = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) }; update_recursive(ns_stats, &next, vv); }
                                }
                                serde_json::Value::Array(arr) => {
                                    let mut count = 0usize; for el in arr { update_recursive(ns_stats, prefix, el); count += 1; if count >= 64 { break; } }
                                }
                                scalar => { ns_stats.update_field(prefix, scalar); }
                            }
                        }
                        update_recursive(&mut ns_stats, &top, &cell);
                    }
                }
            }
            for (_k, fs) in ns_stats.fields.iter_mut() { fs.finalize(); }
            crate::helpers::configuration::Config::write_namespace_stats_async(namespace, &ns_stats).await;
        }}
        Ok(())
    }
}


