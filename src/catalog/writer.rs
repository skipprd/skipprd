use std::collections::HashMap;

use crate::catalog::model::{DataCatalog, SemanticModel, SemanticFieldRole};
use crate::discover::stats::FieldStats;
use serde_json;

/// Enrich field descriptions, synonyms, PII and units in-place using LLM with stats and prior catalog context.
/// No-ops if pipeline/catalog LLM is disabled.
pub async fn enrich_field_descriptions_with_llm(namespace: &str, semantic: &SemanticModel, ns_stats: Option<&crate::discover::stats::NamespaceStats>, catalog: &mut DataCatalog) {
	if !(crate::helpers::configuration::Config::pipeline_llm_enabled() && crate::helpers::configuration::Config::catalog_llm_enabled()) {
		return;
	}
	let llm = crate::llm::create_llm(&crate::llm::config_from_env());

    fn normalize_base_name(field_name: &str) -> String {
        field_name.split('.').last().unwrap_or(field_name).to_string()
    }
    fn is_placeholder_or_garbage(s: &str) -> bool {
        let t = s.trim().to_lowercase();
        t.is_empty() || t.contains('<') || t.contains('≤') || t.contains("placeholder")
    }
    fn clean_synonyms(raw: Vec<String>, role: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in raw {
            let t = s.trim().to_lowercase();
            if t.len() < 3 { continue; }
            if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') { continue; }
            if t == "a" || t == "b" || t == "c" { continue; }
            out.push(t);
        }
        out.sort();
        out.dedup();
        if role.eq_ignore_ascii_case("id") {
            let mut id_syn = vec!["id","identifier","uuid","key"].into_iter().map(|s| s.to_string()).collect::<Vec<_>>();
            out.extend(id_syn);
            out.sort();
            out.dedup();
        }
        out
    }
    fn validate_pii(s: &str) -> Option<String> {
        let t = s.trim().to_lowercase();
        match t.as_str() { "none" | "low" | "medium" | "high" => Some(t), _ => None }
    }

	// Build a quick lookup for semantic roles
	let mut roles: HashMap<String, SemanticFieldRole> = HashMap::new();
	for sf in semantic.fields.iter() { roles.insert(sf.name.clone(), sf.role.clone()); }

	// Load existing catalog (if present) to obtain table description and prior field descriptions
	let mut table_description: Option<String> = None;
	let mut prior_field_desc: HashMap<String, String> = HashMap::new();
	{
		let pipeline = crate::helpers::configuration::Config::get_pipeline_name();
		if let Some(entry) = crate::sql::registry::find_entry(&pipeline, namespace).await {
			if !entry.catalog_key.is_empty() {
				if let Ok(val) = crate::helpers::s3::get_json(&entry.catalog_key).await {
					if let Some(desc) = val.get("description").and_then(|x| x.as_str()) { let trimmed = desc.trim(); if !trimmed.is_empty() { table_description = Some(trimmed.to_string()); } }
					if let Some(fields) = val.get("fields").and_then(|x| x.as_array()) {
						for f in fields {
							if let Some(name) = f.get("name").and_then(|x| x.as_str()) {
								if let Some(d) = f.get("description").and_then(|x| x.as_str()) { let s = d.trim(); if !s.is_empty() { prior_field_desc.insert(name.to_string(), s.to_string()); } }
							}
						}
					}
				}
			}
		}
	}

	// Snapshot names for borrow safety and track in-run enrichments
	let field_names: Vec<String> = catalog.fields.iter().map(|f| f.name.clone()).collect();
	let mut enriched_desc: HashMap<String, String> = HashMap::new();

	for (idx, fld) in catalog.fields.iter_mut().enumerate() {
		let role = roles.get(&fld.name).map(|r| format!("{:?}", r)).unwrap_or_else(|| "Unknown".to_string());
		let stats_for = ns_stats.and_then(|ns| ns.fields.get(&fld.name));
		let stats_snip = stats_for.map(|s| {
			let mut parts: Vec<String> = Vec::new();
			if let Some(d) = s.approx_distinct { parts.push(format!("distinct≈{}", d)); }
			if let Some(mn) = s.min_numeric { parts.push(format!("min={}", mn)); }
			if let Some(mx) = s.max_numeric { parts.push(format!("max={}", mx)); }
			if let Some(ml) = s.max_len { parts.push(format!("max_len={}", ml)); }
			parts.push(format!("nulls={}", s.nulls));
			parts.join(" ")
		}).unwrap_or_default();
		let others_ctx = {
			let mut parts: Vec<String> = Vec::new();
			let this_name = &field_names[idx];
			for name in field_names.iter() {
				if name == this_name { continue; }
				let desc_opt = enriched_desc.get(name).map(|s| s.as_str()).or_else(|| prior_field_desc.get(name).map(|s| s.as_str()));
				if let Some(d) = desc_opt { let t = d.trim(); if !t.is_empty() { parts.push(format!("{}: {}", name, t)); } }
			}
			parts.join("; ")
		};
		// LLM enrichment (separate prompts): description (no heuristic fallback)
		if fld.description.is_none() {
			let base_name = normalize_base_name(&fld.name);
			let prompt = format!(
				"Return STRICT JSON only with a single key 'description'.\
                Rules: one sentence ≤ 20 words; no placeholders; be specific to the field.\
                Example: {{\"description\":\"Unique identifier of the content.\"}}\
                Dataset: {ns}\nTable description: {td}\nOther fields: {ofs}\nField: {f}\nBaseName: {b}\nRole: {r}\nStats: {s}\n\nOutput JSON:",
				ns = namespace,
				td = table_description.clone().unwrap_or_else(|| "N/A".to_string()),
				ofs = if others_ctx.is_empty() { "N/A".to_string() } else { others_ctx.clone() },
				f = fld.name,
				b = base_name,
				r = role,
				s = stats_snip
			);
			let llm_clone = llm.clone();
			let prompt_clone = prompt.clone();
			let timeout_secs = crate::helpers::configuration::Config::catalog_llm_timeout_secs();
			let text_opt = if timeout_secs == 0 {
				match tokio::task::spawn_blocking(move || llm_clone.chat(&[crate::llm::ChatMessage { role: "user".into(), content: prompt_clone }])).await {
					Ok(Ok(t)) => Some(t),
					_ => None,
				}
			} else {
				match tokio::time::timeout(
					std::time::Duration::from_secs(timeout_secs),
					tokio::task::spawn_blocking(move || llm_clone.chat(&[crate::llm::ChatMessage { role: "user".into(), content: prompt }]))
				).await {
					Ok(Ok(Ok(t))) => Some(t),
					_ => None,
				}
			};
			if let Some(text) = text_opt {
				let desc_json = extract_json_value(&text).and_then(|v| v.get("description").and_then(|x| x.as_str()).map(|s| s.to_string()));
				if let Some(mut cleaned) = desc_json {
					if !is_placeholder_or_garbage(&cleaned) {
						fld.description.get_or_insert(cleaned.clone());
						enriched_desc.insert(fld.name.clone(), cleaned);
					}
				}
			}
		}

		// LLM enrichment (separate prompts): synonyms (no heuristic fallback)
		if fld.synonyms.is_none() {
			let base_name = normalize_base_name(&fld.name);
			let prompt = format!(
				"Return STRICT JSON only: {{\"synonyms\":[\"word1\",\"word2\",...]}}.\
                Rules: 3–6 single-word, lowercase, meaningful; no single letters; no placeholders; JSON only.\
                Dataset: {ns}\nField: {f}\nBaseName: {b}\nRole: {r}\n\nOutput JSON:",
				ns = namespace, f = fld.name, b = base_name, r = role
			);
			let llm_clone = llm.clone();
			let prompt_clone = prompt.clone();
			let timeout_secs = crate::helpers::configuration::Config::catalog_llm_timeout_secs();
			let text_opt = if timeout_secs == 0 {
				match tokio::task::spawn_blocking(move || llm_clone.chat(&[crate::llm::ChatMessage { role: "user".into(), content: prompt_clone }])).await {
					Ok(Ok(t)) => Some(t),
					_ => None,
				}
			} else {
				match tokio::time::timeout(
					std::time::Duration::from_secs(timeout_secs),
					tokio::task::spawn_blocking(move || llm_clone.chat(&[crate::llm::ChatMessage { role: "user".into(), content: prompt }]))
				).await {
					Ok(Ok(Ok(t))) => Some(t),
					_ => None,
				}
			};
			if let Some(text) = text_opt {
				let parsed = extract_json_value(&text).and_then(|v| v.get("synonyms").and_then(|a| a.as_array().cloned()));
				if let Some(arr) = parsed {
					let raw = arr.into_iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect::<Vec<String>>();
					let v = clean_synonyms(raw, &role);
					if !v.is_empty() { fld.synonyms.get_or_insert(v); }
				}
			}
		}

		// LLM enrichment (strict JSON): pii_sensitivity and units_or_format
		if fld.pii_sensitivity.is_none() || fld.units_or_format.is_none() {
			let prompt = format!(
				"Return STRICT JSON only: {{\"pii\": \"none|low|medium|high\", \"units\": \"<units or format>\"}}.\nRules:\n- pii must be one of: none, low, medium, high\n- units: short label like 'Celsius', 'ms', 'ISO8601', or use null if N/A\n- JSON only; no prose.\n\nDataset: {ns}\nField: {f}\nRole: {r}\nStats: {s}\n\nOutput JSON:",
				ns = namespace,
				f = fld.name,
				r = role,
				s = stats_snip
			);
			let llm_clone = llm.clone();
			let prompt_clone = prompt.clone();
			let timeout_secs = crate::helpers::configuration::Config::catalog_llm_timeout_secs();
			let text_opt = if timeout_secs == 0 {
				match tokio::task::spawn_blocking(move || llm_clone.chat(&[crate::llm::ChatMessage { role: "user".into(), content: prompt_clone }])).await {
					Ok(Ok(t)) => Some(t),
					_ => None,
				}
			} else {
				match tokio::time::timeout(
					std::time::Duration::from_secs(timeout_secs),
					tokio::task::spawn_blocking(move || llm_clone.chat(&[crate::llm::ChatMessage { role: "user".into(), content: prompt }]))
				).await {
					Ok(Ok(Ok(t))) => Some(t),
					_ => None,
				}
			};
			if let Some(text) = text_opt {
				if let Some(v) = extract_json_value(&text) {
					if fld.pii_sensitivity.is_none() {
						if let Some(s) = v.get("pii").and_then(|x| x.as_str()).and_then(|s| validate_pii(s)) {
							fld.pii_sensitivity = Some(s);
						}
					}
					if fld.units_or_format.is_none() {
						if let Some(u) = v.get("units").and_then(|x| x.as_str()) {
							let trimmed = u.trim();
							if !is_placeholder_or_garbage(trimmed) && !trimmed.is_empty() { fld.units_or_format = Some(trimmed.to_string()); }
						}
					}
				}
			}
		}
		// Heuristic fallback for synonyms and PII if still missing
		// if fld.synonyms.is_none() { let syns = infer_synonyms(&fld.name, roles.get(&fld.name)); if !syns.is_empty() { fld.synonyms = Some(syns); } }
		// if fld.pii_sensitivity.is_none() { fld.pii_sensitivity = Some(infer_pii(&fld.name, stats_for)); }
		// Heuristic fallback for description if still missing
		// if fld.description.is_none() { let d = generate_field_description(&fld.name, roles.get(&fld.name), stats_for); if !d.is_empty() { fld.description = Some(d); } }
	}

	// Root-level description if missing
	if catalog.description.is_none() {
		let desc = generate_root_description(namespace, catalog);
		if !desc.is_empty() { catalog.description = Some(desc); }
	}
}

