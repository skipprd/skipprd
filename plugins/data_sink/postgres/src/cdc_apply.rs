#[path = "../../../shared/cdc_apply_core.rs"]
mod cdc_apply_core;

pub use cdc_apply_core::*;

use bytes::Bytes;
use futures::SinkExt;
use tokio_postgres::{Client, Transaction};

const STAGE_TABLE_NAME: &str = "_skippr_cdc_stage";
const WINNERS_TABLE_NAME: &str = "_skippr_cdc_winners";
const STAGE_ORDINAL_COLUMN: &str = "_skippr_cdc_ordinal";
const STAGE_MUTATION_COLUMN: &str = "_skippr_cdc_mutation";
const STAGE_ORDER_TOKEN_COLUMN: &str = "_skippr_cdc_order_token";
const TARGET_ORDER_TOKEN_COLUMN: &str = "_skippr_order_token";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostgresBulkCdcSql {
    pub create_stage_table: String,
    pub copy_into_stage: String,
    pub materialize_winners: String,
    pub apply_winners: String,
}

/// Generate the Postgres reference-adapter SQL for one bounded CDC batch.
///
/// The staging tables are connection-local and `ON COMMIT DROP`. The caller
/// runs these statements and the native COPY load inside one transaction.
pub fn postgres_bulk_cdc_sql(
    fq_table: &str,
    fq_tombstone_table: &str,
    batch: &CdcApplyBatch,
) -> Result<PostgresBulkCdcSql, CdcApplyBatchError> {
    batch.validate()?;

    let target_columns: Vec<String> = batch
        .columns
        .iter()
        .map(|column| quote_identifier(&column.name))
        .collect();
    let business_keys: Vec<String> = batch
        .business_key_columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect();

    let mut stage_columns = vec![
        format!("{} BIGINT NOT NULL", quote_identifier(STAGE_ORDINAL_COLUMN)),
        format!(
            "{} SMALLINT NOT NULL CHECK ({} IN (0, 1))",
            quote_identifier(STAGE_MUTATION_COLUMN),
            quote_identifier(STAGE_MUTATION_COLUMN)
        ),
        format!(
            "{} BYTEA NOT NULL",
            quote_identifier(STAGE_ORDER_TOKEN_COLUMN)
        ),
    ];
    stage_columns.extend(
        batch
            .columns
            .iter()
            .map(|column| format!("{} {}", quote_identifier(&column.name), column.target_type)),
    );

    let create_stage_table = format!(
        "CREATE TEMP TABLE {} ({}) ON COMMIT DROP;",
        quote_identifier(STAGE_TABLE_NAME),
        stage_columns.join(", ")
    );

    let copy_columns = [
        quote_identifier(STAGE_ORDINAL_COLUMN),
        quote_identifier(STAGE_MUTATION_COLUMN),
        quote_identifier(STAGE_ORDER_TOKEN_COLUMN),
    ]
    .into_iter()
    .chain(target_columns.iter().cloned())
    .collect::<Vec<_>>()
    .join(", ");
    let copy_into_stage = format!(
        "COPY pg_temp.{} ({copy_columns}) FROM STDIN WITH (FORMAT text)",
        quote_identifier(STAGE_TABLE_NAME)
    );

    let winner_projection = target_columns
        .iter()
        .map(|column| format!("ranked.{column}"))
        .chain([
            format!("ranked.{}", quote_identifier(STAGE_ORDINAL_COLUMN)),
            format!("ranked.{}", quote_identifier(STAGE_MUTATION_COLUMN)),
            format!("ranked.{}", quote_identifier(STAGE_ORDER_TOKEN_COLUMN)),
        ])
        .collect::<Vec<_>>()
        .join(", ");
    let partition_by = business_keys
        .iter()
        .map(|column| format!("stage.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    let materialize_winners = format!(
        "CREATE TEMP TABLE {} ON COMMIT DROP AS\n\
         SELECT {winner_projection}\n\
         FROM (\n\
           SELECT stage.*,\n\
             ROW_NUMBER() OVER (\n\
               PARTITION BY {partition_by}\n\
               ORDER BY stage.{} DESC, stage.{} ASC\n\
             ) AS \"_skippr_cdc_rank\"\n\
           FROM pg_temp.{} AS stage\n\
         ) AS ranked\n\
         WHERE ranked.\"_skippr_cdc_rank\" = 1;",
        quote_identifier(WINNERS_TABLE_NAME),
        quote_identifier(STAGE_ORDER_TOKEN_COLUMN),
        quote_identifier(STAGE_ORDINAL_COLUMN),
        quote_identifier(STAGE_TABLE_NAME),
    );

    let target_insert_columns = target_columns
        .iter()
        .cloned()
        .chain(std::iter::once(quote_identifier(TARGET_ORDER_TOKEN_COLUMN)))
        .collect::<Vec<_>>();
    let winner_insert_values = target_columns
        .iter()
        .map(|column| format!("winner.{column}"))
        .chain(std::iter::once(format!(
            "winner.{}",
            quote_identifier(STAGE_ORDER_TOKEN_COLUMN)
        )))
        .collect::<Vec<_>>();
    let conflict_columns = business_keys.join(", ");

    let mut update_assignments = target_columns
        .iter()
        .filter(|column| !business_keys.contains(column))
        .map(|column| format!("{column} = EXCLUDED.{column}"))
        .collect::<Vec<_>>();
    update_assignments.push(format!(
        "{} = EXCLUDED.{}",
        quote_identifier(TARGET_ORDER_TOKEN_COLUMN),
        quote_identifier(TARGET_ORDER_TOKEN_COLUMN)
    ));

    let winner_to_tombstone = key_match("tombstone", "winner", &business_keys);
    let excluded_to_tombstone = key_match("tombstone", "EXCLUDED", &business_keys);
    let winner_to_target = key_match("target", "winner", &business_keys);
    let stage_token = quote_identifier(STAGE_ORDER_TOKEN_COLUMN);
    let target_token = quote_identifier(TARGET_ORDER_TOKEN_COLUMN);
    let mutation = quote_identifier(STAGE_MUTATION_COLUMN);
    let winners_table = format!("pg_temp.{}", quote_identifier(WINNERS_TABLE_NAME));

    let apply_upserts = format!(
        "INSERT INTO {fq_table} AS target ({})\n\
         SELECT {}\n\
         FROM {winners_table} AS winner\n\
         WHERE winner.{mutation} = 0\n\
         AND NOT EXISTS (\n\
           SELECT 1 FROM {fq_tombstone_table} AS tombstone\n\
           WHERE {winner_to_tombstone}\n\
           AND tombstone.{target_token} >= winner.{stage_token}\n\
         )\n\
         ON CONFLICT ({conflict_columns}) DO UPDATE SET {}\n\
         WHERE (\n\
           target.{target_token} IS NULL\n\
           OR target.{target_token} < EXCLUDED.{target_token}\n\
         )\n\
         AND NOT EXISTS (\n\
           SELECT 1 FROM {fq_tombstone_table} AS tombstone\n\
           WHERE {excluded_to_tombstone}\n\
           AND tombstone.{target_token} >= EXCLUDED.{target_token}\n\
         );",
        target_insert_columns.join(", "),
        winner_insert_values.join(", "),
        update_assignments.join(", "),
    );

    let apply_deletes = format!(
        "DELETE FROM {fq_table} AS target\n\
         USING {winners_table} AS winner\n\
         WHERE winner.{mutation} = 1\n\
         AND {winner_to_target}\n\
         AND (\n\
           target.{target_token} IS NULL\n\
           OR target.{target_token} < winner.{stage_token}\n\
         );"
    );

    let tombstone_insert_values = business_keys
        .iter()
        .map(|column| format!("winner.{column}"))
        .chain(std::iter::once(format!("winner.{stage_token}")))
        .collect::<Vec<_>>();
    let tombstone_columns = business_keys
        .iter()
        .cloned()
        .chain(std::iter::once(target_token.clone()))
        .collect::<Vec<_>>();
    let apply_tombstones = format!(
        "INSERT INTO {fq_tombstone_table} AS tombstone ({})\n\
         SELECT {}\n\
         FROM {winners_table} AS winner\n\
         WHERE winner.{mutation} = 1\n\
         ON CONFLICT ({conflict_columns}) DO UPDATE\n\
         SET {target_token} = EXCLUDED.{target_token}\n\
         WHERE tombstone.{target_token} < EXCLUDED.{target_token};",
        tombstone_columns.join(", "),
        tombstone_insert_values.join(", "),
    );

    let clear_stale_tombstones = format!(
        "DELETE FROM {fq_tombstone_table} AS tombstone\n\
         USING {winners_table} AS winner\n\
         WHERE winner.{mutation} = 0\n\
         AND {winner_to_tombstone}\n\
         AND tombstone.{target_token} < winner.{stage_token};"
    );

    Ok(PostgresBulkCdcSql {
        create_stage_table,
        copy_into_stage,
        materialize_winners,
        apply_winners: [
            apply_upserts,
            apply_deletes,
            apply_tombstones,
            clear_stale_tombstones,
        ]
        .join("\n"),
    })
}

/// Apply one bounded CDC batch with a native Postgres COPY text load and one
/// target transaction. A failed COPY or set-based statement is rolled back,
/// which also removes both temporary staging tables.
pub async fn apply_postgres_cdc_batch(
    client: &mut Client,
    fq_table: &str,
    fq_tombstone_table: &str,
    batch: &CdcApplyBatch,
) -> Result<usize, std::io::Error> {
    batch
        .validate()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if batch.rows.is_empty() {
        return Ok(0);
    }

    let sql = postgres_bulk_cdc_sql(fq_table, fq_tombstone_table, batch)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let payload = postgres_copy_text_payload(batch)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let transaction = client
        .transaction()
        .await
        .map_err(|error| postgres_error("begin CDC batch", error))?;

    let apply_result = load_and_apply(&transaction, &sql, payload, batch.rows.len()).await;
    match apply_result {
        Ok(()) => {
            transaction
                .commit()
                .await
                .map_err(|error| postgres_error("commit CDC batch", error))?;
            Ok(batch.rows.len())
        }
        Err(apply_error) => match transaction.rollback().await {
            Ok(()) => Err(apply_error),
            Err(rollback_error) => Err(std::io::Error::other(format!(
                "{apply_error}; Postgres rollback CDC batch failed: {rollback_error}"
            ))),
        },
    }
}

fn postgres_copy_text_payload(batch: &CdcApplyBatch) -> Result<String, CdcApplyBatchError> {
    batch.validate()?;

    let mut payload = String::new();
    for row in &batch.rows {
        payload.push_str(&row.metadata.source_ordinal.to_string());
        payload.push('\t');
        payload.push(match row.metadata.mutation {
            CdcApplyMutation::Upsert => '0',
            CdcApplyMutation::Delete => '1',
        });
        payload.push('\t');
        payload.push_str(&copy_text_escape(&format!(
            "\\x{}",
            encode_hex(&row.metadata.order_token)
        )));
        for value in &row.values {
            payload.push('\t');
            match value {
                CdcApplyValue::Null => payload.push_str("\\N"),
                _ => payload.push_str(&copy_text_escape(&cdc_value_text(value))),
            }
        }
        payload.push('\n');
    }
    Ok(payload)
}

async fn load_and_apply(
    transaction: &Transaction<'_>,
    sql: &PostgresBulkCdcSql,
    payload: String,
    expected_rows: usize,
) -> Result<(), std::io::Error> {
    transaction
        .batch_execute(&sql.create_stage_table)
        .await
        .map_err(|error| postgres_error("create CDC staging table", error))?;

    let sink = transaction
        .copy_in(&sql.copy_into_stage)
        .await
        .map_err(|error| postgres_error("start CDC staging COPY", error))?;
    let mut sink = std::pin::pin!(sink);
    sink.as_mut()
        .send(Bytes::from(payload))
        .await
        .map_err(|error| postgres_error("write CDC staging COPY", error))?;
    let copied_rows = sink
        .as_mut()
        .finish()
        .await
        .map_err(|error| postgres_error("finish CDC staging COPY", error))?;
    if copied_rows != expected_rows as u64 {
        return Err(std::io::Error::other(format!(
            "Postgres CDC staging COPY loaded {copied_rows} rows; expected {expected_rows}"
        )));
    }

    transaction
        .batch_execute(&format!(
            "{}\n{}",
            sql.materialize_winners, sql.apply_winners
        ))
        .await
        .map_err(|error| postgres_error("apply CDC staging batch", error))
}

fn postgres_error(context: &str, error: tokio_postgres::Error) -> std::io::Error {
    std::io::Error::other(format!("Postgres {context} failed: {error}"))
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn key_match(left: &str, right: &str, business_keys: &[String]) -> String {
    business_keys
        .iter()
        .map(|column| format!("{left}.{column} = {right}.{column}"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn cdc_value_text(value: &CdcApplyValue) -> String {
    match value {
        CdcApplyValue::Null => unreachable!("null COPY values use the COPY null marker"),
        CdcApplyValue::Boolean(value) => {
            if *value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        CdcApplyValue::Signed(value) => value.to_string(),
        CdcApplyValue::Unsigned(value) => value.to_string(),
        CdcApplyValue::Float(value)
        | CdcApplyValue::Text(value)
        | CdcApplyValue::Date(value)
        | CdcApplyValue::Timestamp(value) => value.clone(),
    }
}

fn copy_text_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\u{8}' => escaped.push_str("\\b"),
            '\u{c}' => escaped.push_str("\\f"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{b}' => escaped.push_str("\\v"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}
