//! Typed Iceberg-catalog DDL. Ingest [`super::evolution::Evolution`] is a
//! persisted routing record, not an ALTER.

use crate::discover::evolution::Evolution;
use crate::discover::{Metadata, SkipprDataType};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SchemaAlterOp {
    Rename { from: String, to: String },
    Merge { src: String, dst: String },
    Drop { column: String },
    Promote { column: String, to: SkipprDataType },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SchemaAlterError {
    #[error("column '{0}' not found")]
    ColumnNotFound(String),
    #[error("column '{0}' already exists")]
    ColumnExists(String),
    #[error("cannot promote {from} to {to}; Iceberg-legal promotions are byte/short/integer→long, byte/short→integer, float→double, timestamp_milli→timestamp (Iceberg timestamp is already µs); widen to string with MERGE COLUMN")]
    IllegalPromote { from: String, to: String },
    #[error("{0}")]
    Failed(String),
}

pub fn is_iceberg_legal_promote(from: &SkipprDataType, to: &SkipprDataType) -> bool {
    use SkipprDataType::*;
    if from == to {
        return true;
    }
    matches!(
        (from, to),
        (Byte | Short | Integer, Long)
            | (Byte | Short, Integer)
            | (Float, Double)
            | (TimestampMilli, Timestamp)
    )
}

pub fn canonicalize_op(
    metadata: &Metadata,
    op: &SchemaAlterOp,
) -> Result<SchemaAlterOp, SchemaAlterError> {
    Ok(match op {
        SchemaAlterOp::Rename { from, to } => SchemaAlterOp::Rename {
            from: resolve_field_path(metadata, from)?,
            to: to.clone(),
        },
        SchemaAlterOp::Merge { src, dst } => SchemaAlterOp::Merge {
            src: resolve_field_path(metadata, src)?,
            dst: resolve_field_path(metadata, dst)?,
        },
        SchemaAlterOp::Drop { column } => SchemaAlterOp::Drop {
            column: resolve_field_path(metadata, column)?,
        },
        SchemaAlterOp::Promote { column, to } => SchemaAlterOp::Promote {
            column: resolve_field_path(metadata, column)?,
            to: to.clone(),
        },
    })
}

pub fn resolve_field_path(metadata: &Metadata, name: &str) -> Result<String, SchemaAlterError> {
    if exact_path_exists(metadata, name) {
        return Ok(name.to_string());
    }
    if !name.contains('.') {
        if let Some((key, _)) = metadata
            .fields
            .iter()
            .find(|(_, child)| child.out_field_name == name)
        {
            return Ok(key.clone());
        }
    }
    flatten_path_for(metadata, name)
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(name.to_string()))
}

fn exact_path_exists(metadata: &Metadata, name: &str) -> bool {
    if name.contains('.') {
        let mut current = metadata;
        for segment in name.split('.') {
            match current.fields.get(segment) {
                Some(child) => current = child,
                None => return false,
            }
        }
        return true;
    }
    metadata.fields.contains_key(name)
}

fn flatten_name_segment(name: &str) -> String {
    name.replace('-', "_")
}

fn flatten_path_for(metadata: &Metadata, want: &str) -> Option<String> {
    fn walk(node: &Metadata, prefix_flat: &str, prefix_dot: &str, want: &str) -> Option<String> {
        for (key, child) in node.fields.iter() {
            let segment = if child.out_field_name.is_empty() {
                key.as_str()
            } else {
                child.out_field_name.as_str()
            };
            let flat = if prefix_flat.is_empty() {
                flatten_name_segment(segment)
            } else {
                format!("{}_{}", prefix_flat, flatten_name_segment(segment))
            };
            let dotted = if prefix_dot.is_empty() {
                key.clone()
            } else {
                format!("{prefix_dot}.{key}")
            };
            if flat == want {
                return Some(dotted);
            }
            if let Some(hit) = walk(child, &flat, &dotted, want) {
                return Some(hit);
            }
        }
        None
    }
    walk(metadata, "", "", want)
}

pub fn field_id_for_op(metadata: &Metadata, op: &SchemaAlterOp) -> Result<i32, SchemaAlterError> {
    let name = match op {
        SchemaAlterOp::Rename { from, .. } => from.as_str(),
        SchemaAlterOp::Merge { src, .. } => src.as_str(),
        SchemaAlterOp::Drop { column } | SchemaAlterOp::Promote { column, .. } => column.as_str(),
    };
    let field = find_field(metadata, name)
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(name.to_string()))?;
    Ok(field.field_id)
}

