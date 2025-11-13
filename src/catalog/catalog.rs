use tracing::debug;

pub struct CatalogBuilder;

impl CatalogBuilder {
	pub async fn build_and_write_with_stats(namespace: &str, ns_stats: Option<crate::discover::stats::NamespaceStats>, dataset_stats: Option<crate::catalog::model::DatasetStats>) {
		// Build initial semantic view directly from provided stats to ensure first-time catalogs have fields
		fn classify_field(_name: &str, stats: Option<&crate::discover::stats::FieldStats>) -> crate::catalog::model::SemanticFieldRole {
			// Name-agnostic classification using only stats
			if let Some(s) = stats {
				if s.min_numeric.is_some() || s.max_numeric.is_some() {
					return crate::catalog::model::SemanticFieldRole::Metric;
				}
				if s.max_len.unwrap_or(0) > 64 {
					return crate::catalog::model::SemanticFieldRole::FreeText;
				}
				return crate::catalog::model::SemanticFieldRole::Categorical;
			}
			crate::catalog::model::SemanticFieldRole::Categorical
		}
		let (mut semantic_fields, mut semantic_dims, mut semantic_metrics): (Vec<crate::catalog::model::SemanticField>, Vec<String>, Vec<String>) = {
			if let Some(ns) = ns_stats.as_ref() {
				let mut fields: Vec<crate::catalog::model::SemanticField> = Vec::new();
				for (fname, fstats) in ns.fields.iter() {
					let role = classify_field(fname, Some(fstats));
					fields.push(crate::catalog::model::SemanticField { name: fname.clone(), role });
				}
				let mut dims: Vec<String> = Vec::new();
				let mut mets: Vec<String> = Vec::new();
				for f in &fields {
					match f.role {
						crate::catalog::model::SemanticFieldRole::Id |
						crate::catalog::model::SemanticFieldRole::Timestamp |
						crate::catalog::model::SemanticFieldRole::Categorical => dims.push(f.name.clone()),
						crate::catalog::model::SemanticFieldRole::Metric => mets.push(f.name.clone()),
						crate::catalog::model::SemanticFieldRole::FreeText => {}
					}
				}
				(fields, dims, mets)
			} else {
				(Vec::new(), Vec::new(), Vec::new())
			}
		};
		// Keep flat names; allow shallow navigation via structure_index
		fn build_structure_index(names: &[String]) -> std::collections::HashMap<String, Vec<String>> {
			let mut idx: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
			for n in names {
				// For a.b.c -> parents: a, a.b
				let parts: Vec<&str> = n.split('.').collect();
				for i in 0..parts.len().saturating_sub(1) {
					let parent = parts[0..=i].join(".");
					let child = parts[0..=i+1].join(".");
					let e = idx.entry(parent).or_insert_with(Vec::new);
					if !e.iter().any(|x| x == &child) {
						e.push(child);
					}
				}
			}
			// Sort children for stable output
			for (_k, v) in idx.iter_mut() { v.sort(); v.dedup(); }
			idx
		}
		fn to_stats_lite(s: &crate::discover::stats::FieldStats) -> crate::catalog::model::FieldStatsLite {
			crate::catalog::model::FieldStatsLite {
				total: s.total,
				nulls: s.nulls,
				min_numeric: s.min_numeric,
				max_numeric: s.max_numeric,
				min_len: s.min_len,
				max_len: s.max_len,
				approx_distinct: s.approx_distinct,
				histogram_bins: s.histogram_bins.clone(),
				histogram_min: s.histogram_min,
				histogram_max: s.histogram_max,
				last_updated_epoch_ms: s.last_updated_epoch_ms,
			}
		}
		let mut catalog = crate::catalog::model::DataCatalog {
			namespace: namespace.to_string(),
			description: None,
			dimensions: semantic_dims.clone(),
			metrics: semantic_metrics.clone(),
			fields: semantic_fields.iter().map(|f| crate::catalog::model::CatalogField {
				entity: String::new(),
				name: f.name.clone(),
				description: None,
				synonyms: None,
				pii_sensitivity: None,
				units_or_format: None,
				role: Some(format!("{:?}", f.role)),
				stats: None,
			}).collect(),
			structure_index: std::collections::HashMap::new(),
			dataset_stats,
		};
		// Embed per-field stats if provided
		if let Some(ns) = ns_stats.as_ref() {
			let mut nulls_by_field: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
			for cf in catalog.fields.iter_mut() {
				if let Some(fs) = ns.fields.get(&cf.name) {
					nulls_by_field.insert(cf.name.clone(), fs.nulls);
					cf.stats = Some(to_stats_lite(fs));
				}
			}
			if let Some(ds) = catalog.dataset_stats.as_mut() {
				ds.nulls_by_field = nulls_by_field;
			}
		}
		// Build structure index
		let field_names: Vec<String> = catalog.fields.iter().map(|f| f.name.clone()).collect();
		catalog.structure_index = build_structure_index(&field_names);
		debug!("META: build catalog ns='{}' fields={} sample=[{}]", namespace, catalog.fields.len(), catalog.fields.iter().take(8).map(|f| f.name.clone()).collect::<Vec<_>>().join(","));
		// Defer field-level LLM enrichment to end-of-discover pass
		crate::helpers::configuration::Config::write_catalog_async(namespace, &catalog).await;
	}
}
