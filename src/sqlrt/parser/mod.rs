use core::fmt;
use datafusion::sql::sqlparser;
use datafusion::sql::sqlparser::ast::ObjectName;
use datafusion::sql::sqlparser::dialect::Dialect;
use datafusion::sql::sqlparser::keywords::Keyword;
use datafusion::sql::sqlparser::parser::{Parser, ParserError};
use datafusion::sql::sqlparser::tokenizer::{Token, Tokenizer};
use sqlparser::ast::{ArrayElemTypeDef, DataType, Ident};
use sqlparser::dialect::GenericDialect;
use std::collections::VecDeque;

// Keywords used in Skippr SQL
// Defined as a separate enum to avoid conflicts with `sqlparser::ast::Keyword`
// Extending sqlparser define_keywords! macro would be nicer? Not possible in any case and maintainers are not
// interested (understandably) in endlessly adding support for esoteric dialects

enum SkipprKeyword {
    DUMP,
    LOAD,
    SCHEMA,
    ENABLE,
    DISABLE,
    PIPELINE,
    DATABASE,
    DROP,
    RESET,
    TABLE,
}

impl SkipprKeyword {
    fn from_str(s: &str) -> Option<SkipprKeyword> {
        match s.to_uppercase().as_str() {
            "DUMP" => Some(SkipprKeyword::DUMP),
            "LOAD" => Some(SkipprKeyword::LOAD),
            "DATABASE" => Some(SkipprKeyword::DATABASE),
            "SCHEMA" => Some(SkipprKeyword::SCHEMA),
            "ENABLE" => Some(SkipprKeyword::ENABLE),
            "DISABLE" => Some(SkipprKeyword::DISABLE),
            "PIPELINE" => Some(SkipprKeyword::PIPELINE),
            "DROP" => Some(SkipprKeyword::DROP),
            "RESET" => Some(SkipprKeyword::RESET),
            "TABLE" => Some(SkipprKeyword::TABLE),
            _ => None,
        }
    }
}

// After the SkipprKeyword enum, add another enum
enum SkipprShowCommand {
    DOCS,
    STATS,
    SEMANTIC,
    CATALOG,
    PIPELINE,
}

