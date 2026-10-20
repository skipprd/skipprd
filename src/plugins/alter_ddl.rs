use crate::discover::schema_alter::SchemaAlterOp;
use crate::discover::SkipprDataType;
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarehouseDialect {
    Postgres,
    Snowflake,
    BigQuery,
    Redshift,
    MotherDuck,
}

pub fn warehouse_alter_sql(
    dialect: WarehouseDialect,
    fq_table: &str,
    op: &SchemaAlterOp,
) -> Result<String, io::Error> {
    match op {
        SchemaAlterOp::Rename { from, to } => Ok(match dialect {
            WarehouseDialect::BigQuery => format!(
                "ALTER TABLE {fq_table} RENAME COLUMN {} TO {}",
                quote(dialect, from),
                quote(dialect, to)
            ),
            WarehouseDialect::Snowflake => format!(
                "ALTER TABLE {fq_table} RENAME COLUMN {} TO {}",
                quote(dialect, from),
                quote(dialect, to)
            ),
            _ => format!(
                "ALTER TABLE {fq_table} RENAME COLUMN {} TO {}",
                quote(dialect, from),
                quote(dialect, to)
            ),
        }),
        SchemaAlterOp::Drop { column } | SchemaAlterOp::Merge { src: column, .. } => Ok(format!(
            "ALTER TABLE {fq_table} DROP COLUMN {}",
            quote(dialect, column)
        )),
        SchemaAlterOp::Promote { column, to } => {
            let ty = sql_type(dialect, to)?;
            Ok(match dialect {
                WarehouseDialect::Snowflake => format!(
                    "ALTER TABLE {fq_table} ALTER COLUMN {} SET DATA TYPE {ty}",
                    quote(dialect, column)
                ),
                WarehouseDialect::BigQuery => format!(
                    "ALTER TABLE {fq_table} ALTER COLUMN {} SET DATA TYPE {ty}",
                    quote(dialect, column)
                ),
                _ => format!(
                    "ALTER TABLE {fq_table} ALTER COLUMN {} TYPE {ty}",
                    quote(dialect, column)
                ),
            })
        }
    }
}

fn quote(dialect: WarehouseDialect, ident: &str) -> String {
    let name = ident.replace('.', "_");
    match dialect {
        WarehouseDialect::BigQuery => format!("`{}`", name.replace('`', "``")),
        WarehouseDialect::Snowflake => format!("\"{}\"", name.replace('"', "\"\"").to_uppercase()),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

fn sql_type(dialect: WarehouseDialect, ty: &SkipprDataType) -> Result<&'static str, io::Error> {
    Ok(match (dialect, ty) {
        (_, SkipprDataType::Long) => match dialect {
            WarehouseDialect::BigQuery => "INT64",
            WarehouseDialect::Snowflake => "NUMBER",
            _ => "BIGINT",
        },
        (_, SkipprDataType::Integer) => match dialect {
            WarehouseDialect::BigQuery => "INT64",
            WarehouseDialect::Snowflake => "NUMBER",
            _ => "INTEGER",
        },
        (_, SkipprDataType::Double) => match dialect {
            WarehouseDialect::BigQuery => "FLOAT64",
            WarehouseDialect::Snowflake => "FLOAT",
            _ => "DOUBLE PRECISION",
        },
        (_, SkipprDataType::Timestamp) => match dialect {
            WarehouseDialect::BigQuery => "TIMESTAMP",
            _ => "TIMESTAMP",
        },
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("warehouse cannot promote to {ty:?}"),
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_rename_quotes_identifiers() {
        let sql = warehouse_alter_sql(
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            &SchemaAlterOp::Rename {
                from: "region".into(),
                to: "region_code".into(),
            },
        )
        .unwrap();
        assert_eq!(
            sql,
            "ALTER TABLE \"public\".\"trips\" RENAME COLUMN \"region\" TO \"region_code\""
        );
    }
}
