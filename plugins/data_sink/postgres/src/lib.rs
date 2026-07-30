mod cdc_apply;
pub mod config;
mod postgres;

pub use cdc_apply::{
    apply_postgres_cdc_batch, ddl_add_order_token_column, ddl_create_tombstone_table,
    delete_if_newer_sql, postgres_bulk_cdc_sql, tombstone_table_name, upsert_if_newer_sql,
    CdcApplyBatch, CdcApplyBatchError, CdcApplyColumn, CdcApplyMutation, CdcApplyRow,
    CdcApplyRowMetadata, CdcApplyValue, PostgresBulkCdcSql,
};
pub use config::DataSinkPostgresPluginConfig;
pub use postgres::{DataSinkPostgresPlugin, PostgresCdcBackend};
