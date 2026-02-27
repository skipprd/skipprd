mod adapters {
    pub mod storage {
        pub use react_core::storage::StorageAdapter;
    }
}

mod providers {
    pub use react_core::keyspace::Keyspace;
    pub use react_core::scope::RequestScope;
}

mod ws {
    pub mod terminal {
        #[derive(Clone, Debug)]
        pub enum TerminalEvent {
            DbtProgress { phase: String, detail: String },
        }

        #[derive(Clone)]
        pub struct TerminalSink;

        pub fn sink() -> Option<&'static TerminalSink> {
            None
        }

        impl TerminalSink {
            pub fn emit(&self, _ev: TerminalEvent) {}
        }
    }
}

include!("dbt_impl.rs");