pub fn apply(metadata: &mut Metadata, op: &SchemaAlterOp) -> Result<(), SchemaAlterError> {
    match op {
        SchemaAlterOp::Rename { from, to } => rename_column(metadata, from, to),
        SchemaAlterOp::Merge { src, dst } => merge_column(metadata, src, dst),
        SchemaAlterOp::Drop { column } => drop_column(metadata, column),
        SchemaAlterOp::Promote { column, to } => promote_column(metadata, column, to),
    }
}

fn drop_column(metadata: &mut Metadata, column: &str) -> Result<(), SchemaAlterError> {
    Metadata::get_nested_metadata_from_field_notation(metadata, column)
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(column.to_string()))?;
    Metadata::remove_nested_metadata_from_dot_notation(metadata, column);
    retarget_evolution_fields(metadata, column, None);
    Ok(())
}

fn promote_column(
    metadata: &mut Metadata,
    column: &str,
    to: &SkipprDataType,
) -> Result<(), SchemaAlterError> {
    let field = Metadata::get_nested_metadata_from_field_notation(metadata, column)
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(column.to_string()))?;
    if !is_iceberg_legal_promote(&field.determined_type, to) {
        return Err(SchemaAlterError::IllegalPromote {
            from: field.determined_type.as_str().to_string(),
            to: to.as_str().to_string(),
        });
    }
    field.determined_type = to.clone();
    Ok(())
}

fn rename_column(metadata: &mut Metadata, from: &str, to: &str) -> Result<(), SchemaAlterError> {
    if from == to {
        return Ok(());
    }
    if to.contains('.') {
        return Err(SchemaAlterError::Failed(
            "RENAME COLUMN target must be a single field name".to_string(),
        ));
    }
    if field_exists(metadata, to) {
        return Err(SchemaAlterError::ColumnExists(to.to_string()));
    }
    let (parent, leaf) = parent_and_leaf(metadata, from)?;
    let mut field = parent
        .fields
        .remove(&leaf)
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(from.to_string()))?;
    field.out_field_name = last_segment(to).to_string();
    parent.fields.insert(last_segment(to).to_string(), field);
    retarget_evolution_fields(metadata, from, Some(to));
    Ok(())
}

fn merge_column(metadata: &mut Metadata, src: &str, dst: &str) -> Result<(), SchemaAlterError> {
    if src == dst {
        return Err(SchemaAlterError::Failed(
            "MERGE COLUMN source and destination must differ".to_string(),
        ));
    }
    let src_type = {
        let src_field = Metadata::get_nested_metadata_from_field_notation(metadata, src)
            .ok_or_else(|| SchemaAlterError::ColumnNotFound(src.to_string()))?;
        src_field.determined_type.clone()
    };
    Metadata::get_nested_metadata_from_field_notation(metadata, dst)
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(dst.to_string()))?;

    retarget_evolution_fields(metadata, src, Some(dst));
    {
        let dst_field = Metadata::get_nested_metadata_from_field_notation(metadata, dst)
            .ok_or_else(|| SchemaAlterError::ColumnNotFound(dst.to_string()))?;
        let routes_src_type = dst_field
            .evolution
            .values()
            .any(|evo| evo.type_string == src_type && evo.new_field == last_segment(dst));
        if !routes_src_type {
            dst_field.evolution.insert(
                src.to_string(),
                Evolution {
                    type_string: src_type,
                    new_field: last_segment(dst).to_string(),
                    sovled: true,
                },
            );
        }
    }

    let (parent, leaf) = parent_and_leaf(metadata, src)?;
    parent
        .fields
        .remove(&leaf)
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(src.to_string()))?;
    Ok(())
}

fn field_exists(metadata: &Metadata, name: &str) -> bool {
    find_field(metadata, name).is_some()
}

fn find_field<'a>(metadata: &'a Metadata, name: &str) -> Option<&'a Metadata> {
    if metadata.out_field_name == name || metadata.out_field_name == last_segment(name) {
        return Some(metadata);
    }
    if let Some(child) = metadata.fields.get(name) {
        return Some(child);
    }
    if name.contains('.') {
        let mut current = metadata;
        for segment in name.split('.') {
            current = current.fields.get(segment)?;
        }
        return Some(current);
    }
    metadata
        .fields
        .values()
        .find(|child| child.out_field_name == name)
}

