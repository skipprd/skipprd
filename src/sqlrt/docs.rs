use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Documentation for a SQL statement, including its syntax and description
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqlStatementDoc {
    /// The name of the SQL statement
    pub name: String,
    /// The syntax of the SQL statement
    pub syntax: String,
    /// A description of what the SQL statement does
    pub description: String,
    /// Example usage of the SQL statement
    pub example: String,
}

/// Returns documentation for all SQL statements supported by the application.
/// BTreeMap keeps SHOW DOCS / sql-help / sql-docs.md order stable.
pub fn get_sql_docs() -> BTreeMap<String, SqlStatementDoc> {
    let mut docs = BTreeMap::new();

    // Schema Dump
    docs.insert(
        "SCHEMA DUMP".to_string(),
        SqlStatementDoc {
            name: "SCHEMA DUMP".to_string(),
            syntax: "SCHEMA DUMP <pipeline_name>[.<schema_name>] TO '<destination_path>'".to_string(),
            description: "Exports the schema definition of a pipeline or a specific schema within a pipeline to a file.".to_string(),
            example: "SCHEMA DUMP bike_hire TO 'bike_hire_schema.json'".to_string(),
        },
    );

    // Database Drop
    docs.insert(
        "DROP DATABASE".to_string(),
        SqlStatementDoc {
            name: "DROP DATABASE".to_string(),
            syntax: "DROP DATABASE <database_name>".to_string(),
            description: "Drops an Iceberg namespace from the catalog.".to_string(),
            example: "DROP DATABASE data_warehouse".to_string(),
        },
    );

    // Schema Drop
    docs.insert(
        "DROP SCHEMA".to_string(),
        SqlStatementDoc {
            name: "DROP SCHEMA".to_string(),
            syntax: "DROP SCHEMA <pipeline_name>[.<schema_name>]".to_string(),
            description: "Drops a schema from a pipeline. On the next sync, the schema will be re-discovered.".to_string(),
            example: "DROP SCHEMA bike_hire".to_string(),
        },
    );

    // Pipeline Drop
    docs.insert(
        "DROP PIPELINE".to_string(),
        SqlStatementDoc {
            name: "DROP PIPELINE".to_string(),
            syntax: "DROP PIPELINE <pipeline_name>".to_string(),
            description: "Drops all schemas and data for a pipeline. In sync mode, this immediately removes the pipeline data directory. In query mode, the pipeline will be dropped on the next sync run.".to_string(),
            example: "DROP PIPELINE analytics".to_string(),
        },
    );

    // Pipeline Reset
    docs.insert(
        "RESET PIPELINE".to_string(),
        SqlStatementDoc {
            name: "RESET PIPELINE".to_string(),
            syntax: "RESET PIPELINE <pipeline_name>".to_string(),
            description: "Resets the offset database and purges WAL files for a pipeline. In sync mode, this immediately removes the pipeline data directory. In query mode, the pipeline will be reset on the next sync run.".to_string(),
            example: "RESET PIPELINE analytics".to_string(),
        },
    );

    // Schema Load
    docs.insert(
        "SCHEMA LOAD".to_string(),
        SqlStatementDoc {
            name: "SCHEMA LOAD".to_string(),
            syntax: "LOAD SCHEMA '<source_path>' INTO <pipeline_name>".to_string(),
            description: "Loads a schema definition from a JSON file into a pipeline. Column `type` maps VARCHAR/STRING/TEXT→String, NUMBER/INT/INTEGER/BIGINT→Long, DOUBLE/FLOAT/REAL/NUMERIC/DECIMAL→Double, BOOLEAN/BOOL→Boolean, DATE→Date, TIMESTAMP/DATETIME/TIMESTAMP_NTZ→Timestamp, VARIANT/OBJECT/ARRAY→String.".to_string(),
            example: "LOAD SCHEMA 'bike_hire_schema.json' INTO bike_hire".to_string(),
        },
    );

    // Pipeline Toggle
    docs.insert(
        "ENABLE PIPELINE".to_string(),
        SqlStatementDoc {
            name: "ENABLE PIPELINE".to_string(),
            syntax: "ENABLE PIPELINE <pipeline_name>".to_string(),
            description: "Enables a pipeline for processing.".to_string(),
            example: "ENABLE PIPELINE analytics".to_string(),
        },
    );

    // Pipeline Toggle (Disable)
    docs.insert(
        "DISABLE PIPELINE".to_string(),
        SqlStatementDoc {
            name: "DISABLE PIPELINE".to_string(),
            syntax: "DISABLE PIPELINE <pipeline_name>".to_string(),
            description: "Disables a pipeline, preventing it from processing data.".to_string(),
            example: "DISABLE PIPELINE analytics".to_string(),
        },
    );

    docs.insert(
        "ALTER TABLE DROP COLUMN".to_string(),
        SqlStatementDoc {
            name: "ALTER TABLE DROP COLUMN".to_string(),
            syntax: "ALTER TABLE <pipeline>[.<namespace>] DROP COLUMN <column>".to_string(),
            description: "Drops a column from skippr metadata and the Iceberg table (same field id). Pipeline must be DISABLED.".to_string(),
            example: "ALTER TABLE bikehire.trips DROP COLUMN user_id".to_string(),
        },
    );
    docs.insert(
        "ALTER TABLE ALTER COLUMN".to_string(),
        SqlStatementDoc {
            name: "ALTER TABLE ALTER COLUMN".to_string(),
            syntax: "ALTER TABLE <pipeline>[.<namespace>] ALTER COLUMN <column> TYPE <new_type>".to_string(),
            description: "Iceberg-legal promotion only (byte/short/integer→long, byte/short→integer, float→double, timestamp_milli→timestamp; Iceberg timestamp is already µs). Nested DROP/MERGE without a top-level Iceberg field id fails closed. Widen to string with MERGE COLUMN. Pipeline must be DISABLED.".to_string(),
            example: "ALTER TABLE bikehire.trips ALTER COLUMN n TYPE BIGINT".to_string(),
        },
    );
    docs.insert(
        "ALTER TABLE RENAME COLUMN".to_string(),
        SqlStatementDoc {
            name: "ALTER TABLE RENAME COLUMN".to_string(),
            syntax: "ALTER TABLE <pipeline>[.<namespace>] RENAME COLUMN <from> TO <to>".to_string(),
            description: "Renames a column. The Iceberg field id is unchanged so existing files stay readable. Pipeline must be DISABLED.".to_string(),
            example: "ALTER TABLE bikehire.trips RENAME COLUMN price TO amount".to_string(),
        },
    );
    docs.insert(
        "ALTER TABLE MERGE COLUMN".to_string(),
        SqlStatementDoc {
            name: "ALTER TABLE MERGE COLUMN".to_string(),
            syntax: "ALTER TABLE <pipeline>[.<namespace>] MERGE COLUMN <src> INTO <dst>".to_string(),
            description: "Drops src from metadata and Iceberg (same field id path as DROP). Retargets type-conflict evolution so later values write to dst. Historical src-only rows are not copied. Pipeline must be DISABLED.".to_string(),
            example: "ALTER TABLE bikehire.trips MERGE COLUMN price_string INTO price".to_string(),
        },
    );

    // Drop Table
    docs.insert(
        "DROP TABLE".to_string(),
        SqlStatementDoc {
            name: "DROP TABLE".to_string(),
            syntax: "DROP TABLE [<schema_name>.]<table_name>".to_string(),
            description: "Drops an Iceberg table from the catalog and local metadata.".to_string(),
            example: "DROP TABLE analytics.user_events".to_string(),
        },
    );

    // Deadletters Table (Querying)
    docs.insert(
        "DEADLETTERS TABLE".to_string(),
        SqlStatementDoc {
            name: "DEADLETTERS TABLE".to_string(),
            syntax: "SELECT <columns> FROM _dl_<pipeline_name> [WHERE namespace = '<ns>'] [ORDER BY processed_time DESC]".to_string(),
            description: "Query deadletters from the configured deadletter destination. The table name is `_dl_<pipeline_name>`.".to_string(),
            example: "SELECT id, namespace, error FROM _dl_bike_hire WHERE namespace = 'rides' ORDER BY processed_time DESC LIMIT 50".to_string(),
        },
    );

    // Standard SQL queries
    docs.insert(
        "SELECT".to_string(),
        SqlStatementDoc {
            name: "SELECT".to_string(),
            syntax: "SELECT <columns> FROM <table_name> [WHERE <condition>] [GROUP BY <expressions>] [HAVING <condition>] [ORDER BY <expressions>] [LIMIT <count>]".to_string(),
            description: "Executes a standard SQL query against Iceberg tables and the live WAL.".to_string(),
            example: "SELECT user_id, COUNT(*) FROM bike_hire WHERE date > '2023-01-01' GROUP BY user_id LIMIT 10".to_string(),
        },
    );

    // STREAM queries
    docs.insert(
        "STREAM".to_string(),
        SqlStatementDoc {
            name: "STREAM".to_string(),
            syntax: "STREAM <columns> FROM <table_name> [WHERE <condition>] [ORDER BY <expressions>] [LIMIT <count>]".to_string(),
            description: "Executes a streaming SQL query against the data currently ingesting into the WAL, continuously returning new results as data arrives.".to_string(),
            example: "STREAM user_id, event_type FROM user_events WHERE event_time > CURRENT_TIMESTAMP - INTERVAL '1' HOUR ORDER BY event_time LIMIT 100".to_string(),
        },
    );

    // Date functions
    docs.insert(
        "DATEDIFF".to_string(),
        SqlStatementDoc {
            name: "DATEDIFF".to_string(),
            syntax: "DATEDIFF(<start_date>, <end_date>)".to_string(),
            description: "Calculates the difference in days between two dates. Accepts RFC3339 formatted date strings.".to_string(),
            example: "SELECT id, DATEDIFF(start_date, end_date) AS duration FROM bike_hire".to_string(),
        },
    );

    docs.insert(
        "SHOW DOCS".to_string(),
        SqlStatementDoc {
            name: "SHOW DOCS".to_string(),
            syntax: "SHOW DOCS".to_string(),
            description: "Displays the documentation for all supported SQL statements.".to_string(),
            example: "SHOW DOCS".to_string(),
        },
    );

    docs.insert(
        "SHOW STATS".to_string(),
        SqlStatementDoc {
            name: "SHOW STATS".to_string(),
            syntax: "SHOW STATS FOR <pipeline>[.<namespace>]".to_string(),
            description:
                "Show per-field statistics JSON for a pipeline (optionally filtered by namespace)."
                    .to_string(),
            example: "SHOW STATS FOR bike_hire.ride_start".to_string(),
        },
    );

    docs.insert(
        "SHOW SEMANTIC".to_string(),
        SqlStatementDoc {
            name: "SHOW SEMANTIC".to_string(),
            syntax: "SHOW SEMANTIC FOR <pipeline>[.<namespace>]".to_string(),
            description: "Show semantic roles for <pipeline>[.<namespace>]. Falls back to object storage if local cache is missing.".to_string(),
            example: "SHOW SEMANTIC FOR bike_hire.ride_start".to_string(),
        },
    );

    docs.insert(
        "SHOW CATALOG".to_string(),
        SqlStatementDoc {
            name: "SHOW CATALOG".to_string(),
            syntax: "SHOW CATALOG FOR <pipeline>[.<namespace>]".to_string(),
            description: "Show catalog fields for <pipeline>[.<namespace>]. Falls back to object storage if local cache is missing.".to_string(),
            example: "SHOW CATALOG FOR bike_hire.ride_start".to_string(),
        },
    );

    docs.insert(
        "SHOW PIPELINE".to_string(),
        SqlStatementDoc {
            name: "SHOW PIPELINE".to_string(),
            syntax: "SHOW PIPELINE <pipeline_name>".to_string(),
            description: "Show pipeline status as JSON: namespaces (name, enabled, fields), offsets, and metadata_location (S3 URI, or a local path when SKIPPRD_EL_STORAGE_MODE=local).".to_string(),
            example: "SHOW PIPELINE el_mssql".to_string(),
        },
    );

    docs
}

