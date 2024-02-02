use sqlparser::dialect::GenericDialect;
use std::collections::VecDeque;
use datafusion::sql::parser::{CopyToSource, CopyToStatement, CreateExternalTable, DescribeTableStmt, DFParser, ExplainStatement};
use datafusion::sql::{parser, sqlparser};
use datafusion::sql::sqlparser::ast::{ObjectName, Query, Value};
use datafusion::sql::sqlparser::dialect::Dialect;
use datafusion::sql::sqlparser::keywords::Keyword;
use datafusion::sql::sqlparser::parser::{Parser, ParserError};
use datafusion::sql::sqlparser::tokenizer::{Token, Tokenizer};


// Keywords used in Skippr SQL
// Defined as a separate enum to avoid conflicts with `sqlparser::ast::Keyword`
// Extending sqlparser define_keywords! macro would be nicer? Not possible in any case and maintainers are not
// interested (understandably) in endlessly adding support for esoteric dialects
enum SkipprKeyword {
    DUMP,
    LOAD
}

impl SkipprKeyword {

    fn from_str(s: &str) -> Option<SkipprKeyword> {
        match s.to_uppercase().as_str() {
            "DUMP" => Some(SkipprKeyword::DUMP),
            "LOAD" => Some(SkipprKeyword::LOAD),
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
    /// From where the data comes from
    pub table: SchemaDumpSource,
    /// The URL to where the data is heading
    pub target: String,
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
    /// The object name to where the data is heading
    pub table: SchemaLoadDest,
    /// The url from where the data comes
    pub source: String,
}

/// Skippr SQL Statement.
///
/// This can either be a [`Statement`] from [`DFParser`] or [`sqlparser`] from a
/// standard SQL dialect, or a Skippr extension such as `SCHEMA DUMP,
/// SCHMEA LOAD`. See [`Sparser`] for more information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// ANSI SQL AST node (from sqlparser-rs)
    Statement(Box<Statement>),
    /// Extension: `SCHEMA DUMP`
    SchemaDump(SchemaDumpStatement),
    /// Extension: `SCHEMA LOAD`
    SchemaLoad(SchemaLoadStatement),
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
        match self.parser.peek_token().token {
            Token::Word(w) => {
                match w.keyword {
                    Keyword::SCHEMA => {
                        self.parser.next_token(); // SCHEMA
                        self.parse_schema()
                    }
                    _ => {
                        // use sqlparser-rs parser
                        // Ok(Statement::Statement(Box::from(
                        //     self.parser.parse_statement()?,
                        // )))
                        Err(ParserError::ParserError("Not implemented - todo - gracefully fallback to DFParser".to_string()))
                    }
                }
            }
            _ => {
                // use the native parser
                // Ok(Statement::Statement(Box::from(
                //     self.parser.parse_statement()?,
                // )))
                Err(ParserError::ParserError("Not implemented - todo - gracefully fallback to DFParser".to_string()))
            }
        }
    }

    pub fn parse_schema(&mut self) -> Result<Statement, ParserError> {

        return match self.parser.peek_token().token {
            Token::Word(w) => {
                match SkipprKeyword::from_str(&w.value.to_uppercase()) {
                    Some(SkipprKeyword::DUMP) => {
                        self.parser.next_token(); // DUMP

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
                    Some(SkipprKeyword::LOAD) => {
                        self.parser.next_token(); // LOAD

                        let source = self.parser.parse_literal_string()?;

                        self.parser.expect_keyword(Keyword::INTO)?;

                        let table_name = self.parser.parse_object_name()?;

                        // println!("source: {}", source);
                        // println!("table_name: {}", table_name);

                        Ok(Statement::SchemaLoad(SchemaLoadStatement {
                            table: SchemaLoadDest::Relation(table_name),
                            source,
                        }))

                    },
                    _ => {
                        Err(ParserError::ParserError("Not implemented - todo - gracefully fallback to DFParser".to_string()))
                    }
                }
            },
            _ => {
                Err(ParserError::ParserError("Unknown error".to_string()))
            }
        }
    }

}

