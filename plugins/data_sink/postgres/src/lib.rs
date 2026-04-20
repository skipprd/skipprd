mod cdc_apply;
pub mod config;
mod postgres;

pub use cdc_apply::{
    ddl_add_order_token_column, ddl_create_tombstone_table, delete_if_newer_sql,
    tombstone_table_name, upsert_if_newer_sql,
};
pub use config::DataSinkPostgresPluginConfig;
pub use postgres::{DataSinkPostgresPlugin, PostgresCdcBackend};
