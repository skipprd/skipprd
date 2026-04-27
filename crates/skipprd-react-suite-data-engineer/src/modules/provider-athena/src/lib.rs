mod type_parse;

mod discover {
    pub mod stats {
        pub use react_core::discover::stats::FieldStats;
        pub use react_suite_data_engineer::providers::DatasetFieldStats;
    }
}

mod providers {
    pub mod dataset_catalog_provider {
        pub use react_suite_data_engineer::providers::{DatasetCatalogProvider, DatasetId};
    }
    pub mod catalog {
        pub mod types {
            pub use react_suite_data_engineer::providers::DatasetStats;
        }
    }
    pub mod warehouse {
        pub use react_suite_data_engineer::providers::WarehouseNaming;
    }
    pub mod type_parse {
        pub use crate::type_parse::*;
    }
    pub use react_suite_data_engineer::providers::{QueryProvider, QueryResult};
}

mod athena_impl;
pub use athena_impl::*;
