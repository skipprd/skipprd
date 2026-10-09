//! Apply [`SchemaAlterOp`] to an Iceberg table (field ids preserved).

use std::sync::Arc;

use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
use iceberg::transaction::{ApplyTransactionAction, Transaction};
use iceberg::TableIdent;

use crate::cluster::{IcebergLake, PipelineConfigView, QueryBackend};
use crate::discover::schema_alter::SchemaAlterOp;
use crate::discover::SkipprDataType;
use crate::helpers::configuration::Config;

fn iceberg_name_candidates(name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    if name.contains('.') {
        out.push(name.replace('.', "_"));
    }
    if name.contains('-') {
        out.push(name.replace('-', "_"));
        out.push(name.replace('.', "_").replace('-', "_"));
    }
    out
}

fn iceberg_field_name(name: &str) -> String {
    name.replace('.', "_").replace('-', "_")
}

fn lookup_op_name(op: &SchemaAlterOp) -> &str {
    match op {
        SchemaAlterOp::Rename { from, .. } => from.as_str(),
        SchemaAlterOp::Merge { src, .. } => src.as_str(),
        SchemaAlterOp::Drop { column } | SchemaAlterOp::Promote { column, .. } => column.as_str(),
    }
}

fn resolve_iceberg_field_index(
    fields: &[Arc<NestedField>],
    op: &SchemaAlterOp,
    field_id: i32,
) -> Option<usize> {
    let candidates = iceberg_name_candidates(lookup_op_name(op));
    let name_idx = fields
        .iter()
        .position(|field| candidates.iter().any(|name| field.name == *name));
    if let Some(idx) = fields.iter().position(|field| field.id == field_id) {
        if candidates.iter().any(|name| fields[idx].name == *name) {
            return Some(idx);
        }
        // Glue sequential ids (1..N) can collide with a skippr assigned id on a
        // different column. Prefer the name hit when one exists; keep id when
        // Iceberg already renamed and only the id still matches.
        if name_idx.is_none() {
            return Some(idx);
        }
    }
    name_idx
}

