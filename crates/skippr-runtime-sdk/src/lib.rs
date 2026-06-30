pub mod append_source_runtime;
pub mod content_quality_worker;
pub mod google_serp_worker;
pub mod progress;
pub mod protocol;
pub mod runtime_main;
pub mod runtime_offsets;
pub mod sdk;
pub mod sink_compat;
pub mod sink_idempotency;
pub mod sink_runtime_entry;
pub mod site_quality_worker;
pub mod site_security_worker;
pub mod source_compat;
pub mod source_sync;
pub mod wire;

pub use skippr_core::RUNNING;
pub use skippr_core::{
    buffer, converters, discover, helpers, ingest, lineage, metrics, plugins, serdes,
};

#[macro_export]
macro_rules! declare_sink_spec {
    ($spec:ident, $plugin:ty, $capability:path, $support:ty) => {
        const _: () = {
            assert!(!$capability.grouping_support.is_none());
            assert!(
                $capability
                    .grouping_support
                    .equals(<$support as $crate::plugins::SinkWriteSupport>::GROUPING)
            );
        };

        pub struct $spec;

        impl $crate::plugins::SinkSpec for $spec {
            const NAME: &'static str = $capability.name;
            const CAPABILITY: $crate::plugins::cdc::SinkCapability = $capability;
            type WriteSupport = $support;
        }

        impl $crate::plugins::HasSinkSpec for $plugin {
            type Spec = $spec;
        }
    };
}

#[macro_export]
macro_rules! declare_schema_sink_spec {
    ($spec:ident, $plugin:ty, $name:expr) => {
        pub struct $spec;

        impl $crate::plugins::SchemaSinkSpec for $spec {
            const NAME: &'static str = $name;
        }

        impl $crate::plugins::HasSchemaSinkSpec for $plugin {
            type Spec = $spec;
        }
    };
}
