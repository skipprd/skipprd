use core::fmt;
use sqlparser::dialect::GenericDialect;
use std::collections::VecDeque;
use datafusion::sql::parser::{DFParser};
use datafusion::sql::{sqlparser};
use datafusion::sql::sqlparser::ast::{JsonOperator, ObjectName};
use datafusion::sql::sqlparser::dialect::Dialect;
use datafusion::sql::sqlparser::keywords::Keyword;
use datafusion::sql::sqlparser::parser::{Parser, ParserError};
use datafusion::sql::sqlparser::tokenizer::{Token, Tokenizer};
use sqlparser::ast::{ArrayElemTypeDef, DataType, Ident};

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
    DROP,
    RESET,
}

impl SkipprKeyword {

    fn from_str(s: &str) -> Option<SkipprKeyword> {
        match s.to_uppercase().as_str() {
            "DUMP" => Some(SkipprKeyword::DUMP),
            "LOAD" => Some(SkipprKeyword::LOAD),
            "SCHEMA" => Some(SkipprKeyword::SCHEMA),
            "ENABLE" => Some(SkipprKeyword::ENABLE),
            "DISABLE" => Some(SkipprKeyword::DISABLE),
            "PIPELINE" => Some(SkipprKeyword::PIPELINE),
            "DROP" => Some(SkipprKeyword::DROP),
            "RESET" => Some(SkipprKeyword::RESET),
            _ => None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaDumpSource {
    // `SCHEMA DUMP <object_name> TO <schema file>`
    Relation(ObjectName),
}

impl std::fmt::Display for SchemaDumpSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaDumpSource::Relation(name) => write!(f, "{}", name)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaLoadDest {
     // `SCHEMA LOAD <schema file> INTO <object_name>`
    Relation(ObjectName),
}

impl std::fmt::Display for SchemaLoadDest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaLoadDest::Relation(name) => write!(f, "{}", name)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PipelineToggle {
    Enable,
    Disable
}

impl PipelineToggle {

    fn from_str(s: &str) -> Option<PipelineToggle> {
        match s.to_uppercase().as_str() {
            "ENABLE" => Some(PipelineToggle::Enable),
            "DISABLE" => Some(PipelineToggle::Disable),
            _ => None
        }
    }

}

impl fmt::Display for PipelineToggle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PipelineToggle::Enable => {
                write!(f, "Enable")
            },
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
pub(crate) struct SchemaDumpStatement {
    /// From where the data comes from
    pub(crate)  table: SchemaDumpSource,
    /// The URL to where the data is heading
    pub(crate)  target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaDropStatement {
    pub(crate)  table: ObjectName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelineDropStatement {
    pub(crate)  table: ObjectName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelineResetStatement {
    pub(crate)  table: ObjectName,
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
pub(crate) struct SchemaLoadStatement {
    /// The object name to where the data is heading
    pub(crate)  table: SchemaLoadDest,
    /// The url from where the data comes
    pub(crate)  source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelineToggleStatement {
    /// The object name to where the data is heading
    pub(crate)  pipeline: ObjectName,
    /// The url from where the data comes
    pub(crate)  toggle: PipelineToggle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlterTableAddColumn {
    pub(crate) table_name: ObjectName,
    pub(crate) column_name: Ident,
    pub(crate) data_type: DataType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlterTableDropColumn {
    pub(crate) table_name: ObjectName,
    pub(crate) column_name: Ident,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlterTableAlterColumnType {
    pub(crate) table_name: ObjectName,
    pub(crate) column_name: Ident,
    pub(crate) new_type: DataType,
    pub(crate) values_new_type: Option<DataType>,

}

/// Skippr SQL Statement.
///
/// This can either be a [`Statement`] from [`DFParser`] or [`sqlparser`] from a
/// standard SQL dialect, or a Skippr extension such as `SCHEMA DUMP,
/// SCHMEA LOAD`. See [`Sparser`] for more information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// ANSI SQL AST node (from sqlparser-rs)
    // Statement(Box<Statement>),
    /// Extension: `SCHEMA DUMP`
    SchemaDump(SchemaDumpStatement),
    SchemaDrop(SchemaDropStatement),
    PipelineDrop(PipelineDropStatement),
    PipelineReset(PipelineResetStatement),
    /// Extension: `SCHEMA LOAD`
    SchemaLoad(SchemaLoadStatement),
    PipelineToggle(PipelineToggleStatement),
    // AlterTableAddColumn(AlterTableAddColumn),
    AlterTableDropColumn(AlterTableDropColumn),
    AlterTableAlterColumnType(AlterTableAlterColumnType),
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

    pub fn new_with_dialect(
        sql: &str,
        dialect: &'a dyn Dialect,
    ) -> Result<Self, ParserError> {
        let mut tokenizer = Tokenizer::new(dialect, sql);
        let tokens = tokenizer.tokenize()?;

        Ok(SParser {
            parser: Parser::new(dialect).with_tokens(tokens),
        })
    }

    fn parse_sql(sql: &str) -> Result<VecDeque<Statement>, ParserError> {
        let dialect = &GenericDialect {};
        SParser::parse_sql_with_dialect(sql, dialect)
    }

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
                return parser.parser.expected("end of statement", parser.parser.peek_token());
            }

            let statement = parser.parse_statement()?;
            stmts.push_back(statement);
            expecting_statement_delimiter = true;
        }
        Ok(stmts)
    }

    pub fn parse_statement(&mut self) -> Result<Statement, ParserError> {
        // match self.parser.peek_token().token {
        //     Token::Word(w) => {
        //         match w.keyword {
        //             Keyword::DESCRIBE => {
        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::DUMP) => {
                        self.parser.next_token(); // DUMP
                        self.parse_dump()
                    }
                    Some(SkipprKeyword::DROP) => {
                        self.parser.next_token(); // DROP
                        self.parse_drop()
                    }
                    Some(SkipprKeyword::RESET) => {
                        self.parser.next_token(); // RESET
                        self.parse_reset()
                    }
                    Some(SkipprKeyword::LOAD) => {
                        self.parser.next_token(); // LOAD
                        self.parse_load()
                    }
                    Some(SkipprKeyword::ENABLE) => {
                        self.parser.next_token(); // ENABLE
                        self.parse_enable()
                    }
                    Some(SkipprKeyword::DISABLE) => {
                        self.parser.next_token(); // DISABLE
                        self.parse_disable()
                    }
                    None => {
                        match self.parser.peek_token().token {
                            Token::Word(w) => {
                                match w.keyword {
                                    Keyword::ALTER => {
                                        self.parser.next_token(); // ALTER

                                        match self.parser.peek_token().token {
                                            Token::Word(w) => {
                                                match w.keyword {
                                                    Keyword::SCHEMA => {
                                                        self.parser.next_token(); // SCHEMA
                                                        self.parse_alter_schema()
                                                    }
                                                    _ => {
                                                        Err(ParserError::ParserError("Not implemented".to_string()))
                                                    }
                                                }
                                            }
                                            _ => {
                                                Err(ParserError::ParserError("Not implemented".to_string()))
                                            }
                                        }
                                    }
                                    _ => {
                                        // use sqlparser-rs parser
                                        // Ok(Statement::Statement(Box::from(
                                        //     self.parser.parse_statement()?,
                                        // )))
                                        Err(ParserError::ParserError("Not implemented".to_string()))
                                    }
                                }
                            }
                            _ => {
                                // use sqlparser-rs parser
                                // Ok(Statement::Statement(Box::from(
                                //     self.parser.parse_statement()?,
                                // )))
                                Err(ParserError::ParserError("Not implemented".to_string()))
                            }
                        }
                    },
                    _ => {
                        Err(ParserError::ParserError("Not implemented".to_string()))
                    }

                }
            }
            _ => {
                // use the native parser
                // Ok(Statement::Statement(Box::from(
                //     self.parser.parse_statement()?,
                // )))
                Err(ParserError::ParserError("Not implemented".to_string()))
            }
        }
    }

    // This is a simplified sketch and needs to be integrated with your existing parsing logic.

    pub fn parse_alter_schema(&mut self) -> Result<Statement, ParserError> {
        let table_name = self.parser.parse_object_name()?;

        // self.parser.next_token(); // TABLE
        // self.parser.expect_keyword(Keyword::ALTER)?;
        // self.parser.next_token(); // ALTER
        // self.parser.expect_keyword(Keyword::COLUMN)?;
        // self.parser.next_token(); // COLUMN

        // println!("table_name {}", table_name.to_string());
        // println!("next {}", self.parser.peek_token().to_string());

        match self.parser.next_token().token {
            Token::Word(w) => match w.keyword {
                Keyword::ADD => {
                    Err(ParserError::ParserError("ALTER COLUMN ADD Not implemented".to_string()))
                },
                Keyword::DROP => {
                    self.parser.expect_keyword(Keyword::COLUMN)?;
                    let column_name = self.parser.parse_identifier()?;
                    Ok(Statement::AlterTableDropColumn(AlterTableDropColumn {
                        table_name,
                        column_name,
                    }))
                },
                Keyword::ALTER => {
                    self.parser.expect_keyword(Keyword::COLUMN)?;
                    let column_name = self.parser.parse_identifier()?;

                    self.parser.expect_keyword(Keyword::TYPE)?;

                    let column_new_type = self.parser.parse_data_type()?;

                    match column_new_type  {
                        DataType::Array(value_type) => {
                            match value_type {
                                ArrayElemTypeDef::AngleBracket(value) => {

                                    Ok(Statement::AlterTableAlterColumnType(AlterTableAlterColumnType {
                                        table_name,
                                        column_name,
                                        // strip the <value type> from ARRAY<value type> to support matching against `SkipprTypes`
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
                            Ok(Statement::AlterTableAlterColumnType(AlterTableAlterColumnType {
                                table_name,
                                column_name,
                                new_type: column_new_type,
                                values_new_type: None
                            }))
                        }
                    }

                },
                _ => Err(ParserError::ParserError("Unexpected keyword".to_string())),
            },
            _ => Err(ParserError::ParserError("Expected keyword after ALTER TABLE".to_string())),
        }
    }

    pub fn parse_enable(&mut self) -> Result<Statement, ParserError> {
        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::PIPELINE) => {
                        self.parser.next_token(); // PIPELINE

                        let pipeline = self.parser.parse_object_name()?;

                        Ok(Statement::PipelineToggle(PipelineToggleStatement {
                            pipeline,
                            toggle: PipelineToggle::from_str("ENABLE").unwrap()

                        }))

                    },
                    _ => {
                        Err(ParserError::ParserError("Not implemented".to_string()))
                    }
                }
            },
            _ => {
                Err(ParserError::ParserError("Unknown error".to_string()))
            }
        }
    }

    pub fn parse_disable(&mut self) -> Result<Statement, ParserError> {
        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::PIPELINE) => {
                        self.parser.next_token(); // PIPELINE

                        let pipeline = self.parser.parse_object_name()?;

                        Ok(Statement::PipelineToggle(PipelineToggleStatement {
                            pipeline,
                            toggle: PipelineToggle::from_str("DISABLE").unwrap()
                        }))

                    },
                    _ => {
                        Err(ParserError::ParserError("Not implemented".to_string()))
                    }
                }
            },
            _ => {
                Err(ParserError::ParserError("Unknown error".to_string()))
            }
        }
    }

