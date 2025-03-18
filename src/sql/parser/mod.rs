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
use icu::properties::sets::print;
use indexmap::Equivalent;
use sqlparser::ast::{ArrayElemTypeDef, DataType, Ident};
use sqlparser::tokenizer::Token::EOF;

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
    pub(crate) pipeline: ObjectName,
    pub(crate) schema: Option<ObjectName>,
    /// The URL to where the data is heading
    pub(crate) target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaDropStatement {
    pub(crate) pipeline: ObjectName,
    pub(crate) schema: Option<ObjectName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatabaseDropStatement {
    pub(crate) database: ObjectName,
}


#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelineDropStatement {
    pub(crate) pipeline: ObjectName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelineResetStatement {
    pub(crate) pipeline: ObjectName,
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
    pub(crate) pipeline: SchemaLoadDest,
    /// The url from where the data comes
    pub(crate) source: String,
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
pub(crate) struct AlterSchemaDropColumn {
    pub(crate) pipeline: ObjectName,
    pub(crate) schema: Option<ObjectName>,
    // support field names with dots representing nested fields (e.g. foo.bar.baz), hence ObjectName instead of Ident
    pub(crate) column_name: ObjectName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlterSchemaAlterColumnType {
    pub(crate) pipeline: ObjectName,
    pub(crate) schema: Option<ObjectName>,
    // support field names with dots representing nested fields (e.g. foo.bar.baz), hence ObjectName instead of Ident
    pub(crate) column_name: ObjectName,
    pub(crate) new_type: DataType,
    pub(crate) values_new_type: Option<DataType>,

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableDropStatement {
    pub(crate) schema: Option<ObjectName>,
    pub(crate) table: ObjectName,
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
    DatabaseDrop(DatabaseDropStatement),
    SchemaDrop(SchemaDropStatement),
    PipelineDrop(PipelineDropStatement),
    PipelineReset(PipelineResetStatement),
    /// Extension: `SCHEMA LOAD`
    SchemaLoad(SchemaLoadStatement),
    PipelineToggle(PipelineToggleStatement),
    // AlterTableAddColumn(AlterTableAddColumn),
    AlterSchemaDropColumn(AlterSchemaDropColumn),
    AlterSchemaAlterColumnType(AlterSchemaAlterColumnType),
    TableDrop(TableDropStatement),
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
                    Some(SkipprKeyword::TABLE) => {
                        self.parser.next_token(); // TABLE
                        self.parse_drop()
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

        let pipeline = self.parser.next_token().token.to_string();

        let schema = match self.parser.peek_token().token.to_string().as_str() {
            "." => {
                self.parser.next_token(); // .
                let schema = self.parser.next_token().token.to_string();
                Some(ObjectName(vec![Ident::new(schema)]))
            },
            _ => {
                None
            }
        };

        match self.parser.next_token().token {
            Token::Word(w) => match w.keyword {
                Keyword::ADD => {
                    Err(ParserError::ParserError("ALTER COLUMN ADD Not implemented".to_string()))
                },
                Keyword::DROP => {
                    self.parser.expect_keyword(Keyword::COLUMN)?;
                    let column_name = self.parser.parse_object_name(false)?;
                    Ok(Statement::AlterSchemaDropColumn(AlterSchemaDropColumn {
                        pipeline: ObjectName(vec![Ident::new(pipeline)]),
                        schema,
                        column_name,
                    }))
                },
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
                                        pipeline: ObjectName(vec![Ident::new(pipeline)]),
                                        schema,
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
                            Ok(Statement::AlterSchemaAlterColumnType(AlterSchemaAlterColumnType {
                                pipeline: ObjectName(vec![Ident::new(pipeline)]),
                                schema,
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

                        let pipeline = self.parser.parse_object_name(true)?;

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

                        let pipeline = self.parser.parse_object_name(true)?;

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
                        
                        self.parser.next_token(); // SCHEMA
                        
                        let pipeline = self.parser.next_token().token.to_string();

                        let schema = match self.parser.peek_token().token.to_string().as_str() {
                            "." => {
                                self.parser.next_token(); // .
                                let schema = self.parser.next_token().token.to_string();
                                Some(schema)
                            },
                            _ => {
                                None
                            }
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
                            pipeline: ObjectName(vec![Ident::new(pipeline)]),
                            schema: schema.map(|s| ObjectName(vec![Ident::new(s)])),
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

                        let table_name = self.parser.parse_object_name(true)?;

                        Ok(Statement::PipelineReset(PipelineResetStatement {
                            pipeline: table_name
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

                        let pipeline = self.parser.next_token().token.to_string();

                        let schema = match self.parser.peek_token().token.to_string().as_str() {
                            "." => {
                                self.parser.next_token(); // .
                                let schema = self.parser.next_token().token.to_string();
                                Some(schema)
                            },
                            _ => {
                                None
                            }
                        };

                        Ok(Statement::SchemaDrop(SchemaDropStatement {
                            pipeline: ObjectName(vec![Ident::new(pipeline)]),
                            schema: schema.map(|s| ObjectName(vec![Ident::new(s)]))
                        }))
                        // }

                    }
                    Some(SkipprKeyword::PIPELINE) => {

                        self.parser.next_token(); // PIPELINE

                        let table_name = self.parser.parse_object_name(true)?;

                        Ok(Statement::PipelineDrop(PipelineDropStatement {
                            pipeline: table_name
                        }))

                    }
                    Some(SkipprKeyword::DATABASE) => {

                        self.parser.next_token(); // DATABASE

                        let database = self.parser.parse_object_name(false)?;
                        
                        Ok(Statement::DatabaseDrop(DatabaseDropStatement {
                            database
                        }))
                    }
                    Some(SkipprKeyword::TABLE) => {
                        self.parser.next_token(); // TABLE

                        // Parse table name which might be in the format schema.table
                        let object_name = self.parser.parse_object_name(false)?;
                        
                        // If the object name has multiple parts, it's in the format schema.table
                        if object_name.0.len() > 1 {
                            let schema = ObjectName(vec![object_name.0[0].clone()]);
                            let table = ObjectName(vec![object_name.0[1].clone()]);
                            
                            Ok(Statement::TableDrop(TableDropStatement {
                                schema: Some(schema),
                                table
                            }))
                        } else {
                            // No schema specified
                            Ok(Statement::TableDrop(TableDropStatement {
                                schema: None,
                                table: object_name
                            }))
                        }
                    }
                    _ => {
                        Err(ParserError::ParserError("Not implemented".to_string()))
                    },
                }
            },
            _ => {
                Err(ParserError::ParserError("Unknown error".to_string()))
            }
        }
    }

}