fn parent_and_leaf<'a>(
    metadata: &'a mut Metadata,
    path: &str,
) -> Result<(&'a mut Metadata, String), SchemaAlterError> {
    let segments: Vec<&str> = path.split('.').collect();
    let leaf = segments
        .last()
        .copied()
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(path.to_string()))?
        .to_string();
    if segments.len() == 1 {
        if metadata.fields.contains_key(&leaf) {
            return Ok((metadata, leaf));
        }
        let key = metadata
            .fields
            .iter()
            .find(|(_, child)| child.out_field_name == leaf)
            .map(|(k, _)| k.clone())
            .ok_or_else(|| SchemaAlterError::ColumnNotFound(path.to_string()))?;
        return Ok((metadata, key));
    }
    let parent_path = segments[..segments.len() - 1].join(".");
    let parent = Metadata::get_nested_metadata_from_field_notation(metadata, &parent_path)
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(path.to_string()))?;
    if parent.fields.contains_key(&leaf) {
        return Ok((parent, leaf));
    }
    let key = parent
        .fields
        .iter()
        .find(|(_, child)| child.out_field_name == leaf)
        .map(|(k, _)| k.clone())
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(path.to_string()))?;
    Ok((parent, key))
}

fn last_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn retarget_evolution_fields(metadata: &mut Metadata, from: &str, to: Option<&str>) {
    let from_leaf = last_segment(from);
    let to_leaf = to.map(last_segment);
    walk_retarget(metadata, from, from_leaf, to, to_leaf);
}

