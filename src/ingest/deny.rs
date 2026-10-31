use once_cell::sync::Lazy;
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::sync::Mutex;
use tracing::warn;

/// Deadletter `failure_code` for mixed scalar types in a primitive array, or
/// mixed leaf types across array-of-record elements. Lookup: schema docs.
pub const ARRAY_MIXED_TYPES: &str = "ARRAY_MIXED_TYPES";

static DENIED_ARRAY_FIELDS_BY_NAMESPACE: Lazy<Mutex<HashSet<String>>> =
    Lazy::new(|| Mutex::new(HashSet::new()));

#[derive(Debug, Clone)]
pub struct IngestDenyError {
    pub code: &'static str,
    pub message: String,
}

impl fmt::Display for IngestDenyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for IngestDenyError {}

fn log_deny_once(namespace: &str, field: &str) {
    let key = format!("{namespace}\0{field}");
    let first = match DENIED_ARRAY_FIELDS_BY_NAMESPACE.lock() {
        Ok(mut seen) => seen.insert(key),
        Err(_) => false,
    };
    if first {
        warn!(
            namespace = %namespace,
            field = %field,
            failure_code = ARRAY_MIXED_TYPES,
            "Denied mixed array types; further rows deadletter as ARRAY_MIXED_TYPES"
        );
    }
}

/// Wrap a coerce/evolution error as `ARRAY_MIXED_TYPES`. Idempotent.
pub fn wrap_array_mixed(err: Box<dyn Error>, field: &str) -> Box<dyn Error> {
    if err.downcast_ref::<IngestDenyError>().is_some() {
        return err;
    }
    let namespace = crate::ingest::ingest::current_namespace();
    log_deny_once(&namespace, field);
    Box::new(IngestDenyError {
        code: ARRAY_MIXED_TYPES,
        message: format!(
            "{err}. Mixed types in arrays are not type-split; flatten the array or send a single type. See schema docs."
        ),
    })
}

pub fn failure_code_from_error(err: &(dyn Error + 'static)) -> (&'static str, String) {
    if let Some(deny) = err.downcast_ref::<IngestDenyError>() {
        (deny.code, deny.message.clone())
    } else {
        ("EVOLUTION_SLOW_PATH", err.to_string())
    }
}