pub fn iceberg_schema_after_alter(
    current: &Schema,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<Schema, String> {
    let mut fields: Vec<Arc<NestedField>> = current.as_struct().fields().iter().cloned().collect();
    match op {
        SchemaAlterOp::Rename { to, .. } => {
            let new_name = iceberg_field_name(to);
            let Some(idx) = resolve_iceberg_field_index(&fields, op, field_id) else {
                if fields
                    .iter()
                    .any(|field| field.name == new_name || field.name == *to)
                {
                    return Ok(current.clone());
                }
                return Err(format!("Iceberg field id {field_id} not found"));
            };
            if fields[idx].name == new_name {
                return Ok(current.clone());
            }
            if fields
                .iter()
                .any(|field| field.name == new_name && field.id != fields[idx].id)
            {
                return Err(format!("Iceberg column '{new_name}' already exists"));
            }
            let old = fields[idx].as_ref().clone();
            fields[idx] = Arc::new(NestedField::new(
                old.id,
                new_name,
                old.field_type.as_ref().clone(),
                old.required,
            ));
        }
        SchemaAlterOp::Drop { column } | SchemaAlterOp::Merge { src: column, .. } => {
            let Some(idx) = resolve_iceberg_field_index(&fields, op, field_id) else {
                if column.contains('.') {
                    return Err(format!(
                        "Iceberg field id {field_id} ('{column}') not found; nested ALTER is not supported"
                    ));
                }
                return Ok(current.clone());
            };
            let drop_id = fields[idx].id;
            fields.retain(|field| field.id != drop_id);
        }
        SchemaAlterOp::Promote { to, .. } => {
            let idx = resolve_iceberg_field_index(&fields, op, field_id)
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
    fn sequential_id_collision_does_not_mutate_wrong_column() {
        let current = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![
                Arc::new(NestedField::new(
                    1,
                    "region",
                    Type::Primitive(PrimitiveType::String),
                    false,
                )),
                Arc::new(NestedField::new(
                    8,
                    "n_float",
                    Type::Primitive(PrimitiveType::Float),
                    false,
                )),
                Arc::new(NestedField::new(
                    3,
                    "note",
                    Type::Primitive(PrimitiveType::String),
                    false,
                )),
            ])
            .build()
            .unwrap();
        let next = iceberg_schema_after_alter(
            &current,
            &SchemaAlterOp::Drop {
                column: "note".into(),
            },
            8,
        )
        .unwrap();
        assert!(next.field_by_name("note").is_none());
        assert_eq!(next.field_by_name("n_float").unwrap().id, 8);
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
    fn rename_matches_iceberg_name_when_skippr_field_id_diverges() {
        let current = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![Arc::new(NestedField::new(
                21,
                "region",
                Type::Primitive(PrimitiveType::String),
                false,
            ))])
            .build()
            .unwrap();
        let next = iceberg_schema_after_alter(
            &current,
            &SchemaAlterOp::Rename {
                from: "region".into(),
                to: "region_code".into(),
            },
            575921751,
        )
        .unwrap();
        let field = next.field_by_name("region_code").unwrap();
        assert_eq!(field.id, 21);
        assert!(next.field_by_name("region").is_none());
    }

    #[test]
    fn rename_to_nested_path_writes_flatten_name() {
        let current = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![
                Arc::new(NestedField::new(
                    21,
                    "region",
                    Type::Primitive(PrimitiveType::String),
                    false,
                )),
                Arc::new(NestedField::new(
                    4,
                    "detail_truck_reg",
                    Type::Primitive(PrimitiveType::String),
                    false,
                )),
            ])
            .build()
            .unwrap();
        let next = iceberg_schema_after_alter(
            &current,
            &SchemaAlterOp::Rename {
                from: "region".into(),
                to: "detail.region".into(),
            },
            575921751,
        )
        .unwrap();
        assert_eq!(next.field_by_name("detail_region").unwrap().id, 21);
        assert!(next.field_by_name("region").is_none());
        assert_eq!(next.field_by_name("detail_truck_reg").unwrap().id, 4);
    }

    #[test]
    fn rename_matches_flattened_iceberg_name_from_dotted_skippr_path() {
        let current = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![Arc::new(NestedField::new(
                4,
                "detail_truck_reg",
                Type::Primitive(PrimitiveType::String),
                false,
            ))])
            .build()
            .unwrap();
        let next = iceberg_schema_after_alter(
            &current,
            &SchemaAlterOp::Rename {
                from: "detail.truck_reg".into(),
                to: "truck_registration".into(),
            },
            1642008034,
        )
        .unwrap();
        assert_eq!(next.field_by_name("truck_registration").unwrap().id, 4);
        assert!(next.field_by_name("detail_truck_reg").is_none());
    }

    #[test]
    fn promote_matches_iceberg_name_when_skippr_field_id_diverges() {
        let current = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![Arc::new(NestedField::new(
                25,
                "version",
                Type::Primitive(PrimitiveType::Int),
                false,
            ))])
            .build()
            .unwrap();
        let next = iceberg_schema_after_alter(
            &current,
            &SchemaAlterOp::Promote {
                column: "version".into(),
                to: SkipprDataType::Long,
            },
            298041464,
        )
        .unwrap();
        let field = next.field_by_name("version").unwrap();
        assert_eq!(field.id, 25);
        assert_eq!(
            field.field_type.as_ref(),
            &Type::Primitive(PrimitiveType::Long)
        );
    }

    #[test]
    fn drop_matches_flattened_iceberg_name() {
        let current = Schema::builder()
            .with_schema_id(0)
            .with_fields(vec![
                Arc::new(NestedField::new(
                    23,
                    "detail_geofence_id",
                    Type::Primitive(PrimitiveType::String),
                    false,
                )),
                Arc::new(NestedField::new(
                    12,
                    "detail_record_geofence_id",
                    Type::Primitive(PrimitiveType::String),
                    false,
                )),
            ])
            .build()
            .unwrap();
        let next = iceberg_schema_after_alter(
            &current,
            &SchemaAlterOp::Merge {
                src: "detail_record.geofence_id".into(),
                dst: "detail.geofence_id".into(),
            },
            370859891,
        )
        .unwrap();
        assert!(next.field_by_name("detail_record_geofence_id").is_none());
        assert_eq!(next.field_by_name("detail_geofence_id").unwrap().id, 23);
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
