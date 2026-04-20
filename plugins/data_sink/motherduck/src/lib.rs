pub use skippr_core::RUNNING;
pub use skippr_core::{
    buffer, converters, discover, helpers, ingest, ingest_work, metrics, plugins, serdes,
};

#[path = "../../../shared/parquet_util.rs"]
pub mod parquet_util;

#[path = "../../../shared/cdc_encode.rs"]
pub mod cdc_encode;

#[path = "../../../shared/cdc_apply_core.rs"]
mod cdc_apply_core;

pub mod cdc_apply {
    pub use super::cdc_apply_core::*;
}

pub mod motherduck;

pub use motherduck::*;