/// One category in SHOW DOCS / sql-help / generated markdown/html/json.
pub struct SqlDocCategory {
    pub title: &'static str,
    pub rule: &'static str,
    pub statements: Vec<SqlStatementDoc>,
}

/// One grouping for every SQL-doc surface. Iteration order is the BTreeMap key order.
pub fn categorized_sql_docs() -> Vec<SqlDocCategory> {
    let mut schema = Vec::new();
    let mut pipeline = Vec::new();
    let mut data = Vec::new();
    let mut query = Vec::new();
    for doc in get_sql_docs().into_values() {
        if doc.name.contains("SCHEMA") || doc.name.starts_with("ALTER TABLE") {
            schema.push(doc);
        } else if doc.name.contains("PIPELINE") {
            pipeline.push(doc);
        } else if doc.name.contains("TABLE") || doc.name.contains("DATABASE") {
            data.push(doc);
        } else {
            query.push(doc);
        }
    }
    vec![
        SqlDocCategory {
            title: "Schema Operations",
            rule: "-----------------",
            statements: schema,
        },
        SqlDocCategory {
            title: "Pipeline Operations",
            rule: "-------------------",
            statements: pipeline,
        },
        SqlDocCategory {
            title: "Data Operations",
            rule: "---------------",
            statements: data,
        },
        SqlDocCategory {
            title: "Query Operations",
            rule: "----------------",
            statements: query,
        },
    ]
}

