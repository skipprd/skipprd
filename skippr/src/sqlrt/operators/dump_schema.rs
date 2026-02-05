use crate::converters::skippr_hive::SkipprHive;
use crate::discover::{Metadata, OutputMetadata};
use crate::sqlrt::parser::SchemaDumpStatement;
use datafusion::sql::sqlparser::ast::ObjectName;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};

pub fn dump_schema(
    schema_name: ObjectName,
    metadata: &Metadata,
    stmt: &SchemaDumpStatement,
) -> Result<(), String> {
    let metadata_file = format!("{}", stmt.target);

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&metadata_file)
        .expect(&format!(
            "Failed to open target schema file {}",
            &metadata_file
        ));

    let mut writer = BufWriter::new(file);
    let output_metadata = OutputMetadata::from_metadata(metadata);

    let hive_schema = SkipprHive::convert_skippr_to_hive(&output_metadata).unwrap();
    let mut schema_str = format!("CREATE TABLE `{}` (\n", schema_name);

    for (i, col) in hive_schema.iter().enumerate() {
        if let Some(col_type) = col.r#type() {
            schema_str.push_str(&format_column(col.name(), col_type, 1));
            if i < hive_schema.len() - 1 {
                schema_str.push_str(",");
            }
            schema_str.push_str("\n");
        }
    }

    schema_str.push_str(");\n");

    writer
        .write_all(schema_str.as_bytes())
        .expect("Failed to write schema to file");

    Ok(())
}

fn format_column(name: &str, col_type: &str, indent: usize) -> String {
    let indentation = "  ".repeat(indent);

    if col_type.starts_with("struct<") {
        let inner = col_type;
        // .trim_start_matches("struct<")
        // .trim_end_matches('>');

        let fields: Vec<String> = inner
            .split(',')
            .filter_map(|field| field.split_once(':'))
            .map(|(field_name, field_type)| {
                format_column(field_name.trim(), field_type.trim(), indent + 1)
            })
            .collect();

        format!(
            "{} `{}` struct<\n{}\n{}>",
            indentation,
            name,
            fields.join(",\n"),
            indentation
        )
    // } else if col_type.starts_with("array<") {
    //     let inner_type = &col_type[6..col_type.len() - 1]; // Extract inside of array<>
    //     format!("{} `{}` array<{}>", indentation, name, format_column("", inner_type, indent + 1).trim_start())
    } else {
        format!("{} `{}` {}", indentation, name, col_type)
    }
}
