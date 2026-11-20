//! Iceberg schema after one [`CanonicalAlter`]. Columns are found by output
//! name and must carry the alter's field id. Destination apply lives on
//! IcebergWriter (`DataSink::apply_alter`).

use std::sync::Arc;

use iceberg::spec::{NestedField, PrimitiveType, Schema, StructType, Type};

use crate::discover::schema_alter::{AlterColumn, CanonicalAlter, SchemaAlterError};
use crate::discover::SkipprDataType;

type Fields = Vec<Arc<NestedField>>;

pub fn iceberg_schema_after_alter(
    current: &Schema,
    alter: &CanonicalAlter,
) -> Result<Schema, SchemaAlterError> {
    let mut fields: Fields = current.as_struct().fields().to_vec();
    match alter {
        CanonicalAlter::Rename { column, to_leaf } => {
            edit(&mut fields, column, |siblings, idx| {
                if siblings
                    .iter()
                    .enumerate()
                    .any(|(i, field)| i != idx && field.name == *to_leaf)
                {
                    return Err(SchemaAlterError::ColumnExists(to_leaf.clone()));
                }
                let mut renamed = siblings[idx].as_ref().clone();
                renamed.name = to_leaf.clone();
                siblings[idx] = Arc::new(renamed);
                Ok(())
            })?;
        }
        CanonicalAlter::Drop { column } => {
            edit(&mut fields, column, |siblings, idx| {
                siblings.remove(idx);
                Ok(())
            })?;
        }
        CanonicalAlter::Merge { src, dst } => {
            edit(&mut fields.clone(), dst, |_, _| Ok(()))?;
            edit(&mut fields, src, |siblings, idx| {
                siblings.remove(idx);
                Ok(())
            })?;
        }
        CanonicalAlter::Promote { column, to } => {
            edit(&mut fields, column, |siblings, idx| {
                let mut promoted = siblings[idx].as_ref().clone();
                promoted.field_type = Box::new(iceberg_primitive(to)?);
                siblings[idx] = Arc::new(promoted);
                Ok(())
            })?;
        }
    }
    Schema::builder()
        .with_schema_id(current.schema_id() + 1)
        .with_fields(fields)
        .build()
        .map_err(|err| SchemaAlterError::Iceberg(err.to_string()))
}

/// Walk `column`'s output path, check the leaf's id, and edit its siblings.
/// Enclosing structs are rebuilt with every other attribute kept.
fn edit(
    fields: &mut Fields,
    column: &AlterColumn,
    f: impl FnOnce(&mut Fields, usize) -> Result<(), SchemaAlterError>,
) -> Result<(), SchemaAlterError> {
    let path: Vec<&str> = column
        .name
        .parent()
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(column.name.leaf()))
        .collect();
    descend(fields, &path, column, f)
}

fn descend(
    fields: &mut Fields,
    path: &[&str],
    column: &AlterColumn,
    f: impl FnOnce(&mut Fields, usize) -> Result<(), SchemaAlterError>,
) -> Result<(), SchemaAlterError> {
    let (segment, rest) = path.split_first().ok_or(SchemaAlterError::EmptyName)?;
    let idx = fields
        .iter()
        .position(|field| field.name == *segment)
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(column.name.to_string()))?;
    if rest.is_empty() {
        if fields[idx].id != column.id {
            return Err(SchemaAlterError::IdNameMismatch {
                id: column.id,
                name: column.name.to_string(),
            });
        }
        return f(fields, idx);
    }
    let mut children: Fields = match fields[idx].field_type.as_ref() {
        Type::Struct(st) => st.fields().to_vec(),
        Type::List(_) | Type::Map(_) => return Err(SchemaAlterError::NestedCollection),
        Type::Primitive(_) => return Err(SchemaAlterError::NotStruct(segment.to_string())),
    };
    descend(&mut children, rest, column, f)?;
    let mut parent = fields[idx].as_ref().clone();
    parent.field_type = Box::new(Type::Struct(StructType::new(children)));
    fields[idx] = Arc::new(parent);
    Ok(())
}

