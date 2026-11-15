use crate::discover::schema_alter::{CanonicalAlter, CommittedAlter, OutputName};
use crate::discover::SkipprDataType;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WarehouseDialect {
    Postgres,
    Snowflake,
    BigQuery,
    Redshift,
    MotherDuck,
}

/// Warehouse I/O behind the shared ALTER. Implemented by each warehouse sink.
#[async_trait]
pub trait WarehouseExec: Send + Sync {
    /// Column names of the namespace's table as the warehouse reports them.
    async fn columns(&self, namespace: &str) -> io::Result<Vec<String>>;
    async fn execute(&self, sql: &str) -> io::Result<()>;
    /// First cell of the first row, as text. Used to read `skippr_alter_seq`.
    async fn query_first(&self, sql: &str) -> io::Result<Option<String>>;
}

/// Dest high-water. Missing/blank is unset (seq 0 applies). Garbage fails closed.
pub fn parse_dest_high_water(raw: Option<&str>) -> io::Result<Option<u64>> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    raw.parse().map(Some).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("dest alter seq {raw:?} is not u64: {err}"),
        )
    })
}

/// The only warehouse ALTER path. `CommittedAlter.seq` is the destination
/// high-water (`skippr_alter_seq`). Statements already reflected in the
/// table's columns are also skipped, so a crash between DDL and the stamp
/// still resumes.
pub async fn apply_warehouse_alter(
    exec: &dyn WarehouseExec,
    dialect: WarehouseDialect,
    fq_table: &str,
    namespace: &str,
    alter: &CommittedAlter,
) -> io::Result<()> {
    let rel = seq_relation(dialect, fq_table);
    exec.execute(&create_alter_seq_sql(dialect, &rel)).await?;
    let current = parse_dest_high_water(
        exec.query_first(&read_alter_seq_sql(&rel, namespace))
            .await?
            .as_deref(),
    )?;
    if current.is_some_and(|seq| seq >= alter.seq) {
        return Ok(());
    }
    let existing = exec.columns(namespace).await?;
    for sql in warehouse_alter_sql(dialect, fq_table, &alter.alter, &existing)? {
        exec.execute(&sql).await?;
    }
    for sql in stamp_alter_seq_sql(dialect, &rel, namespace, alter.seq) {
        exec.execute(&sql).await?;
    }
    Ok(())
}

fn seq_relation(dialect: WarehouseDialect, fq_table: &str) -> String {
    let name = quote(dialect, "skippr_alter_seq");
    match fq_table.rsplit_once('.') {
        Some((prefix, _)) => format!("{prefix}.{name}"),
        None => name,
    }
}

fn create_alter_seq_sql(dialect: WarehouseDialect, rel: &str) -> String {
    match dialect {
        WarehouseDialect::BigQuery => {
            format!("CREATE TABLE IF NOT EXISTS {rel} (namespace STRING, seq INT64)")
        }
        WarehouseDialect::Snowflake => {
            format!("CREATE TABLE IF NOT EXISTS {rel} (namespace VARCHAR PRIMARY KEY, seq NUMBER)")
        }
        _ => format!(
            "CREATE TABLE IF NOT EXISTS {rel} (namespace VARCHAR(256) PRIMARY KEY, seq BIGINT NOT NULL)"
        ),
    }
}

fn read_alter_seq_sql(rel: &str, namespace: &str) -> String {
    let ns = namespace.replace('\'', "''");
    format!("SELECT CAST(seq AS VARCHAR) FROM {rel} WHERE namespace = '{ns}'")
}