fn extract_json_value(text: &str) -> Option<serde_json::Value> {
    // Direct parse
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) { return Some(v); }
    // Try to extract first balanced JSON object
    let bytes = text.as_bytes();
    let mut depth: i32 = 0;
    let mut start: Option<usize> = None;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'{' {
            if depth == 0 { start = Some(i); }
            depth += 1;
        } else if *b == b'}' {
            if depth > 0 { depth -= 1; }
            if depth == 0 {
                if let Some(s) = start {
                    let slice = &text[s..=i];
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) { return Some(v); }
                }
            }
        }
    }
    None
}

fn infer_synonyms(name: &str, role_opt: Option<&SemanticFieldRole>) -> Vec<String> {
	let n = name.to_lowercase();
	let mut out: Vec<String> = Vec::new();
	// Whitelist-only to avoid noisy/low-signal synonyms
	if n.ends_with("_id") || n == "id" { out.extend(["id","identifier","key","uuid"].iter().map(|s| s.to_string())); }
	if n.contains("event_type") || n == "type" || n.contains("message_type") { out.extend(["type","kind","category"].iter().map(|s| s.to_string())); }
	if n.contains("event_date") || n.contains("timestamp") || n.contains("rcvd_time") || n.contains("sent_time") || n.contains("created_at") { out.extend(["timestamp","datetime","occurred_at"].iter().map(|s| s.to_string())); }
	if n.contains("rider_id") { out.extend(["user_id","customer_id"].iter().map(|s| s.to_string())); }
	// Do not add generic synonyms for FreeText or arbitrary categorical fields
	out.sort(); out.dedup(); out
}

