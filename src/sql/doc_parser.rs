
use crate::sql::docs::{SqlStatementDoc, get_sql_docs};
use crate::sql::parser::{SParser, Statement, PipelineToggle};

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
                    Err(e) => {
                        // If our parser fails, try to identify the statement type
                        // by looking at the first few tokens
                        return Ok(SqlDocParser::identify_statement_type(sql));
                    }
                }
            }
            Err(e) => {
                return Err(format!("Failed to parse SQL: {}", e));
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
            Statement::PipelineToggle(stmt) => {
                match stmt.toggle {
                    PipelineToggle::Enable => docs.get("ENABLE PIPELINE").unwrap().clone(),
                    PipelineToggle::Disable => docs.get("DISABLE PIPELINE").unwrap().clone(),
                }
            },
            Statement::AlterSchemaDropColumn(_) => docs.get("ALTER SCHEMA DROP COLUMN").unwrap().clone(),
            Statement::AlterSchemaAlterColumnType(_) => docs.get("ALTER SCHEMA ALTER COLUMN").unwrap().clone(),
            Statement::TableDrop(_) => docs.get("DROP TABLE").unwrap().clone(),
            Statement::ShowDocs => docs.get("SHOW DOCS").unwrap().clone(),
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

    /// Returns a list of all supported SQL statements
    pub fn list_all_statements() -> Vec<SqlStatementDoc> {
        get_sql_docs().into_values().collect()
    }
    
    /// Validates if a SQL statement is supported
    pub fn is_supported(sql: &str) -> bool {
        match SqlDocParser::parse_and_document(sql) {
            Ok(Some(_)) => true,
            _ => false,
        }
    }
    
    /// Returns documentation in Markdown format for all supported SQL statements
    pub fn generate_markdown_docs() -> String {
        crate::sql::docs::get_sql_docs_formatted()
    }
    
    /// Exports the documentation to a file
    pub fn export_docs_to_file(file_path: &str) -> Result<(), std::io::Error> {
        use std::fs::File;
        use std::io::Write;
        
        let docs = SqlDocParser::generate_markdown_docs();
        let mut file = File::create(file_path)?;
        file.write_all(docs.as_bytes())?;
        
        Ok(())
    }
} 