fn stamp_alter_seq_sql(
    dialect: WarehouseDialect,
    rel: &str,
    namespace: &str,
    seq: u64,
) -> Vec<String> {
    let ns = namespace.replace('\'', "''");
    match dialect {
        WarehouseDialect::Postgres | WarehouseDialect::MotherDuck => vec![format!(
            "INSERT INTO {rel} (namespace, seq) VALUES ('{ns}', {seq}) ON CONFLICT (namespace) DO UPDATE SET seq = EXCLUDED.seq"
        )],
        WarehouseDialect::Redshift => vec![
            format!("DELETE FROM {rel} WHERE namespace = '{ns}'"),
            format!("INSERT INTO {rel} (namespace, seq) VALUES ('{ns}', {seq})"),
        ],
        WarehouseDialect::Snowflake | WarehouseDialect::BigQuery => vec![format!(
            "MERGE INTO {rel} t USING (SELECT '{ns}' AS namespace, {seq} AS seq) s ON t.namespace = s.namespace WHEN MATCHED THEN UPDATE SET seq = s.seq WHEN NOT MATCHED THEN INSERT (namespace, seq) VALUES (s.namespace, s.seq)"
        )],
    }
}

pub fn warehouse_alter_sql(
    dialect: WarehouseDialect,
    fq_table: &str,
    alter: &CanonicalAlter,
    existing: &[String],
) -> io::Result<Vec<String>> {
    let has = |name: &OutputName| {
        let want = fold(dialect, name.column());
        existing.iter().any(|column| fold(dialect, column) == want)
    };
    let col = |name: &OutputName| quote(dialect, name.column());
    let drop = |name: &OutputName| format!("ALTER TABLE {fq_table} DROP COLUMN {}", col(name));
    Ok(match alter {
        CanonicalAlter::Rename { column, .. } => {
            let to = alter
                .rename_target()
                .map_err(invalid)?
                .unwrap_or_else(|| column.name.clone());
            if !has(&column.name) && has(&to) {
                Vec::new()
            } else {
                vec![format!(
                    "ALTER TABLE {fq_table} RENAME COLUMN {} TO {}",
                    col(&column.name),
                    col(&to)
                )]
            }
        }
        CanonicalAlter::Drop { column } if has(&column.name) => vec![drop(&column.name)],
        CanonicalAlter::Drop { .. } => Vec::new(),
        CanonicalAlter::Merge { src, dst } if has(&src.name) => vec![
            format!(
                "UPDATE {fq_table} SET {dst} = COALESCE({dst}, CAST({src} AS {ty})) WHERE {src} IS NOT NULL",
                dst = col(&dst.name),
                src = col(&src.name),
                ty = sql_type(dialect, &dst.ty)?,
            ),
            drop(&src.name),
        ],
        CanonicalAlter::Merge { .. } => Vec::new(),
        CanonicalAlter::Promote { column, to } => {
            let ty = sql_type(dialect, to)?;
            let column = col(&column.name);
            vec![match dialect {
                WarehouseDialect::Snowflake | WarehouseDialect::BigQuery => {
                    format!("ALTER TABLE {fq_table} ALTER COLUMN {column} SET DATA TYPE {ty}")
                }
                _ => format!("ALTER TABLE {fq_table} ALTER COLUMN {column} TYPE {ty}"),
            }]
        }
    })
}

fn invalid(err: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, err)
}

/// First column of a warehouse SQL JSON body (Snowflake `data`, BigQuery/MotherDuck `rows`).
pub fn json_result_names(body: &serde_json::Value) -> io::Result<Vec<String>> {
    json_rows(body)?
        .iter()
        .map(|row| {
            first_cell(row)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "warehouse column row has no cell",
                )
            })
        })
        .collect()
}

/// First cell of the first row as text. Numbers stringify. Missing `data`/`rows`
/// array fails closed. A present empty array is unset. Unreadable cells fail closed.
pub fn json_result_first(body: &serde_json::Value) -> io::Result<Option<String>> {
    match json_rows(body)?.first() {
        None => Ok(None),
        Some(row) => first_cell(row),
    }
}

