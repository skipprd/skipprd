use datafusion::prelude::SessionContext;
use tracing::debug;

pub struct StatsBuilder;

impl StatsBuilder {
	pub async fn compute(ctx: &SessionContext, namespace: &str, _limit_n: i64) -> Result<(crate::discover::stats::NamespaceStats, crate::catalog::model::DatasetStats), String> {
		// Use the registered namespace view
		let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
		let fqn = format!("{}.{}", pipeline, namespace);
		let df = ctx.table(fqn).await.map_err(|e| e.to_string())?;
		let schema = df.schema();

		// Enumerate fields from schema (top-level and nested structs)
		fn walk_fields(prefix: &str, f: &datafusion::arrow::datatypes::Field, out: &mut Vec<String>) {
			use datafusion::arrow::datatypes::DataType as ADT;
			match f.data_type() {
				ADT::Struct(fields) => {
					let base = if prefix.is_empty() { f.name().clone() } else { format!("{}.{}", prefix, f.name()) };
					for child in fields {
						walk_fields(&base, child, out);
					}
				}
				_ => {
					let name = if prefix.is_empty() { f.name().clone() } else { format!("{}.{}", prefix, f.name()) };
					out.push(name);
				}
			}
		}
		let mut field_names: Vec<String> = Vec::new();
		for f in schema.fields() { walk_fields("", f, &mut field_names); }
		field_names.sort();
		field_names.dedup();

        let mut ns_stats = crate::discover::stats::NamespaceStats::new(namespace);

		// Dataset row count (via SQL to avoid version-specific aggregate imports)
		let total_rows: u64 = {
			let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
			let sql = format!("SELECT COUNT(1) AS __cnt FROM \"{}\".\"{}\"", pipeline, namespace);
			match ctx.sql(&sql).await {
				Ok(df_cnt) => match df_cnt.collect().await {
					Ok(batches) => {
						if let Some(b) = batches.get(0) {
							let s = crate::sql::tui::value_to_string(b.column(0).as_ref(), 0);
							s.parse::<u64>().unwrap_or(0)
						} else { 0 }
					}
					Err(_) => 0
				},
				Err(_) => 0
			}
		};

		// Per-field aggregates via SQL (COUNT, COUNT(DISTINCT), MIN, MAX, NULLS)
		for name in field_names.iter() {
			let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
			let sql = format!(
				"SELECT \
					COUNT(\"{col}\") AS non_null, \
					COUNT(DISTINCT \"{col}\") AS distinct_cnt, \
					MIN(\"{col}\") AS min_v, \
					MAX(\"{col}\") AS max_v, \
					SUM(CASE WHEN \"{col}\" IS NULL THEN 1 ELSE 0 END) AS nulls \
				 FROM \"{schema}\".\"{table}\"",
				col = name, schema = pipeline, table = namespace
			);
			let mut fs = crate::discover::stats::FieldStats::default();
			match ctx.sql(&sql).await {
				Ok(df_agg) => match df_agg.collect().await {
					Ok(batches) => {
						if let Some(b) = batches.get(0) {
							let non_null = crate::sql::tui::value_to_string(b.column(0).as_ref(), 0).parse::<u64>().unwrap_or(0);
							let approx_distinct = crate::sql::tui::value_to_string(b.column(1).as_ref(), 0).parse::<u64>().ok();
							let min_v = crate::sql::tui::value_to_string(b.column(2).as_ref(), 0);
							let max_v = crate::sql::tui::value_to_string(b.column(3).as_ref(), 0);
							let nulls = crate::sql::tui::value_to_string(b.column(4).as_ref(), 0).parse::<u64>().unwrap_or(0);
							fs.total = total_rows;
							fs.nulls = nulls;
							fs.approx_distinct = approx_distinct;
							fs.min_numeric = min_v.parse::<f64>().ok();
							fs.max_numeric = max_v.parse::<f64>().ok();
						}
					}
					Err(_) => {}
				},
				Err(_) => {}
			}
			fs.finalize();
			ns_stats.fields.insert(name.clone(), fs);
		}

		let mut ds = crate::catalog::model::DatasetStats::default();
		ds.approx_total_rows = total_rows;
		debug!("META: stats built ns='{}' fields={}", namespace, ns_stats.fields.len());
		Ok((ns_stats, ds))
    }
}