/// How `print_categorized_sql_docs` presents each statement.
#[derive(Clone, Copy)]
pub enum SqlDocListingKind {
    /// sql-help list: name and description.
    Brief,
    /// SHOW DOCS: name, description, syntax, example.
    Full,
}

/// One printer for sql-help list and SHOW DOCS.
pub fn print_categorized_sql_docs(kind: SqlDocListingKind) {
    for category in categorized_sql_docs() {
        if category.statements.is_empty() {
            continue;
        }
        println!("{}:", category.title);
        println!("{}", category.rule);
        for doc in &category.statements {
            println!("  {} - {}", doc.name, doc.description);
            if matches!(kind, SqlDocListingKind::Full) {
                println!("  Syntax: {}", doc.syntax);
                println!("  Example: {}\n", doc.example);
            }
        }
        println!();
    }
}

/// Returns a formatted string with documentation for all SQL statements
pub fn get_sql_docs_formatted() -> String {
    let mut result = String::new();
    result.push_str("# Skippr SQL Documentation\n\n");
    result.push_str("This document describes all SQL statements supported by Skippr.\n\n");
    for category in categorized_sql_docs() {
        result.push_str(&format!("## {}\n\n", category.title));
        for doc in &category.statements {
            add_doc_to_result(&mut result, doc);
        }
    }
    result
}