fn json_rows(body: &serde_json::Value) -> io::Result<&[serde_json::Value]> {
    match body.get("data").or_else(|| body.get("rows")) {
        Some(serde_json::Value::Array(rows)) => Ok(rows.as_slice()),
        Some(other) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("warehouse result data/rows is not an array: {other}"),
        )),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("warehouse result has no data/rows array: {body}"),
        )),
    }
}

fn first_cell(row: &serde_json::Value) -> io::Result<Option<String>> {
    if let Some(name) = row.as_str() {
        return Ok(Some(name.to_string()));
    }
    if let Some(cells) = row.as_array() {
        return match cells.first() {
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "warehouse result row is an empty array",
            )),
            Some(cell) => json_cell_text(cell).map(Some),
        };
    }
    match row.pointer("/f/0/v") {
        Some(cell) => json_cell_text(cell).map(Some),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("warehouse result row is not a scalar, array, or BigQuery cell: {row}"),
        )),
    }
}

fn json_cell_text(value: &serde_json::Value) -> io::Result<String> {
    match value {
        serde_json::Value::String(text) => Ok(text.clone()),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        serde_json::Value::Bool(flag) => Ok(flag.to_string()),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("warehouse cell is not text or number: {other}"),
        )),
    }
}

/// Identifier case as each warehouse sink creates its columns.
fn fold(dialect: WarehouseDialect, ident: &str) -> String {
    match dialect {
        WarehouseDialect::Snowflake => ident.to_uppercase(),
        WarehouseDialect::Postgres | WarehouseDialect::Redshift => ident.to_lowercase(),
        WarehouseDialect::BigQuery | WarehouseDialect::MotherDuck => ident.to_string(),
    }
}

fn quote(dialect: WarehouseDialect, ident: &str) -> String {
    let ident = fold(dialect, ident);
    match dialect {
        WarehouseDialect::BigQuery => format!("`{}`", ident.replace('`', "``")),
        _ => format!("\"{}\"", ident.replace('"', "\"\"")),
    }
}

