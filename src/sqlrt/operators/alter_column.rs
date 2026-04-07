use crate::discover::{Metadata, SkipprDataType};
use crate::sqlrt::parser::AlterSchemaAlterColumnType;

pub fn alter_column_type(
    metadata: &mut Metadata,
    alteration: &AlterSchemaAlterColumnType,
) -> Result<Metadata, String> {
    let column_metadata = Metadata::get_nested_metadata_from_field_notation(
        metadata,
        &alteration.column_name.to_string(),
    )
    .ok_or_else(|| format!("Column '{}' not found", alteration.column_name))?;

    let skippr_new_type = SkipprDataType::from_string(&alteration.new_type.to_string())
        .ok_or_else(|| {
            format!(
                "No type equivalent for '{}' in Skippr types",
                alteration.new_type
            )
        })?;

    match skippr_new_type {
        SkipprDataType::Array => {
            let values_new_type = alteration
                .values_new_type
                .clone()
                .ok_or_else(|| "No values type provided for array type".to_string())?;

            let skippr_value_type = SkipprDataType::from_string(&values_new_type.to_string())
                .ok_or_else(|| {
                    format!(
                        "No type equivalent for '{}' in Skippr types",
                        values_new_type
                    )
                })?;

            column_metadata.determined_type = skippr_new_type.clone();
            column_metadata.determined_type_values = Some(skippr_value_type);
        }
        SkipprDataType::Record => {
            return Err(format!("Type '{}' not supported for ALTER COLUMN. Perhaps try altering a specific field or dropping the struct altogether.", &alteration.new_type.to_string()));
        }
        _ => {
            column_metadata.determined_type = skippr_new_type;
        }
    }

    Ok(column_metadata.clone())
}