impl SkipprShowCommand {
    fn from_str(s: &str) -> Option<SkipprShowCommand> {
        match s.to_uppercase().as_str() {
            "DOCS" => Some(SkipprShowCommand::DOCS),
            "STATS" => Some(SkipprShowCommand::STATS),
            "SEMANTIC" => Some(SkipprShowCommand::SEMANTIC),
            "CATALOG" => Some(SkipprShowCommand::CATALOG),
            "PIPELINE" => Some(SkipprShowCommand::PIPELINE),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaDumpSource {
    // `SCHEMA DUMP <object_name> TO <schema file>`
    #[allow(dead_code)]
    Relation(ObjectName),
}

impl std::fmt::Display for SchemaDumpSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaDumpSource::Relation(name) => write!(f, "{}", name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaLoadDest {
    // `SCHEMA LOAD <schema file> INTO <object_name>`
    #[allow(dead_code)]
    Relation(ObjectName),
}

impl std::fmt::Display for SchemaLoadDest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaLoadDest::Relation(name) => write!(f, "{}", name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineToggle {
    Enable,
    Disable,
}

impl PipelineToggle {
    fn from_str(s: &str) -> Option<PipelineToggle> {
        match s.to_uppercase().as_str() {
            "ENABLE" => Some(PipelineToggle::Enable),
            "DISABLE" => Some(PipelineToggle::Disable),
            _ => None,
        }
    }
}

impl fmt::Display for PipelineToggle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PipelineToggle::Enable => {
                write!(f, "Enable")
            }
            PipelineToggle::Disable => {
                write!(f, "Disable")
            }
        }
    }
}

/// Skppr extension DDL for `SCHEMA DUMP`
///
/// # Syntax:
///
/// ```text
/// SCHEMA DUMP <table_name>
/// TO
/// <destination_url>
/// ```
///
/// # Examples
///
/// ```sql
/// SCHEMA DUMP bike_hire TO 'bike_hire.yaml'
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDumpStatement {
    pub pipeline: ObjectName,
    pub schema: Option<ObjectName>,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDropStatement {
    pub pipeline: ObjectName,
    pub schema: Option<ObjectName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseDropStatement {
    pub database: ObjectName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineDropStatement {
    pub pipeline: ObjectName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineResetStatement {
    pub pipeline: ObjectName,
}

/// Skppr extension DDL for `SCHEMA LOAD`
///
/// # Syntax:
///
/// ```text
/// SCHEMA LOAD <source_url>
/// INTO
/// <table_name>
/// ```
///
/// # Examples
///
/// ```sql
/// SCHEMA LOAD 'bike_hire.yaml INTO bike_hire'
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaLoadStatement {
    pub pipeline: SchemaLoadDest,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineToggleStatement {
    pub pipeline: ObjectName,
    pub toggle: PipelineToggle,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlterTableAddColumn {
    pub(crate) table_name: ObjectName,
    pub(crate) column_name: Ident,
    pub(crate) data_type: DataType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterSchemaDropColumn {
    pub pipeline: ObjectName,
    pub schema: Option<ObjectName>,
    pub column_name: ObjectName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterSchemaAlterColumnType {
    pub pipeline: ObjectName,
    pub schema: Option<ObjectName>,
    pub column_name: ObjectName,
    pub new_type: DataType,
    pub values_new_type: Option<DataType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDropStatement {
    pub schema: Option<ObjectName>,
    pub table: ObjectName,
}

/// Skippr SQL Statement.
///
/// This can either be a [`Statement`] from [`DFParser`] or [`sqlparser`] from a
/// standard SQL dialect, or a Skippr extension such as `SCHEMA DUMP,
/// SCHMEA LOAD`. See [`Sparser`] for more information.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Statement {
    /// ANSI SQL AST node (from sqlparser-rs)
    // Statement(Box<Statement>),
    /// Extension: `SCHEMA DUMP`
    SchemaDump(SchemaDumpStatement),
    DatabaseDrop(DatabaseDropStatement),
    SchemaDrop(SchemaDropStatement),
    PipelineDrop(PipelineDropStatement),
    PipelineReset(PipelineResetStatement),
    /// Extension: `SCHEMA LOAD`
    #[allow(dead_code)]
    SchemaLoad(SchemaLoadStatement),
    PipelineToggle(PipelineToggleStatement),
    // AlterTableAddColumn(AlterTableAddColumn),
    AlterSchemaDropColumn(AlterSchemaDropColumn),
    AlterSchemaAlterColumnType(AlterSchemaAlterColumnType),
    TableDrop(TableDropStatement),
    /// Extension: `SHOW DOCS`
    ShowDocs,
    /// Extension: `SHOW STATS FOR <pipeline>.<namespace>` (namespace optional)
    ShowStats {
        pipeline: String,
        namespace: Option<String>,
    },
    /// Extension: `SHOW SEMANTIC FOR <pipeline>.<namespace>` (namespace optional)
    ShowSemantic {
        pipeline: String,
        namespace: Option<String>,
    },
    /// Extension: `SHOW CATALOG FOR <pipeline>.<namespace>` (namespace optional)
    ShowCatalog {
        pipeline: String,
        namespace: Option<String>,
    },
    /// Extension: `SHOW PIPELINE <pipeline_name>`
    ShowPipeline {
        pipeline: String,
    },
}

/// SQL Parser for Skipprs's SQL dialect, which will often delegate to [`Datafusion`] or [`sqlparser`]
///
/// Skippr SQL adds support for statements supporting the modification of schemas, typically discovered via
/// Skipprs type inference and backwards compatible evolution rules.
pub struct SParser<'a> {
    parser: Parser<'a>,
}

impl<'a> SParser<'a> {
    pub fn new(sql: &str) -> Result<Self, ParserError> {
        let dialect = &GenericDialect {};
        SParser::new_with_dialect(sql, dialect)
    }

    pub fn new_with_dialect(sql: &str, dialect: &'a dyn Dialect) -> Result<Self, ParserError> {
        let mut tokenizer = Tokenizer::new(dialect, sql);
        let tokens = tokenizer.tokenize()?;

        Ok(SParser {
            parser: Parser::new(dialect).with_tokens(tokens),
        })
    }

    #[allow(dead_code)]
    fn parse_sql(sql: &str) -> Result<VecDeque<Statement>, ParserError> {
        let dialect = &GenericDialect {};
        SParser::parse_sql_with_dialect(sql, dialect)
    }

    #[allow(dead_code)]
    pub fn parse_sql_with_dialect(
        sql: &str,
        dialect: &dyn Dialect,
    ) -> Result<VecDeque<Statement>, ParserError> {
        let mut parser = SParser::new_with_dialect(sql, dialect)?;
        let mut stmts = VecDeque::new();
        let mut expecting_statement_delimiter = false;
        loop {
            // ignore empty statements (between successive statement delimiters)
            while parser.parser.consume_token(&Token::SemiColon) {
                expecting_statement_delimiter = false;
            }

            if parser.parser.peek_token() == Token::EOF {
                break;
            }
            if expecting_statement_delimiter {
                return parser
                    .parser
                    .expected("end of statement", parser.parser.peek_token());
            }

            let statement = parser.parse_statement()?;
            stmts.push_back(statement);
            expecting_statement_delimiter = true;
        }
        Ok(stmts)
    }

    pub fn parse_statement(&mut self) -> Result<Statement, ParserError> {
        return match self.parser.peek_token().token {
            Token::Word(w) => {
                if w.value.to_uppercase() == "SHOW" {
                    self.parser.next_token(); // SHOW
                    if let Token::Word(w) = self.parser.peek_token().token {
                        match SkipprShowCommand::from_str(&w.value) {
                            Some(SkipprShowCommand::DOCS) => {
                                self.parser.next_token();
                                return Ok(Statement::ShowDocs);
                            }
                            Some(SkipprShowCommand::STATS) => {
                                self.parser.next_token(); // STATS
                                self.parser.expect_keyword(Keyword::FOR)?;
                                let name = self.parser.parse_object_name(true)?;
                                let parts: Vec<String> =
                                    name.0.iter().map(|i| i.to_string()).collect();
                                let (pipeline, namespace) = match parts.len() {
                                    0 => ("".to_string(), None),
                                    1 => (parts[0].clone(), None),
                                    _ => (parts[0].clone(), Some(parts[1..].join("."))),
                                };
                                return Ok(Statement::ShowStats {
                                    pipeline,
                                    namespace,
                                });
                            }
                            Some(SkipprShowCommand::SEMANTIC) => {
                                self.parser.next_token(); // SEMANTIC
                                self.parser.expect_keyword(Keyword::FOR)?;
                                let name = self.parser.parse_object_name(true)?;
                                let parts: Vec<String> =
                                    name.0.iter().map(|i| i.to_string()).collect();
                                let (pipeline, namespace) = match parts.len() {
                                    0 => ("".to_string(), None),
                                    1 => (parts[0].clone(), None),
                                    _ => (parts[0].clone(), Some(parts[1..].join("."))),
                                };
                                return Ok(Statement::ShowSemantic {
                                    pipeline,
                                    namespace,
                                });
                            }
                            Some(SkipprShowCommand::CATALOG) => {
                                self.parser.next_token(); // CATALOG
                                self.parser.expect_keyword(Keyword::FOR)?;
                                let name = self.parser.parse_object_name(true)?;
                                let parts: Vec<String> =
                                    name.0.iter().map(|i| i.to_string()).collect();
                                let (pipeline, namespace) = match parts.len() {
                                    0 => ("".to_string(), None),
                                    1 => (parts[0].clone(), None),
                                    _ => (parts[0].clone(), Some(parts[1..].join("."))),
                                };
                                return Ok(Statement::ShowCatalog {
                                    pipeline,
                                    namespace,
                                });
                            }
                            Some(SkipprShowCommand::PIPELINE) => {
                                self.parser.next_token(); // PIPELINE
                                let name = self.parser.parse_object_name(false)?;
                                let pipeline = name
                                    .0
                                    .iter()
                                    .map(|i| i.to_string())
                                    .collect::<Vec<_>>()
                                    .join(".");
                                return Ok(Statement::ShowPipeline { pipeline });
                            }
                            _ => {
                                return Err(ParserError::ParserError(
                                    "Unrecognized SHOW command".to_string(),
                                ));
                            }
                        }
                    } else {
                        return Err(ParserError::ParserError(
                            "Expected command after SHOW".to_string(),
                        ));
                    }
                }
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::DUMP) => {
                        self.parser.next_token();
                        self.parse_dump()
                    }
                    Some(SkipprKeyword::DROP) => {
                        self.parser.next_token();
                        self.parse_drop()
                    }
                    Some(SkipprKeyword::RESET) => {
                        self.parser.next_token();
                        self.parse_reset()
                    }
                    Some(SkipprKeyword::LOAD) => {
                        self.parser.next_token();
                        self.parse_load()
                    }
                    Some(SkipprKeyword::ENABLE) => {
                        self.parser.next_token();
                        self.parse_enable()
                    }
                    Some(SkipprKeyword::DISABLE) => {
                        self.parser.next_token();
                        self.parse_disable()
                    }
                    Some(SkipprKeyword::TABLE) => {
                        self.parser.next_token();
                        self.parse_drop()
                    }
                    Some(SkipprKeyword::SCHEMA)
                    | Some(SkipprKeyword::PIPELINE)
                    | Some(SkipprKeyword::DATABASE) => {
                        Err(ParserError::ParserError("Not implemented".to_string()))
                    }
                    None => {
                        // Fallback: handle ALTER TABLE <pipeline>[.<namespace>] ...
                        match self.parser.peek_token().token {
                            Token::Word(w2) => {
                                match w2.keyword {
                                    Keyword::ALTER => {
                                        self.parser.next_token(); // ALTER
                                        match self.parser.peek_token().token {
                                            Token::Word(w3) => {
                                                match w3.keyword {
                                                    // Hard-disable SCHEMA and instruct to use TABLE
                                                    Keyword::SCHEMA => {
                                                        Err(ParserError::ParserError(
                                                            "Use TABLE instead of SCHEMA"
                                                                .to_string(),
                                                        ))
                                                    }
                                                    Keyword::TABLE => {
                                                        self.parser.next_token(); // TABLE
                                                        self.parse_alter_schema()
                                                    }
                                                    _ => Err(ParserError::ParserError(
                                                        "Not implemented".to_string(),
                                                    )),
                                                }
                                            }
                                            _ => Err(ParserError::ParserError(
                                                "Not implemented".to_string(),
                                            )),
                                        }
                                    }
                                    _ => {
                                        Err(ParserError::ParserError("Not implemented".to_string()))
                                    }
                                }
                            }
                            _ => Err(ParserError::ParserError("Not implemented".to_string())),
                        }
                    }
                }
            }
            _ => Err(ParserError::ParserError("Not implemented".to_string())),
        };
    }

    // This is a simplified sketch and needs to be integrated with your existing parsing logic.

    pub fn parse_alter_schema(&mut self) -> Result<Statement, ParserError> {
        let pipeline = self.parser.next_token().token.to_string();

        let schema = match self.parser.peek_token().token.to_string().as_str() {
            "." => {
                self.parser.next_token(); // .
                let schema = self.parser.next_token().token.to_string();
                Some(ObjectName(vec![
                    datafusion::logical_expr::sqlparser::ast::ObjectNamePart::Identifier(
                        Ident::new(schema),
                    ),
                ]))
            }
            _ => None,
        };

        match self.parser.next_token().token {
            Token::Word(w) => {
                match w.keyword {
                    Keyword::ADD => Err(ParserError::ParserError(
                        "ALTER COLUMN ADD Not implemented".to_string(),
                    )),
                    Keyword::DROP => {
                        self.parser.expect_keyword(Keyword::COLUMN)?;
                        let column_name = self.parser.parse_object_name(false)?;
                        Ok(Statement::AlterSchemaDropColumn(AlterSchemaDropColumn {
                        pipeline: ObjectName(vec![datafusion::logical_expr::sqlparser::ast::ObjectNamePart::Identifier(Ident::new(pipeline))]),
                        schema,
                        column_name,
                    }))
                    }
                    Keyword::ALTER => {
                        self.parser.expect_keyword(Keyword::COLUMN)?;

                        let column_name = self.parser.parse_object_name(false)?;

                        self.parser.expect_keyword(Keyword::TYPE)?;

                        let column_new_type = self.parser.parse_data_type()?;

                        match column_new_type  {
                        DataType::Array(value_type) => {
                            match value_type {
                                ArrayElemTypeDef::AngleBracket(value) => {

                                        Ok(Statement::AlterSchemaAlterColumnType(AlterSchemaAlterColumnType {
                                        pipeline: ObjectName(vec![datafusion::logical_expr::sqlparser::ast::ObjectNamePart::Identifier(Ident::new(pipeline))]),
                                        schema,
                                        column_name,
                                        // strip the <value type> from ARRAY<value type> to support matching against `SkipprDataType`
                                        new_type: DataType::Array(ArrayElemTypeDef::None),
                                        values_new_type: Some(*value)
                                     }))
                                }
                                _ => {
                                    return Err(ParserError::ParserError("Expected values type for array, e.g. ARRAY<INT>".to_string()));
                                }
                            }
                        },
                        _ => {
                            Ok(Statement::AlterSchemaAlterColumnType(AlterSchemaAlterColumnType {
                                pipeline: ObjectName(vec![datafusion::logical_expr::sqlparser::ast::ObjectNamePart::Identifier(Ident::new(pipeline))]),
                                schema,
                                column_name,
                                new_type: column_new_type,
                                values_new_type: None
                            }))
                        }
                    }
                    }
                    _ => Err(ParserError::ParserError("Unexpected keyword".to_string())),
                }
            }
            _ => Err(ParserError::ParserError(
                "Expected keyword after ALTER TABLE".to_string(),
            )),
        }
    }

    pub fn parse_enable(&mut self) -> Result<Statement, ParserError> {
        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::PIPELINE) => {
                        self.parser.next_token(); // PIPELINE

                        let pipeline = self.parser.parse_object_name(true)?;

                        Ok(Statement::PipelineToggle(PipelineToggleStatement {
                            pipeline,
                            toggle: PipelineToggle::from_str("ENABLE").unwrap(),
                        }))
                    }
                    _ => Err(ParserError::ParserError("Not implemented".to_string())),
                }
            }
            _ => Err(ParserError::ParserError("Unknown error".to_string())),
        };
    }

    pub fn parse_disable(&mut self) -> Result<Statement, ParserError> {
        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::PIPELINE) => {
                        self.parser.next_token(); // PIPELINE

                        let pipeline = self.parser.parse_object_name(true)?;

                        Ok(Statement::PipelineToggle(PipelineToggleStatement {
                            pipeline,
                            toggle: PipelineToggle::from_str("DISABLE").unwrap(),
                        }))
                    }
                    _ => Err(ParserError::ParserError("Not implemented".to_string())),
                }
            }
            _ => Err(ParserError::ParserError("Unknown error".to_string())),
        };
    }

    pub fn parse_load(&mut self) -> Result<Statement, ParserError> {
        return match self.parser.peek_token().token {
            Token::Word(w) => match SkipprKeyword::from_str(&w.value) {
                Some(SkipprKeyword::SCHEMA) => {
                    self.parser.next_token(); // SCHEMA

                    let source = self.parser.parse_literal_string()?;

                    self.parser.expect_keyword(Keyword::INTO)?;

                    let table_name = self.parser.parse_object_name(false)?;

                    Ok(Statement::SchemaLoad(SchemaLoadStatement {
                        pipeline: SchemaLoadDest::Relation(table_name),
                        source,
                    }))
                }
                _ => Err(ParserError::ParserError("Not implemented".to_string())),
            },
            _ => Err(ParserError::ParserError("Unknown error".to_string())),
        };
    }

    pub fn parse_dump(&mut self) -> Result<Statement, ParserError> {
        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::TABLE) => {
                        self.parser.next_token(); // TABLE

                        let pipeline = self.parser.next_token().token.to_string();

                        let schema = match self.parser.peek_token().token.to_string().as_str() {
                            "." => {
                                self.parser.next_token(); // .
                                let schema = self.parser.next_token().token.to_string();
                                Some(schema)
                            }
                            _ => None,
                        };

                        self.parser.expect_keyword(Keyword::TO)?;

                        let target = match self.parser.parse_literal_string() {
                            Ok(s) => s,
                            Err(e) => {
                                return Err(e);
                            }
                        };

                        Ok(Statement::SchemaDump(SchemaDumpStatement {
                            // pipeline: SchemaDumpSource::Relation(pipeline),
                            pipeline: ObjectName(vec![datafusion::logical_expr::sqlparser::ast::ObjectNamePart::Identifier(Ident::new(pipeline))]),
                            schema: schema.map(|s| ObjectName(vec![datafusion::logical_expr::sqlparser::ast::ObjectNamePart::Identifier(Ident::new(s))])),
                            target
                        }))
                    }
                    Some(SkipprKeyword::SCHEMA) => Err(ParserError::ParserError(
                        "Use TABLE instead of SCHEMA".to_string(),
                    )),
                    _ => Err(ParserError::ParserError("Not implemented".to_string())),
                }
            }
            _ => Err(ParserError::ParserError("Unknown error".to_string())),
        };
    }

    pub fn parse_reset(&mut self) -> Result<Statement, ParserError> {
        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::PIPELINE) => {
                        self.parser.next_token(); // PIPELINE

                        let table_name = self.parser.parse_object_name(true)?;

                        Ok(Statement::PipelineReset(PipelineResetStatement {
                            pipeline: table_name,
                        }))
                    }
                    _ => Err(ParserError::ParserError("Not implemented".to_string())),
                }
            }
            _ => Err(ParserError::ParserError("Unknown error".to_string())),
        };
    }

    pub fn parse_drop(&mut self) -> Result<Statement, ParserError> {
        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    // Support both DUMP SCHEMA ... and DUMP TABLE ...
                    Some(SkipprKeyword::TABLE) => {
                        self.parser.next_token(); // SCHEMA/TABLE

                        let pipeline = self.parser.next_token().token.to_string();

                        let schema = match self.parser.peek_token().token.to_string().as_str() {
                            "." => {
                                self.parser.next_token(); // .
                                let schema = self.parser.next_token().token.to_string();
                                Some(schema)
                            }
                            _ => None,
                        };

                        Ok(Statement::TableDrop(TableDropStatement {
                            schema: schema.map(|s| ObjectName(vec![datafusion::logical_expr::sqlparser::ast::ObjectNamePart::Identifier(Ident::new(s))])),
                            table: ObjectName(vec![datafusion::logical_expr::sqlparser::ast::ObjectNamePart::Identifier(Ident::new(pipeline))])
                        }))
                        // }
                    }
                    Some(SkipprKeyword::PIPELINE) => {
                        self.parser.next_token(); // PIPELINE

                        let table_name = self.parser.parse_object_name(true)?;

                        Ok(Statement::PipelineDrop(PipelineDropStatement {
                            pipeline: table_name,
                        }))
                    }
                    Some(SkipprKeyword::DATABASE) => {
                        self.parser.next_token(); // DATABASE

                        let database = self.parser.parse_object_name(false)?;

                        Ok(Statement::DatabaseDrop(DatabaseDropStatement { database }))
                    }
                    // SCHEMA keyword is deprecated; instruct users
                    Some(SkipprKeyword::SCHEMA) => Err(ParserError::ParserError(
                        "Use TABLE instead of SCHEMA".to_string(),
                    )),
                    _ => Err(ParserError::ParserError("Not implemented".to_string())),
                }
            }
            _ => Err(ParserError::ParserError("Unknown error".to_string())),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_pipeline_unquoted_hyphen_truncates_name() {
        let mut parser = SParser::new("SHOW PIPELINE mssql-migration").unwrap();
        let stmt = parser.parse_statement().unwrap();
        match stmt {
            Statement::ShowPipeline { pipeline } => {
                assert_eq!(
                    pipeline, "mssql",
                    "unquoted hyphenated name is parsed as subtraction"
                );
            }
            other => panic!("expected ShowPipeline, got {:?}", other),
        }
    }

    #[test]
    fn show_pipeline_quoted_hyphenated_name() {
        let mut parser = SParser::new(r#"SHOW PIPELINE "mssql-migration""#).unwrap();
        let stmt = parser.parse_statement().unwrap();
        match stmt {
            Statement::ShowPipeline { pipeline } => {
                // query.rs strips quotes with .replace('"', "") before use
                let resolved = pipeline.replace('"', "");
                assert_eq!(resolved, "mssql-migration");
            }
            other => panic!("expected ShowPipeline, got {:?}", other),
        }
    }

    #[test]
    fn show_pipeline_simple_name() {
        let mut parser = SParser::new("SHOW PIPELINE mssql").unwrap();
        let stmt = parser.parse_statement().unwrap();
        match stmt {
            Statement::ShowPipeline { pipeline } => {
                assert_eq!(pipeline, "mssql");
            }
            other => panic!("expected ShowPipeline, got {:?}", other),
        }
    }
}