fn sql_type(dialect: WarehouseDialect, ty: &SkipprDataType) -> io::Result<&'static str> {
    use WarehouseDialect::*;
    Ok(match ty {
        SkipprDataType::Long => match dialect {
            BigQuery => "INT64",
            Snowflake => "NUMBER",
            _ => "BIGINT",
        },
        SkipprDataType::Byte | SkipprDataType::Short | SkipprDataType::Integer => match dialect {
            BigQuery => "INT64",
            Snowflake => "NUMBER",
            _ => "INTEGER",
        },
        SkipprDataType::Double | SkipprDataType::Float => match dialect {
            BigQuery => "FLOAT64",
            Snowflake => "FLOAT",
            _ => "DOUBLE PRECISION",
        },
        SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => "TIMESTAMP",
        SkipprDataType::Date => "DATE",
        SkipprDataType::Boolean => match dialect {
            BigQuery => "BOOL",
            _ => "BOOLEAN",
        },
        SkipprDataType::String => match dialect {
            BigQuery => "STRING",
            Snowflake => "VARCHAR",
            _ => "TEXT",
        },
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("warehouse ALTER has no SQL type for {ty}"),
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::schema_alter::{AlterColumn, FlattenMode};

    const ALL: [WarehouseDialect; 5] = [
        WarehouseDialect::Postgres,
        WarehouseDialect::Snowflake,
        WarehouseDialect::BigQuery,
        WarehouseDialect::Redshift,
        WarehouseDialect::MotherDuck,
    ];

    fn column(name: &str, id: i32, ty: SkipprDataType) -> AlterColumn {
        AlterColumn {
            name: OutputName::parse_sql(FlattenMode::Flat, name).unwrap(),
            id,
            ty,
        }
    }

    fn merge() -> CanonicalAlter {
        CanonicalAlter::Merge {
            src: column("price_string", 9, SkipprDataType::String),
            dst: column("price", 7, SkipprDataType::String),
        }
    }

    fn rename() -> CanonicalAlter {
        CanonicalAlter::Rename {
            column: column("region", 21, SkipprDataType::String),
            to_leaf: "region_code".into(),
        }
    }

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn postgres_rename_quotes_identifiers() {
        let sql = warehouse_alter_sql(
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            &rename(),
            &cols(&["region"]),
        )
        .unwrap();
        assert_eq!(
            sql,
            vec!["ALTER TABLE \"public\".\"trips\" RENAME COLUMN \"region\" TO \"region_code\""]
        );
    }

    #[test]
    fn postgres_merge_is_update_then_drop() {
        let sql = warehouse_alter_sql(
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            &merge(),
            &cols(&["price", "price_string"]),
        )
        .unwrap();
        assert_eq!(
            sql,
            vec![
                "UPDATE \"public\".\"trips\" SET \"price\" = COALESCE(\"price\", CAST(\"price_string\" AS TEXT)) WHERE \"price_string\" IS NOT NULL",
                "ALTER TABLE \"public\".\"trips\" DROP COLUMN \"price_string\"",
            ]
        );
    }

    #[test]
    fn merge_casts_to_dst_type_for_every_dialect() {
        let alter = CanonicalAlter::Merge {
            src: column("n", 3, SkipprDataType::Integer),
            dst: column("n_string", 4, SkipprDataType::String),
        };
        for dialect in ALL {
            let sql = warehouse_alter_sql(dialect, "t", &alter, &cols(&["n", "n_string"])).unwrap();
            assert_eq!(sql.len(), 2, "{dialect:?}");
            assert!(sql[0].contains("COALESCE"), "{dialect:?} {sql:?}");
            assert!(sql[0].contains(sql_type(dialect, &SkipprDataType::String).unwrap()));
            assert!(sql[1].contains("DROP COLUMN"), "{dialect:?} {sql:?}");
        }
    }

    #[test]
    fn resume_skips_statements_already_applied() {
        for dialect in ALL {
            assert!(
                warehouse_alter_sql(dialect, "t", &rename(), &cols(&["region_code"]))
                    .unwrap()
                    .is_empty(),
                "{dialect:?}"
            );
            assert!(
                warehouse_alter_sql(dialect, "t", &merge(), &cols(&["price"]))
                    .unwrap()
                    .is_empty()
            );
            let drop = CanonicalAlter::Drop {
                column: column("note", 8, SkipprDataType::String),
            };
            assert!(warehouse_alter_sql(dialect, "t", &drop, &cols(&["price"]))
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn identifiers_fold_like_the_sink_creates_them() {
        let snowflake = warehouse_alter_sql(
            WarehouseDialect::Snowflake,
            "t",
            &merge(),
            &cols(&["PRICE_STRING"]),
        )
        .unwrap();
        assert!(snowflake[0].contains("\"PRICE\""), "{snowflake:?}");
        assert!(snowflake[0].contains("VARCHAR"), "{snowflake:?}");
        let bigquery = warehouse_alter_sql(
            WarehouseDialect::BigQuery,
            "t",
            &merge(),
            &cols(&["price_string"]),
        )
        .unwrap();
        assert!(bigquery[0].contains("`price`"), "{bigquery:?}");
    }

    struct FakeExec {
        cols: Vec<String>,
        seq: std::sync::Mutex<Option<String>>,
        ddl: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl WarehouseExec for FakeExec {
        async fn columns(&self, _namespace: &str) -> io::Result<Vec<String>> {
            Ok(self.cols.clone())
        }
        async fn execute(&self, sql: &str) -> io::Result<()> {
            self.ddl.lock().unwrap().push(sql.to_string());
            if sql.contains("skippr_alter_seq") && !sql.contains("CREATE TABLE") {
                if let Some(seq) = sql
                    .split(|ch: char| !ch.is_ascii_digit())
                    .filter_map(|part| part.parse::<u64>().ok())
                    .last()
                {
                    *self.seq.lock().unwrap() = Some(seq.to_string());
                }
            }
            Ok(())
        }
        async fn query_first(&self, _sql: &str) -> io::Result<Option<String>> {
            Ok(self.seq.lock().unwrap().clone())
        }
    }

    fn committed(alter: CanonicalAlter, seq: u64) -> CommittedAlter {
        use crate::discover::schema_alter::{AlterOrigin, MigrationDirection, MigrationId};
        CommittedAlter {
            origin: AlterOrigin {
                migration: MigrationId::new("t"),
                direction: MigrationDirection::Up,
            },
            seq,
            alter,
        }
    }

    #[tokio::test]
    async fn missing_seq_applies_then_high_water_skips() {
        let exec = FakeExec {
            cols: cols(&["region"]),
            seq: std::sync::Mutex::new(None),
            ddl: std::sync::Mutex::new(Vec::new()),
        };
        apply_warehouse_alter(
            &exec,
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            "trips",
            &committed(rename(), 0),
        )
        .await
        .unwrap();
        let first = exec.ddl.lock().unwrap().clone();
        assert!(
            first.iter().any(|sql| sql.contains("RENAME COLUMN")),
            "{first:?}"
        );
        exec.ddl.lock().unwrap().clear();
        apply_warehouse_alter(
            &exec,
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            "trips",
            &committed(rename(), 0),
        )
        .await
        .unwrap();
        let retry = exec.ddl.lock().unwrap().clone();
        assert!(
            retry.iter().all(|sql| !sql.contains("RENAME COLUMN")),
            "{retry:?}"
        );
    }

    #[tokio::test]
    async fn garbage_seq_fails_closed_before_ddl() {
        let exec = FakeExec {
            cols: cols(&["region"]),
            seq: std::sync::Mutex::new(Some("nope".into())),
            ddl: std::sync::Mutex::new(Vec::new()),
        };
        let err = apply_warehouse_alter(
            &exec,
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            "trips",
            &committed(rename(), 0),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not u64"), "{err}");
        let ddl = exec.ddl.lock().unwrap().clone();
        assert!(
            ddl.iter().all(|sql| !sql.contains("RENAME COLUMN")),
            "{ddl:?}"
        );
    }

    #[test]
    fn parse_dest_high_water_missing_is_unset_garbage_fails() {
        assert_eq!(parse_dest_high_water(None).unwrap(), None);
        assert_eq!(parse_dest_high_water(Some("")).unwrap(), None);
        assert_eq!(parse_dest_high_water(Some("  ")).unwrap(), None);
        assert_eq!(parse_dest_high_water(Some("0")).unwrap(), Some(0));
        assert!(parse_dest_high_water(Some("nope")).is_err());
    }

    #[test]
    fn json_result_first_stringifies_numbers() {
        let snowflake = serde_json::json!({"data": [[0]]});
        assert_eq!(json_result_first(&snowflake).unwrap().as_deref(), Some("0"));
        let named = serde_json::json!({"data": ["region"]});
        assert_eq!(
            json_result_first(&named).unwrap().as_deref(),
            Some("region")
        );
        let empty = serde_json::json!({"data": []});
        assert_eq!(json_result_first(&empty).unwrap(), None);
        let object = serde_json::json!({"data": [[{"seq": 1}]]});
        assert!(json_result_first(&object).is_err());
        assert!(json_result_first(&serde_json::json!({"message": "denied"})).is_err());
        assert!(json_result_first(&serde_json::json!({"data": {"seq": 1}})).is_err());
        assert!(json_result_first(&serde_json::json!({"data": [{"seq": 1}]})).is_err());
        assert!(json_result_names(&serde_json::json!({"message": "denied"})).is_err());
        assert_eq!(
            json_result_names(&serde_json::json!({"data": ["region"]})).unwrap(),
            vec!["region"]
        );
    }
}
