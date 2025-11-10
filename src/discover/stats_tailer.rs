use crate::discover::stats::NamespaceStats;
use crate::helpers::configuration::Config;
use once_cell::sync::{OnceCell, Lazy};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::debug;

#[derive(Clone, Debug)]
pub struct FieldObservation {
    pub namespace: String,
    pub field: String,
    pub value: serde_json::Value,
}

enum ObservationMsg { Data(FieldObservation), Shutdown }

static OBS_QUEUE: OnceCell<crossbeam_channel::Sender<ObservationMsg>> = OnceCell::new();
static OBS_MAP: OnceCell<Arc<Mutex<HashMap<String, NamespaceStats>>>> = OnceCell::new();
static WORKER_HANDLE: Lazy<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>> = Lazy::new(|| std::sync::Mutex::new(None));

pub fn ensure_stats_worker() {
    if OBS_QUEUE.get().is_some() { return; }
    let (tx, rx) = crossbeam_channel::unbounded::<ObservationMsg>();
    let _ = OBS_QUEUE.set(tx);
    let map_arc = Arc::new(Mutex::new(HashMap::<String, NamespaceStats>::new()));
    let _ = OBS_MAP.set(map_arc.clone());
    let handle = std::thread::spawn(move || {
        let mut last_flush = std::time::Instant::now();
        let flush_secs = Config::stats_flush_seconds();
        // Debounce map per namespace for LLM/catalog enrichment
        let mut last_llm: HashMap<String, std::time::Instant> = HashMap::new();
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(ObservationMsg::Data(obs)) => {
                    let mut by_ns = match map_arc.lock() { Ok(g) => g, Err(poison) => poison.into_inner() };
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
                Ok(ObservationMsg::Shutdown) => {
                    let mut by_ns = match map_arc.lock() { Ok(g) => g, Err(poison) => poison.into_inner() };
                    flush_all(&mut by_ns);
                    break;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    let mut by_ns = match map_arc.lock() { Ok(g) => g, Err(poison) => poison.into_inner() };
                    flush_all(&mut by_ns);
                    break;
                }
            }
            if last_flush.elapsed().as_secs() >= flush_secs {
                let mut by_ns = match map_arc.lock() { Ok(g) => g, Err(poison) => poison.into_inner() };
                flush_all(&mut by_ns);
                last_flush = std::time::Instant::now();
            }
        }
    });
    if let Ok(mut g) = WORKER_HANDLE.lock() { *g = Some(handle); }
}

fn enrich_llm(ns: &str, stats: &NamespaceStats) {
    let debug = crate::helpers::configuration::Config::debug_enabled();
    let t0 = std::time::Instant::now();
    // finalize view for inference without mutating existing stats
    let mut snapshot = stats.clone();
    for (_k, fs) in snapshot.fields.iter_mut() { fs.finalize(); }
    // Derive semantic and catalog (S3-based stats), optionally LLM-enrich
    let semantic = {
        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                let ns_owned = ns.to_string();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
                    rt.block_on(crate::catalog::infer::infer_semantic_model_async(&ns_owned))
                }).join().unwrap()
            }
            Err(_) => {
                let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
                rt.block_on(crate::catalog::infer::infer_semantic_model_async(ns))
            }
        }
    };
    let mut catalog = crate::catalog::model::DataCatalog {
        namespace: ns.to_string(),
        description: None,
        dimensions: semantic.dimensions.clone(),
        metrics: semantic.metrics.clone(),
        fields: semantic.fields.iter().map(|f| crate::catalog::model::CatalogField {
            entity: String::new(),
            name: f.name.clone(),
            description: None,
            synonyms: None,
            pii_sensitivity: None,
            units_or_format: None,
            role: Some(format!("{:?}", f.role)),
        }).collect(),
    };
    // Defer field-level LLM enrichment to end-of-discover pass
    // Write catalog (unified) to S3; warn if empty
    if catalog.fields.is_empty() { debug!("{} DISCOVER: catalog fields empty for '{}'", chrono::Utc::now().to_rfc3339(), ns); }
    // Write S3 keys and field count for debugging
    let tenant = Config::get_tenant();
    let workspace = Config::get_workspace_name();
    let pipeline = Config::get_pipeline_name();
    let stats_key = format!("{}/{}/{}/stats/{}.json", tenant, workspace, pipeline, ns);
    let cat_key = format!("{}/{}/{}/catalog/{}.yaml", tenant, workspace, pipeline, ns);
    debug!("{} DISCOVER: flush ns='{}' stats='{}' catalog='{}' fields={}", chrono::Utc::now().to_rfc3339(), ns, stats_key, cat_key, catalog.fields.len());
    // Write catalog asynchronously to S3
    match tokio::runtime::Handle::try_current() {
        Ok(_) => {
            let ns_owned = ns.to_string();
            let cat_owned = catalog.clone();
            let _ = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
                rt.block_on(async { Config::write_catalog_async(&ns_owned, &cat_owned).await; });
            }).join();
        }
        Err(_) => {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
            rt.block_on(async { Config::write_catalog_async(ns, &catalog).await; });
        }
    }
    debug!("semantic/catalog enriched for '{}' in {}ms (fields={})", ns, t0.elapsed().as_millis(), catalog.fields.len());
}

fn flush_all(by_ns: &mut HashMap<String, NamespaceStats>) {
    for (ns, stats) in by_ns.iter_mut() {
        for (_k, fs) in stats.fields.iter_mut() { fs.finalize(); }
        // Flush stats to S3 only
        Config::write_namespace_stats_sync(ns, stats);
        // Run enrichment at flush/end to ensure discover mode writes
        enrich_llm(ns, stats);
    }
}

pub fn emit_observation(namespace: &str, field: &str, value: &serde_json::Value) {
    if let Some(tx) = OBS_QUEUE.get() {
        let _ = tx.send(ObservationMsg::Data(FieldObservation { namespace: namespace.to_string(), field: field.to_string(), value: value.clone() }));
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
pub fn enrich_llm_from_existing(_ns: &str) { /* removed local fallback */ }

/// Force a final flush of stats → semantic → catalog for all namespaces.
pub fn force_flush() {
    if let Some(map_arc) = OBS_MAP.get() {
        let mut by_ns = match map_arc.lock() { Ok(g) => g, Err(poison) => poison.into_inner() };
        flush_all(&mut by_ns);
    }
}

pub fn shutdown_and_join(timeout_secs: u64) {
    if let Some(tx) = OBS_QUEUE.get() { let _ = tx.send(ObservationMsg::Shutdown); }
    let (jtx, jrx) = std::sync::mpsc::channel::<()>();
    let handle_opt = { WORKER_HANDLE.lock().ok().and_then(|mut g| g.take()) };
    if let Some(h) = handle_opt {
        std::thread::spawn(move || { let _ = h.join(); let _ = jtx.send(()); });
        let _ = jrx.recv_timeout(std::time::Duration::from_secs(timeout_secs));
    }
}


