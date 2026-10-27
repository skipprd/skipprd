pub use skippr_runtime_sdk::plugins::cdc::*;

use skippr_lease::PipelineKey;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, RwLock};

#[derive(Clone, Debug, Default)]
pub struct CdcContracts {
    pub default: Option<NamespaceContract>,
    pub namespaces: BTreeMap<String, NamespaceContract>,
}

impl CdcContracts {
    pub fn for_namespace(&self, ns: &str) -> Option<NamespaceContract> {
        self.namespaces.get(ns).cloned().or_else(|| {
            self.default.clone().map(|mut contract| {
                contract.namespace = ns.to_string();
                contract
            })
        })
    }

    pub fn is_empty(&self) -> bool {
        self.default.is_none() && self.namespaces.is_empty()
    }
}

fn keys_contract(namespace: String, business_key_columns: Vec<String>) -> NamespaceContract {
    NamespaceContract {
        namespace,
        business_key_columns,
        effective_guarantee: EffectiveGuarantee::CdcEncoded,
        order_token_semantics: OrderTokenSemantics::SourceDefined,
        null_key_policy: NullKeyPolicy::Reject,
        requires_skippr_system_columns: true,
    }
}

/// The only business-key resolver. Named empty keys use source columns, else default.
pub fn resolve_business_keys(
    default_keys: &[String],
    named: &BTreeMap<String, Vec<String>>,
    source_key_columns: &HashMap<String, Vec<String>>,
) -> CdcContracts {
    let default = if default_keys.is_empty() {
        None
    } else {
        Some(keys_contract(String::new(), default_keys.to_vec()))
    };
    let mut namespaces = BTreeMap::new();
    for (namespace, yaml_keys) in named {
        let keys = if yaml_keys.is_empty() {
            source_key_columns
                .get(namespace)
                .cloned()
                .unwrap_or_else(|| default_keys.to_vec())
        } else {
            yaml_keys.clone()
        };
        if keys.is_empty() {
            continue;
        }
        namespaces.insert(namespace.clone(), keys_contract(namespace.clone(), keys));
    }
    CdcContracts {
        default,
        namespaces,
    }
}

#[derive(Debug)]
pub enum CdcMapError {
    Poisoned,
    EmptyContracts,
}

impl fmt::Display for CdcMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poisoned => write!(f, "CDC contract map lock poisoned"),
            Self::EmptyContracts => write!(
                f,
                "CDC pipeline has no business keys after resolve_contracts"
            ),
        }
    }
}

impl std::error::Error for CdcMapError {}

fn map_lock_result<T, G>(result: Result<G, std::sync::PoisonError<T>>) -> Result<G, CdcMapError> {
    result.map_err(|_| CdcMapError::Poisoned)
}

static PIPELINE_CDC_CONTRACTS: std::sync::LazyLock<
    RwLock<HashMap<PipelineKey, Arc<CdcContracts>>>,
> = std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn install_pipeline_cdc_contracts(
    key: PipelineKey,
    contracts: CdcContracts,
) -> Result<(), CdcMapError> {
    let mut guard = map_lock_result(PIPELINE_CDC_CONTRACTS.write())?;
    guard.insert(key, Arc::new(contracts));
    Ok(())
}

pub fn remove_pipeline_cdc_contracts(key: &PipelineKey) -> Result<(), CdcMapError> {
    let mut guard = map_lock_result(PIPELINE_CDC_CONTRACTS.write())?;
    guard.remove(key);
    Ok(())
}

pub fn pipeline_cdc_contracts(key: &PipelineKey) -> Result<Option<Arc<CdcContracts>>, CdcMapError> {
    let guard = map_lock_result(PIPELINE_CDC_CONTRACTS.read())?;
    Ok(guard.get(key).cloned())
}

fn stamp_guarantees(existing: &CdcContracts, mut next: CdcContracts) -> CdcContracts {
    if let (Some(prev), Some(cur)) = (existing.default.as_ref(), next.default.as_mut()) {
        cur.effective_guarantee = prev.effective_guarantee;
    }
    for (namespace, contract) in next.namespaces.iter_mut() {
        if let Some(prev) = existing.namespaces.get(namespace) {
            contract.effective_guarantee = prev.effective_guarantee;
        }
    }
    next
}