pub fn iceberg_primitive(value: &SkipprDataType) -> Result<Type, SchemaAlterError> {
    let primitive = match value {
        SkipprDataType::Boolean => PrimitiveType::Boolean,
        SkipprDataType::Byte | SkipprDataType::Short | SkipprDataType::Integer => {
            PrimitiveType::Int
        }
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
        SkipprDataType::Binary | SkipprDataType::Fixed => PrimitiveType::Binary,
        SkipprDataType::Uuid => PrimitiveType::Uuid,
        SkipprDataType::String | SkipprDataType::Json => PrimitiveType::String,
        SkipprDataType::Array
        | SkipprDataType::Map
        | SkipprDataType::Record
        | SkipprDataType::Unknown
        | SkipprDataType::Null => {
            return Err(SchemaAlterError::Iceberg(format!(
                "Iceberg has no primitive for '{}'",
                value.as_str()
            )))
        }
    };
    Ok(Type::Primitive(primitive))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::schema_alter::{FlattenMode, OutputName};

    #[test]
    fn iceberg_primitive_matches_publish_uuid_and_binary() {
        assert_eq!(
            iceberg_primitive(&SkipprDataType::Uuid).unwrap(),
            Type::Primitive(PrimitiveType::Uuid)
        );
        assert_eq!(
            iceberg_primitive(&SkipprDataType::Binary).unwrap(),
            Type::Primitive(PrimitiveType::Binary)
        );
        assert_eq!(
            iceberg_primitive(&SkipprDataType::Fixed).unwrap(),
            Type::Primitive(PrimitiveType::Binary)
        );
        assert!(iceberg_primitive(&SkipprDataType::Unknown).is_err());
        assert!(iceberg_primitive(&SkipprDataType::Record).is_err());
    }

    fn prim(id: i32, name: &str, ty: PrimitiveType) -> Arc<NestedField> {
        Arc::new(NestedField::optional(id, name, Type::Primitive(ty)))
    }

    fn schema(fields: Fields) -> Schema {
        Schema::builder()
            .with_schema_id(0)
            .with_fields(fields)
            .build()
            .unwrap()
    }

    fn trips() -> Schema {
        schema(vec![
            prim(7, "price", PrimitiveType::String),
            prim(9, "price_string", PrimitiveType::String),
            prim(3, "n", PrimitiveType::Int),
            prim(11, "event_day", PrimitiveType::Date),
            Arc::new(NestedField::optional(
                1,
                "detail",
                Type::Struct(StructType::new(vec![Arc::new(
                    NestedField::optional(4, "region", Type::Primitive(PrimitiveType::String))
                        .with_doc("kept"),
                )])),
            )),
        ])
    }

    fn column(sql: &str, id: i32, ty: SkipprDataType) -> AlterColumn {
        AlterColumn {
            name: OutputName::parse_sql(FlattenMode::Nested, sql).unwrap(),
            id,
            ty,
        }
    }

    #[test]
    fn rename_nested_leaf_keeps_id_and_doc() {
        let next = iceberg_schema_after_alter(
            &trips(),
            &CanonicalAlter::Rename {
                column: column("detail.region", 4, SkipprDataType::String),
                to_leaf: "region_code".into(),
            },
        )
        .unwrap();
        let field = next.field_by_name("detail.region_code").unwrap();
        assert_eq!(field.id, 4);
        assert_eq!(field.doc.as_deref(), Some("kept"));
        assert!(next.field_by_name("detail.region").is_none());
    }

    #[test]
    fn drop_and_merge_remove_by_name_and_id() {
        let dropped = iceberg_schema_after_alter(
            &trips(),
            &CanonicalAlter::Drop {
                column: column("price_string", 9, SkipprDataType::String),
            },
        )
        .unwrap();
        assert!(dropped.field_by_name("price_string").is_none());
        let merged = iceberg_schema_after_alter(
            &trips(),
            &CanonicalAlter::Merge {
                src: column("price_string", 9, SkipprDataType::String),
                dst: column("price", 7, SkipprDataType::String),
            },
        )
        .unwrap();
        assert!(merged.field_by_name("price_string").is_none());
        assert_eq!(merged.field_by_name("price").unwrap().id, 7);
    }

    #[test]
    fn promote_keeps_id() {
        for (sql, id, from, to, want) in [
            (
                "n",
                3,
                SkipprDataType::Integer,
                SkipprDataType::Long,
                PrimitiveType::Long,
            ),
            (
                "event_day",
                11,
                SkipprDataType::Date,
                SkipprDataType::Timestamp,
                PrimitiveType::Timestamp,
            ),
        ] {
            let next = iceberg_schema_after_alter(
                &trips(),
                &CanonicalAlter::Promote {
                    column: column(sql, id, from),
                    to,
                },
            )
            .unwrap();
            let field = next.field_by_name(sql).unwrap();
            assert_eq!(field.id, id);
            assert_eq!(field.field_type.as_ref(), &Type::Primitive(want));
        }
    }

    #[test]
    fn id_and_name_must_agree() {
        assert_eq!(
            iceberg_schema_after_alter(
                &trips(),
                &CanonicalAlter::Drop {
                    column: column("price", 3, SkipprDataType::String),
                },
            ),
            Err(SchemaAlterError::IdNameMismatch {
                id: 3,
                name: "price".into()
            })
        );
        assert_eq!(
            iceberg_schema_after_alter(
                &trips(),
                &CanonicalAlter::Drop {
                    column: column("missing", 9, SkipprDataType::String),
                },
            ),
            Err(SchemaAlterError::ColumnNotFound("missing".into()))
        );
        assert_eq!(
            iceberg_schema_after_alter(
                &trips(),
                &CanonicalAlter::Rename {
                    column: column("price_string", 9, SkipprDataType::String),
                    to_leaf: "price".into(),
                },
            ),
            Err(SchemaAlterError::ColumnExists("price".into()))
        );
    }

    #[test]
    fn flat_names_are_top_level_columns() {
        let flat = schema(vec![prim(4, "detail_truck_reg", PrimitiveType::String)]);
        let next = iceberg_schema_after_alter(
            &flat,
            &CanonicalAlter::Rename {
                column: AlterColumn {
                    name: OutputName::parse_sql(FlattenMode::Flat, "detail_truck_reg").unwrap(),
                    id: 4,
                    ty: SkipprDataType::String,
                },
                to_leaf: "truck_registration".into(),
            },
        )
        .unwrap();
        assert_eq!(next.field_by_name("truck_registration").unwrap().id, 4);
    }
}