fn infer_pii(name: &str, stats_opt: Option<&FieldStats>) -> String {
	let n = name.to_lowercase();
	// Name-based signals
	if n.contains("email") || n.contains("phone") || n.contains("ssn") || n.contains("social_security") { return "high".to_string(); }
	if n.contains("name") || n.contains("address") || n.contains("dob") || n.contains("birth") { return "medium".to_string(); }
	// Stats-based signals
	if let Some(s) = stats_opt {
		if s.approx_distinct.unwrap_or(0) > 100000 && s.max_len.unwrap_or(0) >= 20 { return "medium".to_string(); }
		if s.approx_distinct.unwrap_or(0) > 1000000 { return "medium".to_string(); }
	}
	"none".to_string()
}

fn generate_field_description(name: &str, role_opt: Option<&SemanticFieldRole>, stats_opt: Option<&FieldStats>) -> String {
	let n = name.to_lowercase();
	let base = n.split('.').last().unwrap_or(&n).replace('_', " ");
	match role_opt {
		Some(SemanticFieldRole::Id) => {
			if n.ends_with("_id") && base != "id" { return format!("Unique identifier for {}", base.trim_end_matches(" id")); }
			"Unique identifier.".to_string()
		}
		Some(SemanticFieldRole::Timestamp) => {
			if n.contains("rcvd_time") { return "Timestamp when record was received".to_string(); }
			if n.contains("sent_time") { return "Timestamp when record was sent".to_string(); }
			if n.contains("prcd_micro_time") { return "Processing timestamp for the record".to_string(); }
			if n.contains("event_date") || n.contains("timestamp") { return "Event timestamp".to_string(); }
			"Timestamp value".to_string()
		}
		Some(SemanticFieldRole::Categorical) => {
			if n.contains("event_type") { return "Type of event".to_string(); }
			if n.contains("message_type") { return "Type of message".to_string(); }
			if n.contains("manufacturer") { return "Bike hardware manufacturer".to_string(); }
			if n.contains("model") { return "Bike hardware model".to_string(); }
			format!("Categorical value for {}", base)
		}
		Some(SemanticFieldRole::Metric) => {
			if n.contains("temperature") { return format!("Temperature for {}", base); }
			"Numeric measure".to_string()
		}
		Some(SemanticFieldRole::FreeText) | None => {
			format!("Text for {}", base)
		}
	}
}

fn generate_root_description(namespace: &str, catalog: &DataCatalog) -> String {
	let mut highlights: Vec<&str> = Vec::new();
	let names: Vec<String> = catalog.fields.iter().map(|f| f.name.clone()).collect();
	if names.iter().any(|n| n.contains("event_type")) { highlights.push("event types"); }
	if names.iter().any(|n| n.contains("event_date") || n.contains("timestamp")) { highlights.push("timestamps"); }
	if names.iter().any(|n| n.ends_with("_id") || n.contains("bike_id")) { highlights.push("identifiers"); }
	let summary_bits = if highlights.is_empty() { String::new() } else { format!(" including {}", highlights.join(", ")) };
	format!("Dataset '{}' of events and attributes{}.", namespace, summary_bits)
}