fn add_doc_to_result(result: &mut String, doc: &SqlStatementDoc) {
    result.push_str(&format!("### {}\n\n", doc.name));
    result.push_str(&format!("**Syntax:**\n```sql\n{}\n```\n\n", doc.syntax));
    result.push_str(&format!("**Description:**\n{}\n\n", doc.description));
    result.push_str(&format!("**Example:**\n```sql\n{}\n```\n\n", doc.example));
}

/// Format for documentation output
pub enum DocFormat {
    Markdown,
    Html,
    Json,
}

/// Returns documentation in a specific format
pub fn get_docs_in_format(format: DocFormat) -> String {
    match format {
        DocFormat::Markdown => get_sql_docs_formatted(),
        DocFormat::Html => get_sql_docs_html(),
        DocFormat::Json => get_sql_docs_json(),
    }
}

/// Returns documentation in HTML format
pub fn get_sql_docs_html() -> String {
    let mut result = String::new();

    result.push_str("<!DOCTYPE html>\n<html>\n<head>\n");
    result.push_str("<title>Skippr SQL Documentation</title>\n");
    result.push_str("<style>\n");
    result.push_str("body { font-family: Arial, sans-serif; max-width: 1000px; margin: 0 auto; padding: 20px; }\n");
    result.push_str("h1 { color: #333; }\n");
    result.push_str("h2 { color: #0066cc; margin-top: 30px; }\n");
    result.push_str("h3 { margin-top: 25px; }\n");
    result.push_str(".syntax { background-color: #f5f5f5; padding: 10px; border-radius: 5px; font-family: monospace; }\n");
    result.push_str(".example { background-color: #f0f8ff; padding: 10px; border-radius: 5px; font-family: monospace; }\n");
    result.push_str("</style>\n");
    result.push_str("</head>\n<body>\n");

    result.push_str("<h1>Skippr SQL Documentation</h1>\n");
    result.push_str("<p>This document describes all SQL statements supported by Skippr.</p>\n");

    for category in categorized_sql_docs() {
        result.push_str(&format!("<h2>{}</h2>\n", category.title));
        for doc in &category.statements {
            add_doc_to_html(&mut result, doc);
        }
    }

    result.push_str("</body>\n</html>");

    result
}

/// Returns documentation in JSON format
pub fn get_sql_docs_json() -> String {
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Serialize, Deserialize)]
    struct DocCategory {
        name: String,
        statements: Vec<SqlStatementDoc>,
    }

    let categories: Vec<DocCategory> = categorized_sql_docs()
        .into_iter()
        .map(|category| DocCategory {
            name: category.title.to_string(),
            statements: category.statements,
        })
        .collect();

    let json_value = json!({
        "title": "Skippr SQL Documentation",
        "description": "This document describes all SQL statements supported by Skippr.",
        "categories": categories,
    });

    serde_json::to_string_pretty(&json_value).unwrap()
}