fn walk_retarget(
    metadata: &mut Metadata,
    from: &str,
    from_leaf: &str,
    to: Option<&str>,
    to_leaf: Option<&str>,
) {
    let keys: Vec<String> = metadata.evolution.keys().cloned().collect();
    for key in keys {
        if let Some(evo) = metadata.evolution.get_mut(&key) {
            if evo.new_field == from || evo.new_field == from_leaf {
                match (to, to_leaf) {
                    (Some(to), Some(to_leaf)) => {
                        evo.new_field = if evo.new_field == from {
                            to.to_string()
                        } else {
                            to_leaf.to_string()
                        };
                    }
                    _ => {
                        metadata.evolution.remove(&key);
                    }
                }
            }
        }
    }
    let children: Vec<String> = metadata.fields.keys().cloned().collect();
    for child in children {
        if let Some(child_meta) = metadata.fields.get_mut(&child) {
            walk_retarget(child_meta, from, from_leaf, to, to_leaf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, ty: SkipprDataType, field_id: i32) -> Metadata {
        let mut m = Metadata::new_with_type(ty, name);
        m.field_id = field_id;
        m
    }

    fn ns(fields: Vec<Metadata>) -> Metadata {
        let mut root = Metadata::new().unwrap();
        for f in fields {
            root.set_field(&f.out_field_name.clone(), f);
        }
        root
    }

    #[test]
    fn rename_rejects_dotted_target() {
        let mut meta = ns(vec![field("price", SkipprDataType::Long, 7)]);
        let err = apply(
            &mut meta,
            &SchemaAlterOp::Rename {
                from: "price".into(),
                to: "amount.usd".into(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, SchemaAlterError::Failed(_)));
    }

    #[test]
    fn resolve_field_path_maps_flattened_iceberg_name() {
        let mut detail = field("detail", SkipprDataType::Record, 1);
        detail.fields.insert(
            "truck_reg".into(),
            field("truck_reg", SkipprDataType::String, 4),
        );
        let meta = ns(vec![detail, field("region", SkipprDataType::String, 21)]);
        assert_eq!(
            resolve_field_path(&meta, "detail_truck_reg").unwrap(),
            "detail.truck_reg"
        );
        assert_eq!(resolve_field_path(&meta, "region").unwrap(), "region");
        assert_eq!(
            resolve_field_path(&meta, "detail.truck_reg").unwrap(),
            "detail.truck_reg"
        );
        assert!(matches!(
            resolve_field_path(&meta, "nope"),
            Err(SchemaAlterError::ColumnNotFound(_))
        ));
    }

    #[test]
    fn canonicalize_op_rewrites_flatten_src() {
        let mut detail = field("detail", SkipprDataType::Record, 1);
        detail.fields.insert(
            "truck_reg".into(),
            field("truck_reg", SkipprDataType::String, 4),
        );
        let meta = ns(vec![detail]);
        let op = canonicalize_op(
            &meta,
            &SchemaAlterOp::Rename {
                from: "detail_truck_reg".into(),
                to: "truck_registration".into(),
            },
        )
        .unwrap();
        assert_eq!(
            op,
            SchemaAlterOp::Rename {
                from: "detail.truck_reg".into(),
                to: "truck_registration".into(),
            }
        );
    }

    #[test]
    fn field_id_for_op_reads_src_before_apply() {
        let meta = ns(vec![
            field("price", SkipprDataType::Long, 7),
            field("price_string", SkipprDataType::String, 9),
        ]);
        assert_eq!(
            field_id_for_op(
                &meta,
                &SchemaAlterOp::Merge {
                    src: "price_string".into(),
                    dst: "price".into(),
                },
            )
            .unwrap(),
            9
        );
    }

    #[test]
    fn rename_keeps_field_id() {
        let mut meta = ns(vec![field("price", SkipprDataType::Long, 7)]);
        apply(
            &mut meta,
            &SchemaAlterOp::Rename {
                from: "price".into(),
                to: "amount".into(),
            },
        )
        .unwrap();
        assert!(meta.fields.get("price").is_none());
        let amount = meta.fields.get("amount").unwrap();
        assert_eq!(amount.out_field_name, "amount");
        assert_eq!(amount.field_id, 7);
        assert_eq!(amount.determined_type, SkipprDataType::Long);
    }

    #[test]
    fn drop_removes_column() {
        let mut meta = ns(vec![
            field("price", SkipprDataType::Long, 7),
            field("note", SkipprDataType::String, 8),
        ]);
        apply(
            &mut meta,
            &SchemaAlterOp::Drop {
                column: "note".into(),
            },
        )
        .unwrap();
        assert!(meta.fields.get("note").is_none());
        assert!(meta.fields.get("price").is_some());
    }

    #[test]
    fn promote_integer_to_long() {
        let mut meta = ns(vec![field("n", SkipprDataType::Integer, 1)]);
        apply(
            &mut meta,
            &SchemaAlterOp::Promote {
                column: "n".into(),
                to: SkipprDataType::Long,
            },
        )
        .unwrap();
        assert_eq!(
            meta.fields.get("n").unwrap().determined_type,
            SkipprDataType::Long
        );
    }

    #[test]
    fn promote_integer_to_string_is_illegal() {
        let mut meta = ns(vec![field("n", SkipprDataType::Integer, 1)]);
        let err = apply(
            &mut meta,
            &SchemaAlterOp::Promote {
                column: "n".into(),
                to: SkipprDataType::String,
            },
        )
        .unwrap_err();
        assert!(matches!(err, SchemaAlterError::IllegalPromote { .. }));
    }

    #[test]
    fn merge_drops_src_and_retargets_evolution() {
        let mut price = field("price", SkipprDataType::Long, 7);
        price.evolution.insert(
            "price_string".into(),
            Evolution {
                type_string: SkipprDataType::String,
                new_field: "price_string".into(),
                sovled: true,
            },
        );
        let mut meta = ns(vec![
            price,
            field("price_string", SkipprDataType::String, 9),
        ]);
        apply(
            &mut meta,
            &SchemaAlterOp::Merge {
                src: "price_string".into(),
                dst: "price".into(),
            },
        )
        .unwrap();
        assert!(meta.fields.get("price_string").is_none());
        let price = meta.fields.get("price").unwrap();
        assert_eq!(price.field_id, 7);
        let evo = price
            .evolution
            .values()
            .find(|evo| evo.type_string == SkipprDataType::String)
            .expect("MERGE keeps src incoming type and points new_field at dst");
        assert_eq!(evo.new_field, "price");
    }

    #[test]
    fn merge_without_prior_evolution_routes_src_type_to_dst() {
        let mut meta = ns(vec![
            field("price", SkipprDataType::Long, 7),
            field("price_string", SkipprDataType::String, 9),
        ]);
        apply(
            &mut meta,
            &SchemaAlterOp::Merge {
                src: "price_string".into(),
                dst: "price".into(),
            },
        )
        .unwrap();
        let price = meta.fields.get("price").unwrap();
        let evo = price
            .evolution
            .values()
            .find(|evo| evo.type_string == SkipprDataType::String)
            .expect("MERGE inserts src-type routing when none existed");
        assert_eq!(evo.new_field, "price");
    }
}
