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
        println!("META: stats_builder ns='{}' found_s3_tables={}", namespace, sources.len());
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
        // Authoritative stats via aggregate queries (per-column), no row iteration
        // Register union as view under namespace for SQL ease
        if let Ok(plan) = df_union.clone().into_optimized_plan() {
            if let Ok(view) = datafusion::datasource::view::ViewTable::try_new(plan, Some(namespace.to_string())) {
                let _ = ctx.register_table(namespace, std::sync::Arc::new(view));
            }
        }
        // Fall back to authoritative row streaming with deep recursion for all nested structures
        let max_rows: usize = if limit_n > 0 { limit_n as usize } else { usize::MAX };
        if max_rows != usize::MAX { println!("META: stats row_limit applied ns='{}' limit_n={}", namespace, max_rows); }
        let mut ns_stats = crate::discover::stats::NamespaceStats::new(namespace);
        let mut stream = df_union.execute_stream().await.map_err(|e| e.to_string())?;
        use futures::StreamExt;
        let mut processed_rows: usize = 0;
        'batches: while let Some(batch_res) = stream.next().await {
            let b = batch_res.map_err(|e| e.to_string())?;
            let schema = b.schema();
            // Determine how many rows from this batch to process, honoring limit
            let remaining = max_rows.saturating_sub(processed_rows);
            let take_rows = std::cmp::min(b.num_rows(), remaining);
            for (i, f) in schema.fields().iter().enumerate() {
                let top = f.name().clone();
                println!("META: stats processing ns='{}' field='{}' dtype='{:?}' rows={}", namespace, top, f.data_type(), b.num_rows());
                let col = b.column(i);
                for r in 0..take_rows {
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
            processed_rows = processed_rows.saturating_add(take_rows);
            if processed_rows >= max_rows { break 'batches; }
        }
        let mut fields_vec: Vec<String> = ns_stats.fields.keys().cloned().collect(); fields_vec.sort();
        println!("META: stats built ns='{}' fields={} sample=[{}]", namespace, fields_vec.len(), fields_vec.iter().take(12).cloned().collect::<Vec<_>>().join(","));
        for (_k, fs) in ns_stats.fields.iter_mut() { fs.finalize(); }
        crate::helpers::configuration::Config::write_namespace_stats_async(namespace, &ns_stats).await;
        Ok(())
    }
}


