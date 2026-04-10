//! Sink connector implementations for external `plugins/data_sink/*` binaries.
//!
//! These modules are `#[path]`-included here (not via `plugins::data_sink`) so connector code
//! compiles exactly once.

#[path = "../../shared/parquet_util.rs"]
pub mod parquet_util;

#[path = "plugins/data_sink/cdc_encode.rs"]
pub mod cdc_encode;

#[path = "plugins/data_sink/cdc_apply.rs"]
pub mod cdc_apply;

#[path = "plugins/data_sink/amqp.rs"]
pub mod amqp;

#[path = "plugins/data_sink/athena.rs"]
pub mod athena;

#[path = "plugins/data_sink/azure_blob.rs"]
pub mod azure_blob;

#[path = "plugins/data_sink/bigquery.rs"]
pub mod bigquery;

#[path = "plugins/data_sink/clickhouse.rs"]
pub mod clickhouse;

#[path = "plugins/data_sink/databricks.rs"]
pub mod databricks;

#[path = "plugins/data_sink/gcs.rs"]
pub mod gcs;

#[path = "plugins/data_sink/motherduck.rs"]
pub mod motherduck;

#[path = "plugins/data_sink/redshift.rs"]
pub mod redshift;

#[path = "plugins/data_sink/s3.rs"]
pub mod s3;

#[path = "plugins/data_sink/sftp.rs"]
pub mod sftp;

#[path = "plugins/data_sink/snowflake.rs"]
pub mod snowflake;

#[path = "plugins/data_sink/stdout.rs"]
pub mod stdout;

#[path = "plugins/data_sink/synapse.rs"]
pub mod synapse;

#[path = "plugins/schema_sink/glue.rs"]
pub mod glue;
