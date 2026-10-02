use crate::sqlrt::parser::SchemaDumpStatement;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::prelude::SessionContext;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};

pub async fn dump_schema(
    ctx: &SessionContext,
    pipeline: &str,
    namespace: &str,
    stmt: &SchemaDumpStatement,
) -> Result<(), String> {
    let metadata_file = format!("{}", stmt.target);
    let table = match ctx.table(&format!("{pipeline}.{namespace}")).await {
        Ok(table) => table,
        Err(_) => ctx.table(namespace).await.map_err(|err| {
            format!("SCHEMA DUMP: table '{pipeline}.{namespace}' is not registered: {err}")
        })?,
    };
    let schema: SchemaRef = table.schema().inner().clone();
    write_arrow_create_table(&metadata_file, namespace, &schema)
}

fn write_arrow_create_table(
    path: &str,
    table_name: &str,
    schema: &SchemaRef,
) -> Result<(), String> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|err| format!("Failed to open target schema file {path}: {err}"))?;
    let mut writer = BufWriter::new(file);
    let mut schema_str = format!("CREATE TABLE `{table_name}` (\n");
    for (i, field) in schema.fields().iter().enumerate() {
        schema_str.push_str(&format!(
            "  `{}` {}{}",
            field.name(),
            field.data_type(),
            if field.is_nullable() { "" } else { " NOT NULL" }
        ));
        if i + 1 < schema.fields().len() {
            schema_str.push(',');
        }
        schema_str.push('\n');
    }
    schema_str.push_str(");\n");
    writer
        .write_all(schema_str.as_bytes())
        .map_err(|err| format!("Failed to write schema to file: {err}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::datasource::MemTable;
    use datafusion::sql::sqlparser::ast::{Ident, ObjectName};
    use std::sync::Arc;

    #[tokio::test]
    async fn dump_schema_writes_arrow_field_types() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("n", DataType::Int64, false),
        ]));
        let ctx = SessionContext::new();
        let mem = MemTable::try_new(schema, vec![vec![]]).unwrap();
        ctx.register_table("rides", Arc::new(mem)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("schema.sql");
        let stmt = SchemaDumpStatement {
            pipeline: ObjectName::from(vec![Ident::new("p")]),
            schema: Some(ObjectName::from(vec![Ident::new("rides")])),
            target: target.to_string_lossy().into_owned(),
        };
        dump_schema(&ctx, "p", "rides", &stmt).await.unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("CREATE TABLE `rides`"));
        assert!(text.contains("`id` Utf8"));
        assert!(text.contains("`n` Int64 NOT NULL"));
        assert!(!text.contains("record<"));
        assert!(!text.contains("struct<"));
    }
}
