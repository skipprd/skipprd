use crate::discover::{Metadata, PipelineMetadata};
use crate::sql::parser::TableDropStatement;

/// Removes a table from the metadata
/// 
/// This function takes a mutable reference to the pipeline metadata and a TableDropStatement,
/// and removes the table from the metadata.
pub fn drop_table(pipeline_metadata: &mut PipelineMetadata, stmt: &TableDropStatement) -> Result<(), String> {
    // Get the schema and table names
    let table_str = format!("{}", stmt.table);
    // let schema_str = match &stmt.schema {
    //     Some(schema) => format!("{}", schema),
    //     None => "".to_string(), // No schema specified
    // };
    // 
    // // Determine the key to use in the metadata map
    // let metadata_key = if schema_str.is_empty() {
    //     // If no schema specified, use just the table name
    //     table_str
    // } else {
    //     // If schema specified, use schema.table format
    //     format!("{}.{}", schema_str, table_str)
    // };

    // Remove the table from the metadata
    match pipeline_metadata.metadata.remove(&table_str) {
        Some(_) => Ok(()),
        None => Err(format!("Table '{}' not found", table_str)),
    }
} 