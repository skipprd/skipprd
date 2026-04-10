mod cdc_apply;
pub mod config;
mod postgres;

pub use config::DataSinkPostgresPluginConfig;
pub use postgres::DataSinkPostgresPlugin;