    pub fn parse_load(&mut self) -> Result<Statement, ParserError> {
        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::SCHEMA) => {

                        Err(ParserError::ParserError("Not implemented - LOAD SCHEMA not currently supported".to_string()))

                        // @todo - this functions, but we need to think about how to serialise the dump

                        // self.parser.next_token(); // SCHEMA
                        //
                        // let source = self.parser.parse_literal_string()?;
                        //
                        // self.parser.expect_keyword(Keyword::INTO)?;
                        //
                        // let table_name = self.parser.parse_object_name()?;
                        //
                        // // println!("source: {}", source);
                        // // println!("table_name: {}", table_name);
                        //
                        // Ok(Statement::SchemaLoad(SchemaLoadStatement {
                        //     table: SchemaLoadDest::Relation(table_name),
                        //     source,
                        // }))

                    },
                    _ => {
                        Err(ParserError::ParserError("Not implemented".to_string()))
                    }
                }
            },
            _ => {
                Err(ParserError::ParserError("Unknown error".to_string()))
            }
        }
    }

    pub fn parse_dump(&mut self) -> Result<Statement, ParserError> {

        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::SCHEMA) => {

                        // @todo - this functions, but we need to think about how to serialise the dump

                        self.parser.next_token(); // SCHEMA

                        let table_name = self.parser.parse_object_name()?;

                        self.parser.expect_keyword(Keyword::TO)?;

                        let target = self.parser.parse_literal_string()?;

                        // println!("target: {}", target);
                        // println!("table_name: {}", table_name);

                        Ok(Statement::SchemaDump(SchemaDumpStatement {
                            table: SchemaDumpSource::Relation(table_name),
                            target
                        }))

                    }
                    _ => {
                        Err(ParserError::ParserError("Not implemented".to_string()))
                    }
                }
            },
            _ => {
                Err(ParserError::ParserError("Unknown error".to_string()))
            }
        }
    }

    pub fn parse_reset(&mut self) -> Result<Statement, ParserError> {

        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::PIPELINE) => {

                        self.parser.next_token(); // PIPELINE

                        let table_name = self.parser.parse_object_name()?;

                        Ok(Statement::PipelineReset(PipelineResetStatement {
                            table: table_name
                        }))

                    }
                    _ => {
                        Err(ParserError::ParserError("Not implemented".to_string()))
                    }
                }
            },
            _ => {
                Err(ParserError::ParserError("Unknown error".to_string()))
            }
        }
    }

    pub fn parse_drop(&mut self) -> Result<Statement, ParserError> {

        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value) {
                    Some(SkipprKeyword::SCHEMA) => {

                        self.parser.next_token(); // SCHEMA

                        let table_name = self.parser.parse_object_name()?;

                        Ok(Statement::SchemaDrop(SchemaDropStatement {
                            table: table_name
                        }))

                    }
                    Some(SkipprKeyword::PIPELINE) => {

                        self.parser.next_token(); // PIPELINE

                        let table_name = self.parser.parse_object_name()?;

                        Ok(Statement::PipelineDrop(PipelineDropStatement {
                            table: table_name
                        }))

                    }
                    _ => {
                        Err(ParserError::ParserError("Not implemented".to_string()))
                    }
                }
            },
            _ => {
                Err(ParserError::ParserError("Unknown error".to_string()))
            }
        }
    }

}

