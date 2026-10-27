pub mod alter_ddl;
pub mod cdc;
pub mod source_contract;
pub mod source_sync;
pub mod traits;

pub use skippr_runtime_sdk::plugins::*;
pub use source_contract::{
    apply_runtime_source_namespace_contracts, ensure_source_contract_for_policy,
    merge_source_contracts_into_pipeline, namespace_source_contract,
    replace_source_contracts_authoritative, resolved_write_policy_for_namespace,
    validate_active_sink_supports_contracts,
};
