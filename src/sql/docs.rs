use std::collections::HashMap;
use serde::{Serialize, Deserialize};

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

/// Returns documentation for all SQL statements supported by the application
pub fn get_sql_docs() -> HashMap<String, SqlStatementDoc> {
    let mut docs = HashMap::new();

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
            description: "Drops a database from the AWS Glue Catalog.".to_string(),
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
            description: "Loads a schema definition from a file into a pipeline.".to_string(),
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

    // Alter Schema Drop Column
    docs.insert(
        "ALTER SCHEMA DROP COLUMN".to_string(),
        SqlStatementDoc {
            name: "ALTER SCHEMA DROP COLUMN".to_string(),
            syntax: "ALTER SCHEMA <pipeline_name>[.<schema_name>] DROP COLUMN <column_name>".to_string(),
            description: "Drops a column from a schema. Supports nested fields using dot notation.".to_string(),
            example: "ALTER SCHEMA bike_hire DROP COLUMN user_id".to_string(),
        },
    );

    // Alter Schema Alter Column Type
    docs.insert(
        "ALTER SCHEMA ALTER COLUMN".to_string(),
        SqlStatementDoc {
            name: "ALTER SCHEMA ALTER COLUMN".to_string(),
            syntax: "ALTER SCHEMA <pipeline_name>[.<schema_name>] ALTER COLUMN <column_name> TYPE <new_type>".to_string(),
            description: "Changes the data type of a column in a schema. For arrays, use ARRAY<TYPE> format.".to_string(),
            example: "ALTER SCHEMA bike_hire ALTER COLUMN price TYPE DECIMAL(10,2)".to_string(),
        },
    );

    // Drop Table
    docs.insert(
        "DROP TABLE".to_string(),
        SqlStatementDoc {
            name: "DROP TABLE".to_string(),
            syntax: "DROP TABLE [<schema_name>.]<table_name>".to_string(),
            description: "Drops a table from the metadata and from AWS Glue catalog.".to_string(),
            example: "DROP TABLE analytics.user_events".to_string(),
        },
    );

    // Deadletters Table (Querying)
    docs.insert(
        "DEADLETTERS TABLE".to_string(),
        SqlStatementDoc {
            name: "DEADLETTERS TABLE".to_string(),
            syntax: "SELECT <columns> FROM deadletters [WHERE namespace = '<ns>'] [AND dt BETWEEN 'YYYY-MM-DD' AND 'YYYY-MM-DD']".to_string(),
            description: "Query deadletter events uploaded directly to the Skippr state bucket under deadletters/. Supports JSON extraction via json_extract_scalar(record.raw_json, '$.<path>').".to_string(),
            example: "SELECT id, namespace, failure.error_messages[1] AS err FROM deadletters WHERE namespace = 'bike_hire' AND dt BETWEEN '2025-11-05' AND '2025-11-07'".to_string(),
        },
    );

    // Standard SQL queries
    docs.insert(
        "SELECT".to_string(),
        SqlStatementDoc {
            name: "SELECT".to_string(),
            syntax: "SELECT <columns> FROM <table_name> [WHERE <condition>] [GROUP BY <expressions>] [HAVING <condition>] [ORDER BY <expressions>] [LIMIT <count>]".to_string(),
            description: "Executes a standard SQL query against the data. Supports querying from AWS Athena/Glue tables.".to_string(),
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

    // Show Docs command
    docs.insert(
        "SHOW DOCS".to_string(),
        SqlStatementDoc {
            name: "SHOW DOCS".to_string(),
            syntax: "SHOW DOCS".to_string(),
            description: "Displays the documentation for all supported SQL statements.".to_string(),
            example: "SHOW DOCS".to_string(),
        },
    );

    docs
}

/// Returns a formatted string with documentation for all SQL statements
pub fn get_sql_docs_formatted() -> String {
    let docs = get_sql_docs();
    let mut result = String::new();
    
    result.push_str("# Skippr SQL Documentation\n\n");
    result.push_str("This document describes all SQL statements supported by Skippr.\n\n");
    
    // Group docs by category
    let mut schema_operations: Vec<&SqlStatementDoc> = Vec::new();
    let mut pipeline_operations: Vec<&SqlStatementDoc> = Vec::new();
    let mut data_operations: Vec<&SqlStatementDoc> = Vec::new();
    let mut query_operations: Vec<&SqlStatementDoc> = Vec::new();
    
    for (_, doc) in &docs {
        if doc.name.contains("SCHEMA") {
            schema_operations.push(doc);
        } else if doc.name.contains("PIPELINE") {
            pipeline_operations.push(doc);
        } else if doc.name.contains("TABLE") || doc.name.contains("DATABASE") {
            data_operations.push(doc);
        } else {
            query_operations.push(doc);
        }
    }
    
    // Add schema operations
    result.push_str("## Schema Operations\n\n");
    for doc in schema_operations {
        add_doc_to_result(&mut result, doc);
    }
    
    // Add pipeline operations
    result.push_str("## Pipeline Operations\n\n");
    for doc in pipeline_operations {
        add_doc_to_result(&mut result, doc);
    }
    
    // Add data operations
    result.push_str("## Data Operations\n\n");
    for doc in data_operations {
        add_doc_to_result(&mut result, doc);
    }
    
    // Add query operations
    result.push_str("## Query Operations\n\n");
    for doc in query_operations {
        add_doc_to_result(&mut result, doc);
    }
    
    result
}

fn add_doc_to_result(result: &mut String, doc: &SqlStatementDoc) {
    result.push_str(&format!("### {}\n\n", doc.name));
    result.push_str(&format!("**Syntax:**\n```sql\n{}\n```\n\n", doc.syntax));
    result.push_str(&format!("**Description:**\n{}\n\n", doc.description));
    result.push_str(&format!("**Example:**\n```sql\n{}\n```\n\n", doc.example));
}

/// Returns a list of supported SQL statements with their syntax
#[allow(dead_code)]
pub fn list_supported_sql_statements() -> Vec<String> {
    get_sql_docs().into_iter().map(|(_, doc)| format!("{}: {}", doc.name, doc.syntax)).collect()
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
    let docs = get_sql_docs();
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
    
    // Group docs by category
    let mut schema_operations: Vec<&SqlStatementDoc> = Vec::new();
    let mut pipeline_operations: Vec<&SqlStatementDoc> = Vec::new();
    let mut data_operations: Vec<&SqlStatementDoc> = Vec::new();
    let mut query_operations: Vec<&SqlStatementDoc> = Vec::new();
    
    for (_, doc) in &docs {
        if doc.name.contains("SCHEMA") {
            schema_operations.push(doc);
        } else if doc.name.contains("PIPELINE") {
            pipeline_operations.push(doc);
        } else if doc.name.contains("TABLE") || doc.name.contains("DATABASE") {
            data_operations.push(doc);
        } else {
            query_operations.push(doc);
        }
    }
    
    // Add schema operations
    result.push_str("<h2>Schema Operations</h2>\n");
    for doc in schema_operations {
        add_doc_to_html(&mut result, doc);
    }
    
    // Add pipeline operations
    result.push_str("<h2>Pipeline Operations</h2>\n");
    for doc in pipeline_operations {
        add_doc_to_html(&mut result, doc);
    }
    
    // Add data operations
    result.push_str("<h2>Data Operations</h2>\n");
    for doc in data_operations {
        add_doc_to_html(&mut result, doc);
    }
    
    // Add query operations
    result.push_str("<h2>Query Operations</h2>\n");
    for doc in query_operations {
        add_doc_to_html(&mut result, doc);
    }
    
    result.push_str("</body>\n</html>");
    
    result
}

/// Returns documentation in JSON format
pub fn get_sql_docs_json() -> String {
    use serde::{Serialize, Deserialize};
    use serde_json::json;
    
    #[derive(Serialize, Deserialize)]
    struct DocCategory {
        name: String,
        statements: Vec<SqlStatementDoc>,
    }
    
    let docs = get_sql_docs();
    
    // Group docs by category
    let mut schema_operations: Vec<SqlStatementDoc> = Vec::new();
    let mut pipeline_operations: Vec<SqlStatementDoc> = Vec::new();
    let mut data_operations: Vec<SqlStatementDoc> = Vec::new();
    let mut query_operations: Vec<SqlStatementDoc> = Vec::new();
    
    for (_, doc) in docs {
        if doc.name.contains("SCHEMA") {
            schema_operations.push(doc);
        } else if doc.name.contains("PIPELINE") {
            pipeline_operations.push(doc);
        } else if doc.name.contains("TABLE") || doc.name.contains("DATABASE") {
            data_operations.push(doc);
        } else {
            query_operations.push(doc);
        }
    }
    
    let categories = vec![
        DocCategory {
            name: "Schema Operations".to_string(),
            statements: schema_operations,
        },
        DocCategory {
            name: "Pipeline Operations".to_string(),
            statements: pipeline_operations,
        },
        DocCategory {
            name: "Data Operations".to_string(),
            statements: data_operations,
        },
        DocCategory {
            name: "Query Operations".to_string(),
            statements: query_operations,
        },
    ];
    
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