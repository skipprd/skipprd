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
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(obs) => {
                    let entry = by_ns.entry(obs.namespace.clone()).or_insert_with(|| NamespaceStats::new(&obs.namespace));
                    entry.update_field(&obs.field, &obs.value);
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

fn flush_all(by_ns: &mut HashMap<String, NamespaceStats>) {
    for (ns, stats) in by_ns.iter_mut() {
        // finalize approximate distinct estimates before writing
        for (_k, fs) in stats.fields.iter_mut() { fs.finalize(); }
        // Best-effort synchronous upload to avoid unbounded buffer growth
        Config::write_namespace_stats_sync(ns, stats);
    }
}

pub fn emit_observation(namespace: &str, field: &str, value: &serde_json::Value) {
    if let Some(tx) = OBS_QUEUE.get() {
        let _ = tx.send(FieldObservation { namespace: namespace.to_string(), field: field.to_string(), value: value.clone() });
    }
}


