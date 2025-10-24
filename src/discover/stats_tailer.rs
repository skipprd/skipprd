use crate::discover::stats::NamespaceStats;
use crate::helpers::configuration::Config;
use once_cell::sync::OnceCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct FieldObservation {
    pub namespace: String,
    pub field: String,
    pub value: serde_json::Value,
}

static OBS_QUEUE: OnceCell<crossbeam_channel::Sender<FieldObservation>> = OnceCell::new();

pub fn ensure_stats_worker() {
    if OBS_QUEUE.get().is_some() { return; }
    let (tx, rx) = crossbeam_channel::unbounded::<FieldObservation>();
    let _ = OBS_QUEUE.set(tx);
    std::thread::spawn(move || {
        let mut by_ns: HashMap<String, NamespaceStats> = HashMap::new();
        let mut last_flush = std::time::Instant::now();
        let flush_secs = Config::stats_flush_seconds();
        // Debounce map per namespace for LLM/catalog enrichment
        let mut last_llm: HashMap<String, std::time::Instant> = HashMap::new();
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(obs) => {
                    let entry = by_ns.entry(obs.namespace.clone()).or_insert_with(|| NamespaceStats::new(&obs.namespace));
                    entry.update_field(&obs.field, &obs.value);
                    // If LLM enabled, debounce enrichment per namespace
                    if Config::pipeline_llm_enabled() {
                        let now = std::time::Instant::now();
                        let due = match last_llm.get(&obs.namespace) {
                            Some(t) => now.duration_since(*t).as_millis() as u64 >= Config::pipeline_llm_debounce_ms(),
                            None => true,
                        };
                        if due {
                            enrich_llm(&obs.namespace, entry);
                            last_llm.insert(obs.namespace.clone(), now);
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            if last_flush.elapsed().as_secs() >= flush_secs {
                flush_all(&mut by_ns);
                last_flush = std::time::Instant::now();
            }
        }
        flush_all(&mut by_ns);
    });
}

fn enrich_llm(ns: &str, stats: &NamespaceStats) {
    let debug = crate::helpers::configuration::Config::debug_enabled();
    let t0 = std::time::Instant::now();
    // finalize view for inference without mutating existing stats
    let mut snapshot = stats.clone();
    for (_k, fs) in snapshot.fields.iter_mut() { fs.finalize(); }
    // Derive semantic and catalog, optionally LLM-enrich
    let semantic = crate::semantics::infer::infer_semantic_model(ns);
    let mut catalog = crate::semantics::model::DataCatalog {
        namespace: ns.to_string(),
        description: None,
        fields: semantic.fields.iter().map(|f| crate::semantics::model::CatalogField {
            entity: String::new(),
            name: f.name.clone(),
            description: None,
            synonyms: None,
            pii_sensitivity: None,
            units_or_format: None,
        }).collect(),
    };
    if Config::pipeline_llm_enabled() && Config::catalog_llm_enabled() {
        let llm = crate::llm::create_llm(&crate::llm::config_from_env());
        // Build a single prompt that includes field names, roles, stats and a few sample values
        let mut field_lines: Vec<String> = Vec::new();
        for f in &catalog.fields {
            let role = semantic.fields.iter().find(|sf| sf.name == f.name).map(|sf| format!("{:?}", sf.role)).unwrap_or_else(|| "Unknown".to_string());
            let stats_line = if let Some(fs) = snapshot.fields.get(&f.name) {
                let mut parts: Vec<String> = Vec::new();
                if let Some(mi) = fs.min_numeric { parts.push(format!("min={}", mi)); }
                if let Some(ma) = fs.max_numeric { parts.push(format!("max={}", ma)); }
                if let Some(dl) = fs.min_len { parts.push(format!("min_len={}", dl)); }
                if let Some(xl) = fs.max_len { parts.push(format!("max_len={}", xl)); }
                if let Some(d) = fs.approx_distinct { parts.push(format!("approx_distinct={}", d)); }
                let ex = fs.examples();
                let examples = if !ex.is_empty() { format!(" examples=[{}]", ex.iter().take(5).map(|e| e.replace('\n', " ")).collect::<Vec<_>>().join(", ")) } else { String::new() };
                if parts.is_empty() && examples.is_empty() { String::new() } else { format!(" stats: {}{}", parts.join(", "), examples) }
            } else { String::new() };
            field_lines.push(format!("- {} role:{}{}", f.name, role, stats_line));
        }
        field_lines.sort();
        let list = field_lines.join("\n");
        let prompt = format!(
            "You are creating a data catalog. Using the field name, semantic role, basic stats and a few sample values, write a short, clear description, synonyms, PII sensitivity (none, low, medium, high), and units/format if detectable.\nRespond with one line per field in this exact format:\n<field>: description: <short description> | synonyms: a,b,c | pii: <none|low|medium|high> | units: <units or format>\nFields:\n{}",
            list
        );
        match llm.chat(&[crate::llm::ChatMessage { role: "user".into(), content: prompt }]) {
        Ok(text) => {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() { continue; }
                // Expect: name: description: ... | synonyms: ...
                let (name_part, rest) = match line.split_once(':') { Some(x) => x, None => continue };
                let name = name_part.trim().trim_start_matches('-').trim();
                if let Some(cf) = catalog.fields.iter_mut().find(|f| f.name == name) {
                    let parts: Vec<&str> = rest.split('|').collect();
                    for p in parts {
                        let s = p.trim();
                        if let Some(r) = s.strip_prefix("description:") { cf.description = Some(r.trim().to_string()); }
                        // Replace escape chars "\" in synonyms as llm seems to add them
                        if let Some(r) = s.strip_prefix("synonyms:") { cf.synonyms = Some(r.replace('\\', "").split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()); }
                        if let Some(r) = s.strip_prefix("pii:") { cf.pii_sensitivity = Some(r.trim().to_string()); }
                        if let Some(r) = s.strip_prefix("units:") { let v = r.trim(); if !v.is_empty() { cf.units_or_format = Some(v.to_string()); } }
                    }
                    // Log discovered description for visibility during discovery
                    if let Some(desc) = cf.description.as_ref() {
                        if !desc.is_empty() { println!("catalog: {}.{} description: {}", ns, cf.name, desc); }
                    }
                }
            }
            crate::metrics::counters::add_llm_enrich_success(1);
            crate::metrics::counters::add_llm_enrich_latency_ns(t0.elapsed().as_nanos() as u64);
        }
        Err(_e) => {
            crate::metrics::counters::add_llm_enrich_failure(1);
        }
        }
    }
    Config::write_semantic_and_catalog_sync(ns, &semantic, &catalog);
    crate::metrics::counters::add_semantic_write_success(1);
    if debug { println!("semantic/catalog enriched for '{}' in {}ms (fields={})", ns, t0.elapsed().as_millis(), catalog.fields.len()); }
}

fn flush_all(by_ns: &mut HashMap<String, NamespaceStats>) {
    for (ns, stats) in by_ns.iter_mut() {
        for (_k, fs) in stats.fields.iter_mut() { fs.finalize(); }
        Config::write_namespace_stats_sync(ns, stats);
        // Run enrichment at flush/end to ensure discover mode writes
        enrich_llm(ns, stats);
    }
}

pub fn emit_observation(namespace: &str, field: &str, value: &serde_json::Value) {
    if let Some(tx) = OBS_QUEUE.get() {
        let _ = tx.send(FieldObservation { namespace: namespace.to_string(), field: field.to_string(), value: value.clone() });
    }
}

/// Recursively emit observations for nested structures using dot-notation paths.
/// Scalars and nulls are emitted for stats; arrays are traversed, emitting each element under the same field path.
pub fn emit_observation_deep(namespace: &str, field: &str, value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter() {
                let next = if field.is_empty() { k.clone() } else { format!("{}.{}", field, k) };
                emit_observation_deep(namespace, &next, v);
            }
        }
        serde_json::Value::Array(arr) => {
            // Emit each element under the same field path; if elements are objects, they will expand further
            // Cap traversal to avoid runaway cost
            let mut count = 0usize;
            for el in arr {
                emit_observation_deep(namespace, field, el);
                count = count.saturating_add(1);
                if count >= 64 { break; }
            }
        }
        // Scalars and nulls
        _ => {
            emit_observation(namespace, field, value);
        }
    }
}

/// Public helper to run LLM enrichment using an existing on-disk stats snapshot, if present.
pub fn enrich_llm_from_existing(ns: &str) {
    let path = crate::helpers::configuration::Config::get_stats_local_path(ns);
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Ok(mut stats) = serde_json::from_str::<NamespaceStats>(&s) {
            for (_k, fs) in stats.fields.iter_mut() { fs.finalize(); }
            enrich_llm(ns, &stats);
        }
    }
}


