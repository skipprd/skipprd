use crate::discover::{Metadata, SkipprTypes};
use crate::sql::parser::{AlterSchemaAlterColumnType, AlterSchemaDropColumn};

pub fn alter_column_drop(metadata: &mut Metadata, stmt: &AlterSchemaDropColumn) -> Result<Metadata, String> {

    let column_metadata = Metadata::get_nested_metadata_from_field_notation(metadata, &stmt.column_name.value)
        .ok_or_else(|| format!("Column '{}' not found", stmt.column_name))?;

    // remove the column from the metadata
    Metadata::remove_nested_metadata_from_dot_notation(metadata, stmt.column_name.value.as_str());

    Ok(metadata.clone())
}


