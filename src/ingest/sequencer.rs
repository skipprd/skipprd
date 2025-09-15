use std::sync::Arc;
use dashmap::DashMap;
use tokio::sync::{mpsc, watch};
use once_cell::sync::Lazy;

use crate::{METADATA, ARROW_SCHEMA_VERSION};
use crate::ingest::fast_ingest::create_default_nested_message;
use crate::ingest_work::Ingest;
use crate::helpers::configuration::Config;
use crate::discover::evolution::{EvolutionSpec, EvolutionProposal, apply_specs_to_namespace};

// Types moved to discover::evolution

#[derive(Clone)]
pub struct SequencerHandle {
    pub tx: mpsc::Sender<EvolutionProposal>,
    pub version_rx: watch::Receiver<u64>,
}

static MANAGER: Lazy<DashMap<String, SequencerHandle>> = Lazy::new(|| DashMap::new());

pub fn ensure_sequencer(namespace: &str) -> SequencerHandle {
    if let Some(h) = MANAGER.get(namespace) { return h.value().clone(); }
    let (tx, mut rx) = mpsc::channel::<EvolutionProposal>(1024);
    let (version_tx, version_rx) = watch::channel::<u64>(ARROW_SCHEMA_VERSION.get(namespace).map(|v| v.value().load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0));
    let ns = namespace.to_string();

    tokio::spawn(async move {
        let mut pending: Vec<EvolutionProposal> = Vec::new();
        loop {
            if pending.is_empty() {
                match rx.recv().await { Some(p) => pending.push(p), None => break }
            }
            // debounce/coalesce
            let mut more = true;
            let start = std::time::Instant::now();
            while more && start.elapsed() < std::time::Duration::from_millis(400) {
                match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
                    Ok(Some(p)) => pending.push(p),
                    _ => { more = false; }
                }
            }
            // apply coalesced proposals
            if !pending.is_empty() {
                apply_evolutions(&ns, &pending);
                pending.clear();
                let v = ARROW_SCHEMA_VERSION.get(&ns).map(|v| v.value().load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0);
                let _ = version_tx.send(v);
            }
        }
    });

    let handle = SequencerHandle { tx, version_rx };
    MANAGER.insert(namespace.to_string(), handle.clone());
    handle
}

fn apply_evolutions(namespace: &str, proposals: &Vec<EvolutionProposal>) {
    // Build a temp metadata copy for this namespace
    let current = METADATA.load().as_ref().clone();
    let mut pm = current.clone();
    // Apply via shared helper
    let specs: Vec<EvolutionSpec> = proposals.iter().flat_map(|p| p.fields.clone()).collect();
    apply_specs_to_namespace(namespace, &specs, &mut pm);

    // Publish new METADATA snapshot atomically
    METADATA.store(Arc::new(pm.clone()));

    // Rebuild default template and arrow schema; bump version only if md5 changed
    if let Some(ns_pub) = pm.metadata.get(namespace) {
        let template = create_default_nested_message(&ns_pub.fields);
        crate::ingest::fast_ingest::DEFAULT_NESTED_MESSAGE.write().insert(namespace.to_string(), template);
    }

    let flatten = Config::get_transform_flatten_events();
    if let Ok(_) = Ingest::prepare_arrow_schema_with_metadata(namespace, &pm.metadata, flatten) {
        // version bumped in prepare if schema changed
    }
}

pub async fn propose_and_wait(namespace: &str, proposal: EvolutionProposal, timeout_ms: u64) -> Option<u64> {
    let handle = ensure_sequencer(namespace);
    let cur = *handle.version_rx.borrow();
    if handle.tx.send(proposal).await.is_err() { return None; }
    let mut rx = handle.version_rx.clone();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if *rx.borrow() > cur { return Some(*rx.borrow()); }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() { return None; }
        if rx.changed().await.is_err() { return None; }
    }
}


