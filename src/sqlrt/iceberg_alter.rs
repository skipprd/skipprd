//! Apply [`SchemaAlterOp`] to an Iceberg table (field ids preserved).

use std::sync::Arc;

use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
use iceberg::transaction::{ApplyTransactionAction, Transaction};
use iceberg::TableIdent;

use crate::cluster::{IcebergLake, PipelineConfigView, QueryBackend};
use crate::discover::schema_alter::SchemaAlterOp;
use crate::discover::SkipprDataType;
use crate::helpers::configuration::Config;

pub fn iceberg_schema_after_alter(
    current: &Schema,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<Schema, String> {
    let mut fields: Vec<Arc<NestedField>> = current.as_struct().fields().iter().cloned().collect();
    match op {
        SchemaAlterOp::Rename { to, .. } => {
            if to.contains('.') {
                return Err("RENAME COLUMN target must be a single field name".into());
            }
            let Some(idx) = fields.iter().position(|field| field.id == field_id) else {
                return Err(format!("Iceberg field id {field_id} not found"));
            };
            if fields[idx].name == *to {
                return Ok(current.clone());
            }
            if fields
                .iter()
                .any(|field| field.name == *to && field.id != field_id)
            {
                return Err(format!("Iceberg column '{to}' already exists"));
            }
            let old = fields[idx].as_ref().clone();
            fields[idx] = Arc::new(NestedField::new(
                old.id,
                to,
                old.field_type.as_ref().clone(),
                old.required,
            ));
        }
        SchemaAlterOp::Drop { column } | SchemaAlterOp::Merge { src: column, .. } => {
            let before = fields.len();
            fields.retain(|field| field.id != field_id);
            if fields.len() == before {
                if column.contains('.') {
                    return Err(format!(
                        "Iceberg field id {field_id} ('{column}') not found; nested ALTER is not supported"
                    ));
                }
                return Ok(current.clone());
            }
        }
        SchemaAlterOp::Promote { to, .. } => {
            let idx = fields
                .iter()
                .position(|field| field.id == field_id)
                .ok_or_else(|| format!("Iceberg field id {field_id} not found"))?;
            if fields[idx].field_type.as_ref() == &iceberg_primitive(to) {
                return Ok(current.clone());
            }
            let old = fields[idx].as_ref().clone();
            fields[idx] = Arc::new(NestedField::new(
                old.id,
                old.name,
                iceberg_primitive(to),
                old.required,
            ));
        }
    }
    Schema::builder()
        .with_schema_id(0)
        .with_fields(fields)
        .build()
        .map_err(|err| err.to_string())
}

fn iceberg_primitive(value: &SkipprDataType) -> Type {
    let primitive = match value {
        SkipprDataType::Boolean => PrimitiveType::Boolean,
        SkipprDataType::Byte
        | SkipprDataType::Short
        | SkipprDataType::Integer
        | SkipprDataType::Unknown
        | SkipprDataType::Null => PrimitiveType::Int,
        SkipprDataType::Long => PrimitiveType::Long,
        SkipprDataType::Float => PrimitiveType::Float,
        SkipprDataType::Double => PrimitiveType::Double,
        SkipprDataType::Decimal => PrimitiveType::Decimal {
            precision: 38,
            scale: 9,
        },
        SkipprDataType::Date => PrimitiveType::Date,
        SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => PrimitiveType::Timestamp,
        SkipprDataType::Time => PrimitiveType::Time,
        _ => PrimitiveType::String,
    };
    Type::Primitive(primitive)
}

pub async fn commit_schema_alter(
    config: &Config,
    pipeline: &str,
    table: &str,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<(), String> {
    let view = PipelineConfigView::for_name(config, pipeline).map_err(|err| err.to_string())?;
    let lake = match &view.backend {
        QueryBackend::Iceberg(lake) => lake,
        QueryBackend::WalOnly => {
            return Err("ALTER TABLE requires an Iceberg sink".into());
        }
    };
    commit_on_lake(lake, table, op, field_id).await
}

async fn commit_on_lake(
    lake: &IcebergLake,
    table: &str,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<(), String> {
    let catalog = crate::cluster::backend::open_iceberg_catalog(&lake.catalog).await?;
    let ident = TableIdent::from_strs([lake.ingest_namespace.as_str(), table])
        .map_err(|err| err.to_string())?;
    let loaded = catalog
        .load_table(&ident)
        .await
        .map_err(|err| err.to_string())?;
    let next = iceberg_schema_after_alter(loaded.metadata().current_schema(), op, field_id)?;
    let tx = Transaction::new(&loaded);
    let tx = tx
        .replace_schema(next)
        .apply(tx)
        .map_err(|err| err.to_string())?;
    tx.commit(catalog.as_ref())
        .await
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_keeps_field_id() {
        let current = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![Arc::new(NestedField::new(
                7,
                "price",
                Type::Primitive(PrimitiveType::Long),
                false,
            ))])
            .build()
            .unwrap();
        let next = iceberg_schema_after_alter(
            &current,
            &SchemaAlterOp::Rename {
                from: "price".into(),
                to: "amount".into(),
            },
            7,
        )
        .unwrap();
        assert_eq!(next.field_by_name("amount").unwrap().id, 7);
        assert!(next.field_by_name("price").is_none());
        let already = iceberg_schema_after_alter(
            &next,
            &SchemaAlterOp::Rename {
                from: "price".into(),
                to: "amount".into(),
            },
            7,
        )
        .unwrap();
        assert_eq!(already.field_by_name("amount").unwrap().id, 7);
    }

    fn two_cols() -> Schema {
        Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![
                Arc::new(NestedField::new(
                    7,
                    "price",
                    Type::Primitive(PrimitiveType::Long),
                    false,
                )),
                Arc::new(NestedField::new(
                    9,
                    "price_string",
                    Type::Primitive(PrimitiveType::String),
                    false,
                )),
            ])
            .build()
            .unwrap()
    }

    #[test]
    fn drop_nested_path_without_id_fails_closed() {
        let err = iceberg_schema_after_alter(
            &two_cols(),
            &SchemaAlterOp::Drop {
                column: "hardware.manufacturer".into(),
            },
            99,
        )
        .unwrap_err();
        assert!(err.contains("nested ALTER is not supported"), "{err}");
    }

    #[test]
    fn drop_already_absent_is_idempotent() {
        let next = iceberg_schema_after_alter(
            &two_cols(),
            &SchemaAlterOp::Drop {
                column: "missing".into(),
            },
            11,
        )
        .unwrap();
        assert_eq!(next.field_by_name("price").unwrap().id, 7);
    }

    #[test]
    fn drop_uses_field_id_not_name() {
        let current = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![Arc::new(NestedField::new(
                9,
                "other",
                Type::Primitive(PrimitiveType::String),
                false,
            ))])
            .build()
            .unwrap();
        let next = iceberg_schema_after_alter(
            &current,
            &SchemaAlterOp::Drop {
                column: "price_string".into(),
            },
            9,
        )
        .unwrap();
        assert!(next.field_by_name("other").is_none());
        assert!(next.as_struct().fields().is_empty());
    }

    #[test]
    fn drop_omits_column_keeps_other_id() {
        let next = iceberg_schema_after_alter(
            &two_cols(),
            &SchemaAlterOp::Drop {
                column: "price_string".into(),
            },
            9,
        )
        .unwrap();
        assert!(next.field_by_name("price_string").is_none());
        assert_eq!(next.field_by_name("price").unwrap().id, 7);
    }

    #[test]
    fn merge_omits_src_does_not_rewrite_dst_id() {
        let next = iceberg_schema_after_alter(
            &two_cols(),
            &SchemaAlterOp::Merge {
                src: "price_string".into(),
                dst: "price".into(),
            },
            9,
        )
        .unwrap();
        assert!(next.field_by_name("price_string").is_none());
        assert_eq!(next.field_by_name("price").unwrap().id, 7);
    }

    #[test]
    fn promote_keeps_field_id() {
        let current = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![Arc::new(NestedField::new(
                3,
                "n",
                Type::Primitive(PrimitiveType::Int),
                false,
            ))])
            .build()
            .unwrap();
        let next = iceberg_schema_after_alter(
            &current,
            &SchemaAlterOp::Promote {
                column: "n".into(),
                to: SkipprDataType::Long,
            },
            3,
        )
        .unwrap();
        let field = next.field_by_name("n").unwrap();
        assert_eq!(field.id, 3);
        assert_eq!(
            field.field_type.as_ref(),
            &Type::Primitive(PrimitiveType::Long)
        );
        let already = iceberg_schema_after_alter(
            &next,
            &SchemaAlterOp::Promote {
                column: "n".into(),
                to: SkipprDataType::Long,
            },
            3,
        )
        .unwrap();
        assert_eq!(already.field_by_name("n").unwrap().id, 3);
    }
}
