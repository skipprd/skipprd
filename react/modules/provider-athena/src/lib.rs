mod type_parse;

mod discover {
    pub mod stats {
        pub use react_core::discover::stats::*;
    }
}

mod providers {
    pub mod dataset_catalog_provider {
        pub use react_core::providers::dataset_catalog_provider::*;
    }
    pub mod catalog {
        pub mod types {
            pub use react_core::providers::catalog::types::*;
        }
    }
    pub mod type_parse {
        pub use crate::type_parse::*;
    }
    pub use react_core::providers::{QueryProvider, QueryResult};
}

include!("athena_impl.rs");