/// Re-resolve keys after runtime `source_contracts` publish; keep prior guarantees.
pub fn reinstall_resolved_contracts(
    key: PipelineKey,
    resolved: CdcContracts,
) -> Result<(), CdcMapError> {
    if resolved.is_empty() {
        return Err(CdcMapError::EmptyContracts);
    }
    let stamped = match pipeline_cdc_contracts(&key)? {
        Some(existing) => stamp_guarantees(&existing, resolved),
        None => resolved,
    };
    install_pipeline_cdc_contracts(key, stamped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_business_keys_two_pipelines_same_namespace_are_isolated() {
        let named_a = BTreeMap::from([("orders".into(), vec!["a_id".into()])]);
        let named_b = BTreeMap::from([("orders".into(), vec!["b_id".into()])]);
        let a = resolve_business_keys(&[], &named_a, &HashMap::new());
        let b = resolve_business_keys(&[], &named_b, &HashMap::new());
        assert_eq!(
            a.for_namespace("orders").unwrap().business_key_columns,
            vec!["a_id".to_string()]
        );
        assert_eq!(
            b.for_namespace("orders").unwrap().business_key_columns,
            vec!["b_id".to_string()]
        );
    }

    #[test]
    fn resolve_business_keys_empty_named_uses_source_then_default() {
        let named = BTreeMap::from([("orders".into(), Vec::new())]);
        let source = HashMap::from([("orders".into(), vec!["src_id".into()])]);
        let from_source = resolve_business_keys(&["def_id".into()], &named, &source);
        assert_eq!(
            from_source
                .for_namespace("orders")
                .unwrap()
                .business_key_columns,
            vec!["src_id".to_string()]
        );
        let from_default = resolve_business_keys(&["def_id".into()], &named, &HashMap::new());
        assert_eq!(
            from_default
                .for_namespace("orders")
                .unwrap()
                .business_key_columns,
            vec!["def_id".to_string()]
        );
        assert_eq!(
            from_default
                .for_namespace("discovered")
                .unwrap()
                .business_key_columns,
            vec!["def_id".to_string()]
        );
        assert!(!from_default.namespaces.contains_key("*"));
    }

    #[test]
    fn poisoned_cdc_map_lock_is_error() {
        let lock = RwLock::new(());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock.write().unwrap();
            panic!("poison");
        }));
        assert!(matches!(
            map_lock_result(lock.read()),
            Err(CdcMapError::Poisoned)
        ));
    }

    #[test]
    fn cdc_contracts_sot_deletes_global_namespace_map() {
        let src = include_str!("cdc.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(!prod.contains("CDC_NAMESPACE_CONTRACTS"));
        assert!(!prod.contains("fn install_query_cdc_contracts"));
        assert!(!prod.contains("fn get_namespace_cdc_contract"));
        assert!(!prod.contains("fn set_global_cdc_contract"));
        assert!(!prod.contains("fn query_cdc_business_keys"));
        assert!(prod.contains("struct CdcContracts"));
        assert!(prod.contains("fn resolve_business_keys"));
        assert!(prod.contains("PIPELINE_CDC_CONTRACTS"));
        assert!(prod.contains("fn install_pipeline_cdc_contracts"));
        assert!(prod.contains("fn reinstall_resolved_contracts"));

        let engine = include_str!("../engine.rs");
        let engine_prod = engine.split("#[cfg(test)]").next().unwrap();
        assert!(engine_prod.contains("resolve_contracts"));
        assert!(engine_prod.contains("cdc_source_key_columns"));
        assert!(engine_prod.contains("install_pipeline_cdc_contracts"));
        assert!(engine_prod.contains("has not declared capabilities"));
        assert!(engine_prod.contains("pipeline_metadata.source_contracts"));
        assert!(engine_prod.contains("has no business keys after resolve_contracts"));
        assert!(!engine_prod.contains("will run in append mode"));
        assert!(!engine_prod.contains("CDC_NAMESPACE_CONTRACTS"));
        assert!(!engine_prod.contains("install_query_cdc_contracts"));

        let tables = include_str!("../sqlrt/tables.rs");
        let tables_prod = tables.split("#[cfg(test)]").next().unwrap();
        assert!(tables_prod.contains("query_namespace_business_keys"));
        assert!(tables_prod.contains("cdc_source_key_columns"));
        assert!(!tables_prod.contains("pipeline_cdc_contracts"));
        assert!(!tables_prod.contains("install_query_cdc_contracts"));

        let ingest = include_str!("../buffer/ingest_buffer.rs");
        assert!(ingest.contains("pipeline_cdc_contracts(&work.pipeline)"));
        assert!(ingest.contains("crate::pipeline_backend::pipeline_key(config)"));
        assert!(!ingest.contains(concat!("ingest_wal_handle()?", ".key()")));

        let source_contract = include_str!("source_contract.rs");
        let apply = source_contract
            .split("pub async fn apply_runtime_source_namespace_contracts")
            .nth(1)
            .expect("apply_runtime_source_namespace_contracts");
        let apply = apply
            .split("pub fn resolved_write_policy_for_namespace")
            .next()
            .unwrap();
        let reinstall = apply
            .find("reinstall_resolved_contracts")
            .expect("reinstall before persist");
        let persist = apply
            .find("persist_pipeline_metadata")
            .expect("persist after reinstall");
        assert!(
            reinstall < persist,
            "CDC map reinstall must commit before persisting source_contracts"
        );
        assert!(source_contract.contains("cdc_source_key_columns"));
    }

    #[test]
    fn reinstall_resolved_contracts_keeps_prior_guarantee() {
        let key = PipelineKey::new("t", "w", "reinstall-keys").unwrap();
        let _ = remove_pipeline_cdc_contracts(&key);
        let mut installed =
            resolve_business_keys(&["id".into()], &BTreeMap::new(), &HashMap::new());
        installed.default.as_mut().unwrap().effective_guarantee =
            EffectiveGuarantee::ExactOnceFinalState;
        install_pipeline_cdc_contracts(key.clone(), installed).unwrap();
        let refreshed = resolve_business_keys(
            &["id".into(), "tenant".into()],
            &BTreeMap::new(),
            &HashMap::new(),
        );
        reinstall_resolved_contracts(key.clone(), refreshed).unwrap();
        let got = pipeline_cdc_contracts(&key).unwrap().unwrap();
        assert_eq!(
            got.default.as_ref().unwrap().business_key_columns,
            vec!["id".to_string(), "tenant".to_string()]
        );
        assert_eq!(
            got.default.as_ref().unwrap().effective_guarantee,
            EffectiveGuarantee::ExactOnceFinalState
        );
        let _ = remove_pipeline_cdc_contracts(&key);
    }

    #[test]
    fn reinstall_does_not_stamp_default_guarantee_onto_new_namespace() {
        let key = PipelineKey::new("t", "w", "reinstall-new-ns").unwrap();
        let _ = remove_pipeline_cdc_contracts(&key);
        let mut installed =
            resolve_business_keys(&["id".into()], &BTreeMap::new(), &HashMap::new());
        installed.default.as_mut().unwrap().effective_guarantee =
            EffectiveGuarantee::ExactOnceFinalState;
        install_pipeline_cdc_contracts(key.clone(), installed).unwrap();
        let named = BTreeMap::from([("orders".into(), vec!["src_id".into()])]);
        reinstall_resolved_contracts(
            key.clone(),
            resolve_business_keys(&["id".into()], &named, &HashMap::new()),
        )
        .unwrap();
        let got = pipeline_cdc_contracts(&key).unwrap().unwrap();
        assert_eq!(
            got.default.as_ref().unwrap().effective_guarantee,
            EffectiveGuarantee::ExactOnceFinalState
        );
        assert_eq!(
            got.for_namespace("orders").unwrap().effective_guarantee,
            EffectiveGuarantee::CdcEncoded
        );
        let _ = remove_pipeline_cdc_contracts(&key);
    }

    #[test]
    fn reinstall_resolved_contracts_rejects_empty() {
        let key = PipelineKey::new("t", "w", "reinstall-empty").unwrap();
        let _ = remove_pipeline_cdc_contracts(&key);
        let installed = resolve_business_keys(&["id".into()], &BTreeMap::new(), &HashMap::new());
        install_pipeline_cdc_contracts(key.clone(), installed).unwrap();
        assert!(matches!(
            reinstall_resolved_contracts(key.clone(), CdcContracts::default()),
            Err(CdcMapError::EmptyContracts)
        ));
        assert!(pipeline_cdc_contracts(&key).unwrap().is_some());
        let _ = remove_pipeline_cdc_contracts(&key);
    }
}
