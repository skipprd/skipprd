use crate::sqlrt::docs::{get_sql_docs, SqlStatementDoc};
use crate::sqlrt::parser::{PipelineToggle, SParser, Statement};

/// SQL documentation parser
/// This parser analyzes SQL statements and returns documentation about them
pub struct SqlDocParser;

impl SqlDocParser {
    /// Parses a SQL statement and returns documentation about it
    pub fn parse_and_document(sql: &str) -> Result<Option<SqlStatementDoc>, String> {
        // First try to parse with our custom parser
        match SParser::new(sql) {
            Ok(mut parser) => {
                match parser.parse_statement() {
                    Ok(statement) => {
                        return Ok(Some(SqlDocParser::get_doc_for_statement(statement)));
                    }
                    Err(_e) => {
                        // If our parser fails, try to identify the statement type
                        // by looking at the first few tokens
                        return Ok(SqlDocParser::identify_statement_type(sql));
                    }
                }
            }
            Err(_e) => {
                return Err(format!("Failed to parse SQL: {}", _e));
            }
        }
    }

    /// Gets documentation for a statement
    fn get_doc_for_statement(statement: Statement) -> SqlStatementDoc {
        let docs = get_sql_docs();

        match statement {
            Statement::SchemaDump(_) => docs.get("SCHEMA DUMP").unwrap().clone(),
            Statement::DatabaseDrop(_) => docs.get("DROP DATABASE").unwrap().clone(),
            Statement::SchemaDrop(_) => docs.get("DROP SCHEMA").unwrap().clone(),
            Statement::PipelineDrop(_) => docs.get("DROP PIPELINE").unwrap().clone(),
            Statement::PipelineReset(_) => docs.get("RESET PIPELINE").unwrap().clone(),
            Statement::SchemaLoad(_) => docs.get("SCHEMA LOAD").unwrap().clone(),
            Statement::PipelineToggle(stmt) => match stmt.toggle {
                PipelineToggle::Enable => docs.get("ENABLE PIPELINE").unwrap().clone(),
                PipelineToggle::Disable => docs.get("DISABLE PIPELINE").unwrap().clone(),
            },
            Statement::AlterTable(stmt) => match stmt.op {
                crate::discover::schema_alter::SchemaAlterOp::Drop { .. } => {
                    docs.get("ALTER TABLE DROP COLUMN").unwrap().clone()
                }
                crate::discover::schema_alter::SchemaAlterOp::Promote { .. } => {
                    docs.get("ALTER TABLE ALTER COLUMN").unwrap().clone()
                }
                crate::discover::schema_alter::SchemaAlterOp::Rename { .. } => {
                    docs.get("ALTER TABLE RENAME COLUMN").unwrap().clone()
                }
                crate::discover::schema_alter::SchemaAlterOp::Merge { .. } => {
                    docs.get("ALTER TABLE MERGE COLUMN").unwrap().clone()
                }
            },
            Statement::TableDrop(_) => docs.get("DROP TABLE").unwrap().clone(),
            Statement::ShowDocs => docs.get("SHOW DOCS").unwrap().clone(),
            Statement::ShowStats { .. } => docs.get("SHOW STATS").unwrap().clone(),
            Statement::ShowSemantic { .. } => docs.get("SHOW SEMANTIC").unwrap().clone(),
            Statement::ShowCatalog { .. } => docs.get("SHOW CATALOG").unwrap().clone(),
            Statement::ShowPipeline { .. } => docs.get("SHOW PIPELINE").unwrap().clone(),
        }
    }

    /// Identifies the statement type by looking at the first few tokens
    fn identify_statement_type(sql: &str) -> Option<SqlStatementDoc> {
        let sql_lower = sql.to_lowercase();
        let docs = get_sql_docs();

        if sql_lower.starts_with("select") {
            return Some(docs.get("SELECT").unwrap().clone());
        } else if sql_lower.contains("datediff") {
            return Some(docs.get("DATEDIFF").unwrap().clone());
        } else if sql_lower == "show docs" {
            return Some(docs.get("SHOW DOCS").unwrap().clone());
        }

        None
    }
}
