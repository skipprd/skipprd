pub mod append_source_runtime;
pub mod progress;
pub mod protocol;
pub mod runtime_main;
pub mod sdk;
pub mod sink_compat;
pub mod sink_idempotency;
pub mod sink_runtime_entry;
pub mod source_compat;
pub mod source_sync;
pub mod wire;

pub use skippr_core::RUNNING;
pub use skippr_core::{
    buffer, converters, discover, helpers, ingest, lineage, metrics, plugins, serdes, wal_slice_ref,
};
pub use skippr_plugin_macros::SkipprConfig;

#[macro_export]
macro_rules! declare_sink_spec {
    ($spec:ident, $plugin:ty, $capability:path, $support:ty) => {
        $crate::declare_sink_spec!($spec, $capability, $support);

        impl $crate::plugins::HasSinkSpec for $plugin {
            type Spec = $spec;
        }
    };
    // Spec only: for plugins whose writer type already implements `HasSinkSpec<Spec = S>`.
    ($spec:ident, $capability:path, $support:ty) => {
        const _: () = {
            assert!(!$capability.grouping_support.is_none());
            assert!($capability
                .grouping_support
                .equals(<$support as $crate::plugins::SinkWriteSupport>::GROUPING));
            assert!($capability
                .retry_semantics
                .equals(<$support as $crate::plugins::SinkWriteSupport>::RETRY));
            assert!(
                $capability.grouping_support.is_none()
                    || !$capability.retry_semantics.requires_idempotent_replay()
                    || <$support as $crate::plugins::SinkWriteSupport>::CAN_RETURN_ALREADY_APPLIED
            );
            assert!(
                $capability.grouping_support.is_none()
                    || <$support as $crate::plugins::SinkWriteSupport>::BOUNDED_GROUPED_STREAM
            );
            assert!(
                !$capability.retry_semantics.grouped_writes_are_exact_once()
                    || <$support as $crate::plugins::SinkWriteSupport>::EXACT_ONCE_ALLOWED
            );
        };

        pub struct $spec;

        impl $crate::plugins::SinkSpec for $spec {
            const NAME: &'static str = $capability.name;
            const CAPABILITY: $crate::plugins::cdc::SinkCapability = $capability;
            type WriteSupport = $support;
        }
    };
}

#[macro_export]
macro_rules! declare_schema_sink_spec {
    ($spec:ident, $plugin:ty, $name:expr) => {
        $crate::declare_schema_sink_spec!(
            $spec,
            $plugin,
            $name,
            $crate::plugins::AlterSupport::Unsupported
        );
    };
    ($spec:ident, $plugin:ty, $name:expr, $alter:expr) => {
        pub struct $spec;

        impl $crate::plugins::SchemaSinkSpec for $spec {
            const NAME: &'static str = $name;
            const ALTER_SUPPORT: $crate::plugins::AlterSupport = $alter;
        }

        impl $crate::plugins::HasSchemaSinkSpec for $plugin {
            type Spec = $spec;
        }
    };
}
