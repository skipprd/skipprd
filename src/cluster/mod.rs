//! Clustered WAL coordination: identity, membership, replica protocol, and scheduler.
//!
//! Replica/query/catch-up code must use [`skippr_lease::PipelineKey`] and
//! [`skippr_lease::PipelinePaths`]. It must not call `Config::get_pipeline_name()`
//! or `Config::get_data_dir()`.

pub mod backend;
pub mod baseline;
pub mod catchup;
pub mod disk;
pub mod failpoint;
pub mod gossip;
pub mod identity;
pub mod lifecycle;
pub mod membership;
pub mod peer;
pub mod pipeline_registry;
pub mod pipeline_view;
pub mod placement;
pub mod promote;
pub mod scheduler;
pub mod schema;
pub mod tls;
pub mod validation;
pub mod wal_head;

pub use identity::{
    derive_advertised_ip, derive_host_id, ClusterConfig, ClusterIdentity, ProcessQueryBind,
    TenantScope,
};
pub use pipeline_view::PipelineConfigView;
pub use validation::{validate_clustered_mode, CliModeKind};

#[cfg(test)]
mod tests {
    use crate::helpers::wal_storage::WalStorage;
    use std::path::PathBuf;

    #[test]
    fn replica_modules_do_not_call_process_global_pipeline_config() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let forbidden = ["Config::get_pipeline_name()", "Config::get_data_dir()"];
        for rel in ["cluster", "buffer/durable", "query_flight"] {
            let dir = root.join(rel);
            if !dir.exists() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&dir) {
                let entry = entry.unwrap();
                if entry.path().extension().and_then(|s| s.to_str()) != Some("rs") {
                    continue;
                }
                if entry.path().file_name().and_then(|s| s.to_str()) == Some("mod.rs")
                    && rel == "cluster"
                {
                    // This file contains the guard test source itself.
                    continue;
                }
                let text = std::fs::read_to_string(entry.path()).unwrap();
                for needle in forbidden {
                    assert!(
                        !text.contains(needle),
                        "{} must not call {needle}",
                        entry.path().display()
                    );
                }
            }
        }
    }

    #[test]
    fn clustered_requires_explicit_match() {
        fn classify(storage: WalStorage) -> &'static str {
            match storage {
                WalStorage::Disk => "disk",
                WalStorage::S3 => "s3",
                WalStorage::Clustered => "clustered",
            }
        }
        assert_eq!(classify(WalStorage::Clustered), "clustered");
    }
}