fn add_doc_to_html(result: &mut String, doc: &SqlStatementDoc) {
    result.push_str(&format!("<h3>{}</h3>\n", doc.name));
    result.push_str("<h4>Syntax:</h4>\n");
    result.push_str(&format!("<div class=\"syntax\">{}</div>\n", doc.syntax));
    result.push_str("<h4>Description:</h4>\n");
    result.push_str(&format!("<p>{}</p>\n", doc.description));
    result.push_str("<h4>Example:</h4>\n");
    result.push_str(&format!("<div class=\"example\">{}</div>\n", doc.example));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn get_sql_docs_covers_show_statements() {
        let docs = get_sql_docs();
        for name in [
            "SHOW STATS",
            "SHOW SEMANTIC",
            "SHOW CATALOG",
            "SHOW PIPELINE",
            "ALTER TABLE DROP COLUMN",
            "ALTER TABLE ALTER COLUMN",
            "ALTER TABLE RENAME COLUMN",
            "ALTER TABLE MERGE COLUMN",
        ] {
            assert!(
                docs.contains_key(name),
                "get_sql_docs() is the SQL-doc SoT and must include {name}"
            );
        }
    }

    #[test]
    fn select_and_drop_docs_name_iceberg() {
        let docs = get_sql_docs();
        let banned_a = concat!("ath", "ena");
        let banned_g = concat!("gl", "ue");
        for name in ["SELECT", "DROP DATABASE", "DROP TABLE"] {
            let description = &docs[name].description;
            assert!(
                description.contains("Iceberg"),
                "{name} must name Iceberg: {description}"
            );
            let lower = description.to_ascii_lowercase();
            assert!(
                !lower.contains(banned_g) && !lower.contains(banned_a),
                "{name} must not teach other warehouse sinks as the engine catalog: {description}"
            );
        }
    }

    #[test]
    fn checked_in_sql_docs_md_matches_formatted() {
        let path = repo_root().join("sql-docs.md");
        let on_disk = fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            on_disk,
            get_sql_docs_formatted(),
            "{} must be generated from get_sql_docs_formatted()",
            path.display()
        );
    }

    #[test]
    fn engine_sql_markdown_is_not_a_second_handwritten_sot() {
        let gone = repo_root().join("docs/docs/sql/reference.md");
        assert!(
            !gone.exists(),
            "{} is a second SQL SoT; delete it and point CLI docs at sql-help / sql-docs.md",
            gone.display()
        );
        let query_md = fs::read_to_string(repo_root().join("docs/docs/cli/query.md")).unwrap();
        assert!(
            !query_md.contains("sql/reference"),
            "cli/query.md must not link a handwritten SQL reference"
        );
        assert!(
            query_md.contains("sql-help") || query_md.contains("SHOW DOCS"),
            "cli/query.md must send readers to sql-help or SHOW DOCS"
        );
        assert!(
            !repo_root().join("src/commands/sql_help.rs").exists(),
            "src/commands/sql_help.rs is a dead second sql-help path"
        );
    }

    #[test]
    fn warehouse_cutover_section_three_is_historical() {
        let text = fs::read_to_string(
            repo_root().join("docs/docs/maintainers/warehouse-sinks-cutover.md"),
        )
        .unwrap();
        assert!(
            !text.contains("## 3. Current state (audit)"),
            "cutover §3 must not read as present-tense current state"
        );
        assert!(
            !text.contains(&format!(
                "src/sqlrt/docs.rs: remove {}/{} text",
                concat!("Ath", "ena"),
                concat!("Gl", "ue")
            )),
            "W2.7 must not still treat warehouse-sink query docs as open work"
        );
    }

    #[test]
    fn doc_parser_looks_up_show_docs_from_get_sql_docs() {
        let src = include_str!("doc_parser.rs");
        for name in [
            "SHOW STATS",
            "SHOW SEMANTIC",
            "SHOW CATALOG",
            "SHOW PIPELINE",
        ] {
            assert!(
                src.contains(&format!("docs.get(\"{name}\")")),
                "doc_parser must look up {name} from get_sql_docs()"
            );
            assert!(
                !src.contains(&format!("name: \"{name}\"")),
                "doc_parser must not inline {name} SqlStatementDoc fields"
            );
        }
    }
}
