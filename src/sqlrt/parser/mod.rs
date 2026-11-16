use crate::compute::{ComputeAlteration, Fqn, UnboundMeasureKind};
use crate::discover::schema_alter::SchemaAlterOp;
use crate::discover::SkipprDataType;
use core::fmt;
use datafusion::sql::sqlparser::ast::{
    DataType, Ident, ObjectName, ObjectNamePart, SetExpr, Statement as StdStatement, TableFactor,
};
use datafusion::sql::sqlparser::dialect::{Dialect, GenericDialect};
use datafusion::sql::sqlparser::keywords::Keyword;
use datafusion::sql::sqlparser::parser::{Parser, ParserError};
use datafusion::sql::sqlparser::tokenizer::{Token, Tokenizer};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterTableStatement {
    pub pipeline: ObjectName,
    pub namespace: Option<ObjectName>,
    pub op: SchemaAlterOp,
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
    AlterTable(AlterTableStatement),
    TableDrop(TableDropStatement),
    CreateView {
        name: Fqn,
        as_select: String,
    },
    CreateTableAs {
        name: Fqn,
        as_select: String,
    },
    DropCompute {
        name: Fqn,
    },
    AlterCompute {
        name: Fqn,
        alterations: Vec<ComputeAlteration>,
    },
    Stream(StreamStatement),
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
    source: &'a str,
}

/// `STREAM <select> FROM <pipeline>.<namespace> [WINDOW <seconds>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStatement {
    pub select_sql: String,
    pub from: String,
    pub window_secs: Option<i64>,
}

impl<'a> SParser<'a> {
    pub fn new(sql: &'a str) -> Result<Self, ParserError> {
        let dialect = &GenericDialect {};
        SParser::new_with_dialect(sql, dialect)
    }

    pub fn new_with_dialect(sql: &'a str, dialect: &'a dyn Dialect) -> Result<Self, ParserError> {
        let mut tokenizer = Tokenizer::new(dialect, sql);
        let tokens = tokenizer.tokenize()?;

        Ok(SParser {
            parser: Parser::new(dialect).with_tokens(tokens),
            source: sql,
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
                if w.value.eq_ignore_ascii_case("STREAM") {
                    return self.parse_stream();
                }
                if w.value.to_uppercase() == "CREATE" {
                    self.parser.next_token();
                    return self.parse_create();
                }
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
                                                        self.parse_alter_table()
                                                    }
                                                    _ if w3
                                                        .value
                                                        .eq_ignore_ascii_case("COMPUTE") =>
                                                    {
                                                        self.parser.next_token();
                                                        self.parse_alter_compute()
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

    fn parse_stream(&mut self) -> Result<Statement, ParserError> {
        self.parser.next_token();
        let trimmed = self.source.trim();
        let upper_sql = trimmed.to_ascii_uppercase();
        if !upper_sql.starts_with("STREAM ") {
            return Err(ParserError::ParserError(
                "STREAM requires a select and FROM <pipeline>.<namespace>".into(),
            ));
        }
        let mut body = trimmed[6..].trim().to_string();
        let mut window_secs = None;
        let upper = body.to_ascii_uppercase();
        if let Some(idx) = upper.find(" WINDOW ") {
            let start = idx + 8;
            let bytes = body.as_bytes();
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let num_start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > num_start {
                window_secs = body[num_start..j].parse::<i64>().ok();
                let left = body[..idx].trim_end().to_string();
                let right = body[j..].trim_start().to_string();
                body = if right.is_empty() {
                    left
                } else {
                    format!("{left} {right}")
                };
            }
        }
        let select_sql = format!("SELECT {body}");
        let dialect = GenericDialect {};
        let ast = Parser::parse_sql(&dialect, &select_sql)
            .map_err(|err| ParserError::ParserError(format!("STREAM select: {err}")))?;
        let mut from = None;
        for stmt in ast {
            if let StdStatement::Query(query) = stmt {
                if let SetExpr::Select(select) = &*query.body {
                    if let Some(table) = select.from.first() {
                        if let TableFactor::Table { name, .. } = &table.relation {
                            from = Some(name.to_string());
                        }
                    }
                }
            }
        }
        let from = from.ok_or_else(|| {
            ParserError::ParserError("STREAM requires a FROM <pipeline>.<namespace>".into())
        })?;
        Ok(Statement::Stream(StreamStatement {
            select_sql,
            from,
            window_secs,
        }))
    }

    pub fn parse_alter_table(&mut self) -> Result<Statement, ParserError> {
        let pipeline = token_ident(&self.parser.next_token().token);

        let namespace = match self.parser.peek_token().token.to_string().as_str() {
            "." => {
                self.parser.next_token();
                let namespace = token_ident(&self.parser.next_token().token);
                Some(ObjectName(vec![ObjectNamePart::Identifier(Ident::new(
                    namespace,
                ))]))
            }
            _ => None,
        };

        if self.peek_compute_alter() {
            let name = match &namespace {
                Some(ns) => Fqn::from_parts(&pipeline, &token_ident_from_object(ns)?)
                    .map_err(ParserError::ParserError)?,
                None => {
                    return Err(ParserError::ParserError(
                        "ALTER TABLE compute sugar requires pipeline.name".into(),
                    ))
                }
            };
            let alterations = self.parse_compute_alterations()?;
            return Ok(Statement::AlterCompute { name, alterations });
        }

        match self.parser.next_token().token {
            Token::Word(w) => {
                let pipeline = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(pipeline))]);
                match w.keyword {
                    Keyword::DROP => {
                        self.parser.expect_keyword(Keyword::COLUMN)?;
                        let column = object_name_to_column(&self.parser.parse_object_name(false)?)?;
                        Ok(Statement::AlterTable(AlterTableStatement {
                            pipeline,
                            namespace,
                            op: SchemaAlterOp::Drop { column },
                        }))
                    }
                    Keyword::ALTER => {
                        self.parser.expect_keyword(Keyword::COLUMN)?;
                        let column = object_name_to_column(&self.parser.parse_object_name(false)?)?;
                        self.parser.expect_keyword(Keyword::TYPE)?;
                        let parsed = self.parser.parse_data_type()?;
                        let to = skippr_type_from_sql(&parsed)?;
                        Ok(Statement::AlterTable(AlterTableStatement {
                            pipeline,
                            namespace,
                            op: SchemaAlterOp::Promote { column, to },
                        }))
                    }
                    Keyword::RENAME => {
                        self.parser.expect_keyword(Keyword::COLUMN)?;
                        let from = object_name_to_column(&self.parser.parse_object_name(false)?)?;
                        self.parser.expect_keyword(Keyword::TO)?;
                        let to = object_name_to_column(&self.parser.parse_object_name(false)?)?;
                        Ok(Statement::AlterTable(AlterTableStatement {
                            pipeline,
                            namespace,
                            op: SchemaAlterOp::Rename { from, to },
                        }))
                    }
                    _ if w.value.eq_ignore_ascii_case("MERGE") => {
                        self.parser.expect_keyword(Keyword::COLUMN)?;
                        let src = object_name_to_column(&self.parser.parse_object_name(false)?)?;
                        if !self.parser.parse_keyword(Keyword::INTO) {
                            return Err(ParserError::ParserError(
                                "Expected INTO after MERGE COLUMN".to_string(),
                            ));
                        }
                        let dst = object_name_to_column(&self.parser.parse_object_name(false)?)?;
                        Ok(Statement::AlterTable(AlterTableStatement {
                            pipeline,
                            namespace,
                            op: SchemaAlterOp::Merge { src, dst },
                        }))
                    }
                    Keyword::ADD => Err(ParserError::ParserError(
                        "ALTER TABLE ADD COLUMN is not implemented; discover adds columns"
                            .to_string(),
                    )),
                    _ => Err(ParserError::ParserError(
                        "Expected DROP COLUMN, RENAME COLUMN, MERGE COLUMN, or ALTER COLUMN"
                            .to_string(),
                    )),
                }
            }
            _ => Err(ParserError::ParserError(
                "Expected keyword after ALTER TABLE".to_string(),
            )),
        }
    }

    fn parse_alter_compute(&mut self) -> Result<Statement, ParserError> {
        let name = parse_compute_name(&self.parser.parse_object_name(true)?)?;
        let alterations = self.parse_compute_alterations()?;
        Ok(Statement::AlterCompute { name, alterations })
    }

    fn peek_compute_alter(&mut self) -> bool {
        match &self.parser.peek_nth_token(0).token {
            Token::Word(w) if w.keyword == Keyword::ALTER => {
                word_eq(&self.parser.peek_nth_token(1).token, "COLUMN")
                    && word_eq(&self.parser.peek_nth_token(3).token, "SET")
                    && (word_eq(&self.parser.peek_nth_token(4).token, "MEASURE")
                        || word_eq(&self.parser.peek_nth_token(4).token, "FILTER"))
            }
            Token::Word(w) if w.keyword == Keyword::ADD => {
                word_eq(&self.parser.peek_nth_token(1).token, "COLUMN")
                    && word_eq(&self.parser.peek_nth_token(3).token, "MEASURE")
            }
            _ => false,
        }
    }

    fn parse_compute_alterations(&mut self) -> Result<Vec<ComputeAlteration>, ParserError> {
        let mut out = Vec::new();
        loop {
            out.push(self.parse_one_compute_alteration()?);
            match self.parser.peek_token().token {
                Token::Comma => {
                    self.parser.next_token();
                }
                Token::EOF | Token::SemiColon => break,
                _ => break,
            }
        }
        if out.is_empty() {
            return Err(ParserError::ParserError(
                "ALTER COMPUTE requires at least one alteration".into(),
            ));
        }
        Ok(out)
    }

    fn parse_one_compute_alteration(&mut self) -> Result<ComputeAlteration, ParserError> {
        match self.parser.next_token().token {
            Token::Word(w) if w.keyword == Keyword::DROP => {
                self.parser.expect_keyword(Keyword::COLUMN)?;
                let column = object_name_to_column(&self.parser.parse_object_name(false)?)?;
                Ok(ComputeAlteration::DropColumn { column })
            }
            Token::Word(w) if w.keyword == Keyword::RENAME => {
                self.parser.expect_keyword(Keyword::COLUMN)?;
                let from = object_name_to_column(&self.parser.parse_object_name(false)?)?;
                self.parser.expect_keyword(Keyword::TO)?;
                let to = object_name_to_column(&self.parser.parse_object_name(false)?)?;
                Ok(ComputeAlteration::RenameColumn { from, to })
            }
            Token::Word(w) if w.keyword == Keyword::ADD => {
                self.parser.expect_keyword(Keyword::COLUMN)?;
                let name = object_name_to_column(&self.parser.parse_object_name(false)?)?;
                expect_word(&mut self.parser, "MEASURE")?;
                let kind = parse_measure_kind(&mut self.parser)?;
                Ok(ComputeAlteration::AddMeasure { name, kind })
            }
            Token::Word(w) if w.keyword == Keyword::ALTER => {
                self.parser.expect_keyword(Keyword::COLUMN)?;
                let column = object_name_to_column(&self.parser.parse_object_name(false)?)?;
                expect_word(&mut self.parser, "SET")?;
                match self.parser.next_token().token {
                    Token::Word(w) if w.value.eq_ignore_ascii_case("MEASURE") => {
                        let kind = parse_measure_kind(&mut self.parser)?;
                        Ok(ComputeAlteration::SetMeasure { column, kind })
                    }
                    Token::Word(w) if w.value.eq_ignore_ascii_case("FILTER") => {
                        let expr = self.parser.parse_expr()?.to_string();
                        Ok(ComputeAlteration::SetFilter { expr })
                    }
                    other => Err(ParserError::ParserError(format!(
                        "expected SET MEASURE or SET FILTER, got {other:?}"
                    ))),
                }
            }
            other => Err(ParserError::ParserError(format!(
                "expected ALTER COMPUTE alteration, got {other:?}"
            ))),
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
                            pipeline: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(
                                pipeline,
                            ))]),
                            schema: schema.map(|s| {
                                ObjectName(vec![ObjectNamePart::Identifier(Ident::new(s))])
                            }),
                            target,
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
                        self.parser.next_token();
                        let name = parse_fqn_object(&self.parser.parse_object_name(true)?)?;
                        Ok(Statement::DropCompute { name })
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

    fn parse_create(&mut self) -> Result<Statement, ParserError> {
        match self.parser.peek_token().token {
            Token::Word(w) => match w.value.to_uppercase().as_str() {
                "INDEX" => Err(ParserError::ParserError(
                    "CREATE INDEX is not Skippr SQL; use DISTINCT ON or GROUP BY".into(),
                )),
                "SINK" => Err(ParserError::ParserError(
                    "CREATE SINK is not Skippr SQL".into(),
                )),
                "VIEW" => {
                    self.parser.next_token();
                    let name = parse_fqn_object(&self.parser.parse_object_name(true)?)?;
                    self.parser.expect_keyword(Keyword::AS)?;
                    let as_select = self.rest_sql()?;
                    if as_select.trim().is_empty() {
                        return Err(ParserError::ParserError(
                            "CREATE VIEW requires AS SELECT".into(),
                        ));
                    }
                    Ok(Statement::CreateView { name, as_select })
                }
                "TABLE" => {
                    self.parser.next_token();
                    let name = parse_fqn_object(&self.parser.parse_object_name(true)?)?;
                    match self.parser.peek_token().token {
                        Token::Word(w) if w.keyword == Keyword::AS => {
                            self.parser.next_token();
                            let as_select = self.rest_sql()?;
                            if as_select.trim().is_empty() {
                                return Err(ParserError::ParserError(
                                    "CREATE TABLE requires AS SELECT".into(),
                                ));
                            }
                            Ok(Statement::CreateTableAs { name, as_select })
                        }
                        _ => Err(ParserError::ParserError(
                            "empty CREATE TABLE is not Skippr SQL; use CREATE TABLE AS SELECT"
                                .into(),
                        )),
                    }
                }
                other => Err(ParserError::ParserError(format!(
                    "CREATE {other} is not Skippr SQL"
                ))),
            },
            _ => Err(ParserError::ParserError(
                "CREATE requires VIEW or TABLE".into(),
            )),
        }
    }

    fn rest_sql(&mut self) -> Result<String, ParserError> {
        let query = self.parser.parse_query()?;
        Ok(query.to_string())
    }
}

fn parse_fqn_object(name: &ObjectName) -> Result<Fqn, ParserError> {
    let parts: Vec<String> = name
        .0
        .iter()
        .filter_map(|p| p.as_ident().map(|id| id.value.clone()))
        .collect();
    match parts.as_slice() {
        [pipeline, ident] => Fqn::from_parts(pipeline, ident).map_err(ParserError::ParserError),
        _ => Err(ParserError::ParserError(
            "name must be pipeline.name".into(),
        )),
    }
}

fn parse_compute_name(name: &ObjectName) -> Result<Fqn, ParserError> {
    let parts: Vec<String> = name
        .0
        .iter()
        .filter_map(|p| p.as_ident().map(|id| id.value.clone()))
        .collect();
    match parts.as_slice() {
        [pipeline, ident] => Fqn::from_parts(pipeline, ident).map_err(ParserError::ParserError),
        [ident] => Fqn::from_parts(Fqn::BARE, ident).map_err(ParserError::ParserError),
        _ => Err(ParserError::ParserError(
            "ALTER COMPUTE name must be ident or pipeline.ident".into(),
        )),
    }
}

fn token_ident_from_object(name: &ObjectName) -> Result<String, ParserError> {
    object_name_to_column(name)
}

fn word_eq(token: &Token, want: &str) -> bool {
    match token {
        Token::Word(w) => w.value.eq_ignore_ascii_case(want),
        _ => false,
    }
}

fn expect_word(parser: &mut Parser<'_>, word: &str) -> Result<(), ParserError> {
    match parser.next_token().token {
        Token::Word(w) if w.value.eq_ignore_ascii_case(word) => Ok(()),
        other => Err(ParserError::ParserError(format!(
            "expected {word}, got {other:?}"
        ))),
    }
}

fn parse_measure_kind(parser: &mut Parser<'_>) -> Result<UnboundMeasureKind, ParserError> {
    match parser.next_token().token {
        Token::Word(w) if w.value.eq_ignore_ascii_case("COUNT") => {
            parser.expect_token(&Token::LParen)?;
            match parser.next_token().token {
                Token::Mul | Token::Word(_) => {}
                other => {
                    return Err(ParserError::ParserError(format!(
                        "COUNT expects * or a column, got {other:?}"
                    )))
                }
            }
            parser.expect_token(&Token::RParen)?;
            Ok(UnboundMeasureKind::Count)
        }
        Token::Word(w) => {
            parser.expect_token(&Token::LParen)?;
            let col = object_name_to_column(&parser.parse_object_name(false)?)?;
            parser.expect_token(&Token::RParen)?;
            match w.value.to_ascii_uppercase().as_str() {
                "SUM" => Ok(UnboundMeasureKind::Sum(col)),
                "MIN" => Ok(UnboundMeasureKind::Min(col)),
                "MAX" => Ok(UnboundMeasureKind::Max(col)),
                other => Err(ParserError::ParserError(format!("unknown measure {other}"))),
            }
        }
        other => Err(ParserError::ParserError(format!(
            "expected measure function, got {other:?}"
        ))),
    }
}

fn token_ident(token: &Token) -> String {
    match token {
        Token::Word(word) => word.value.clone(),
        other => other.to_string().replace('"', ""),
    }
}

fn object_name_to_column(name: &ObjectName) -> Result<String, ParserError> {
    if name.0.is_empty() {
        return Err(ParserError::ParserError("Expected column name".to_string()));
    }
    Ok(name
        .0
        .iter()
        .map(|part| part.to_string())
        .collect::<Vec<_>>()
        .join("."))
}

fn skippr_type_from_sql(data_type: &DataType) -> Result<SkipprDataType, ParserError> {
    let rendered = data_type.to_string();
    SkipprDataType::from_string(&rendered)
        .or_else(|| match data_type {
            DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => {
                Some(SkipprDataType::Integer)
            }
            DataType::BigInt(_) | DataType::Int8(_) => Some(SkipprDataType::Long),
            DataType::SmallInt(_) | DataType::Int2(_) => Some(SkipprDataType::Short),
            DataType::Float(_) | DataType::Float4 | DataType::Real => Some(SkipprDataType::Float),
            DataType::Double(_) | DataType::Float8 | DataType::DoublePrecision => {
                Some(SkipprDataType::Double)
            }
            DataType::Decimal(_) | DataType::Numeric(_) => Some(SkipprDataType::Decimal),
            DataType::Boolean => Some(SkipprDataType::Boolean),
            DataType::Text | DataType::String(_) | DataType::Varchar(_) | DataType::Char(_) => {
                Some(SkipprDataType::String)
            }
            _ => None,
        })
        .ok_or_else(|| {
            ParserError::ParserError(format!("No Skippr type equivalent for '{rendered}'"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::schema_alter::SchemaAlterOp;

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

    fn alter_op(sql: &str) -> SchemaAlterOp {
        let mut parser = SParser::new(sql).unwrap();
        match parser.parse_statement().unwrap() {
            Statement::AlterTable(stmt) => stmt.op,
            other => panic!("expected AlterTable, got {other:?}"),
        }
    }

    #[test]
    fn alter_table_rename_column() {
        assert_eq!(
            alter_op("ALTER TABLE cube_events.pour_start RENAME COLUMN price TO amount"),
            SchemaAlterOp::Rename {
                from: "price".into(),
                to: "amount".into(),
            }
        );
    }

    #[test]
    fn alter_table_merge_column() {
        assert_eq!(
            alter_op("ALTER TABLE cube_events.pour_start MERGE COLUMN price_string INTO price"),
            SchemaAlterOp::Merge {
                src: "price_string".into(),
                dst: "price".into(),
            }
        );
    }

    #[test]
    fn alter_table_drop_column() {
        assert_eq!(
            alter_op("ALTER TABLE cube_events.pour_start DROP COLUMN note"),
            SchemaAlterOp::Drop {
                column: "note".into(),
            }
        );
    }

    #[test]
    fn alter_table_promote_column() {
        assert_eq!(
            alter_op("ALTER TABLE cube_events.pour_start ALTER COLUMN n TYPE BIGINT"),
            SchemaAlterOp::Promote {
                column: "n".into(),
                to: SkipprDataType::Long,
            }
        );
    }

    #[test]
    fn alter_schema_is_rejected() {
        let mut parser = SParser::new("ALTER SCHEMA bike_hire DROP COLUMN old_field").unwrap();
        let err = parser.parse_statement().unwrap_err();
        assert!(
            err.to_string().contains("TABLE"),
            "expected TABLE instruction, got {err}"
        );
    }

    #[test]
    fn alter_table_quoted_hyphenated_namespace() {
        let mut parser = SParser::new(
            r#"ALTER TABLE s3_alter."skippr-e2e-sample-data" RENAME COLUMN region TO region_code"#,
        )
        .unwrap();
        match parser.parse_statement().unwrap() {
            Statement::AlterTable(stmt) => {
                let pipeline = format!("{}", stmt.pipeline).replace('"', "");
                let namespace = stmt
                    .namespace
                    .as_ref()
                    .map(|n| format!("{n}").replace('"', ""))
                    .expect("namespace");
                assert_eq!(pipeline, "s3_alter");
                assert_eq!(namespace, "skippr-e2e-sample-data");
                assert_eq!(
                    stmt.op,
                    SchemaAlterOp::Rename {
                        from: "region".into(),
                        to: "region_code".into(),
                    }
                );
            }
            other => panic!("expected AlterTable, got {other:?}"),
        }
    }

    fn parse_ok(sql: &str) -> Statement {
        let mut parser = SParser::new(sql).unwrap();
        parser.parse_statement().unwrap()
    }

    fn parse_err(sql: &str) -> String {
        let mut parser = SParser::new(sql).unwrap();
        parser.parse_statement().unwrap_err().to_string()
    }

    #[test]
    fn create_view_parses_fqn() {
        match parse_ok(
            "CREATE VIEW invoices.positive AS SELECT * FROM invoices.orders WHERE amount > 0",
        ) {
            Statement::CreateView { name, as_select } => {
                assert_eq!(name.to_string(), "invoices.positive");
                assert!(as_select.to_uppercase().contains("SELECT"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stream_parses_from_and_window() {
        match parse_ok("STREAM customer_id FROM invoices.orders WINDOW 30") {
            Statement::Stream(stream) => {
                assert_eq!(stream.from, "invoices.orders");
                assert_eq!(stream.window_secs, Some(30));
                assert!(stream.select_sql.to_uppercase().starts_with("SELECT "));
                assert!(!stream.select_sql.to_uppercase().contains(" WINDOW "));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stream_from_pipeline_without_window() {
        match parse_ok("STREAM count(*) FROM shop") {
            Statement::Stream(stream) => {
                assert_eq!(stream.from, "shop");
                assert_eq!(stream.window_secs, None);
                assert!(stream.select_sql.to_ascii_uppercase().contains("COUNT"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stream_requires_from() {
        let err = parse_err("STREAM 1");
        assert!(err.to_ascii_uppercase().contains("FROM"), "{err}");
    }

    #[test]
    fn create_view_quoted_fqn() {
        match parse_ok(r#"CREATE VIEW "invoices"."positive" AS SELECT * FROM invoices.orders"#) {
            Statement::CreateView { name, as_select } => {
                assert_eq!(name.to_string(), "invoices.positive");
                assert!(as_select.to_uppercase().contains("SELECT"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn create_view_requires_as_select() {
        let err = parse_err("CREATE VIEW invoices.positive");
        assert!(
            err.to_uppercase().contains("AS") || err.to_uppercase().contains("SELECT"),
            "{err}"
        );
    }

    #[test]
    fn create_view_rejects_one_part_name() {
        let err = parse_err("CREATE VIEW positive AS SELECT * FROM invoices.orders");
        assert!(err.contains("pipeline.name"), "{err}");
    }

    #[test]
    fn empty_create_view_is_parse_fail() {
        let err = parse_err("CREATE VIEW invoices.positive AS");
        assert!(
            !err.is_empty(),
            "empty CREATE VIEW AS must fail closed: {err}"
        );
    }

    #[test]
    fn create_table_as_parses_group_by() {
        match parse_ok(
            "CREATE TABLE invoices.customer_totals AS SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
        ) {
            Statement::CreateTableAs { name, .. } => {
                assert_eq!(name.to_string(), "invoices.customer_totals")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn create_index_is_parse_fail() {
        let err = parse_err("CREATE INDEX foo ON invoices.orders (customer_id)");
        assert!(err.contains("CREATE INDEX"), "{err}");
    }

    #[test]
    fn create_sink_is_parse_fail() {
        let err = parse_err("CREATE SINK lake FROM invoices.orders");
        assert!(err.contains("CREATE SINK"), "{err}");
    }

    #[test]
    fn empty_create_table_is_parse_fail() {
        let err = parse_err("CREATE TABLE invoices.customer_totals");
        assert!(err.to_uppercase().contains("AS SELECT"), "{err}");
    }

    #[test]
    fn drop_table_compute_is_drop_compute() {
        match parse_ok("DROP TABLE invoices.customer_totals") {
            Statement::DropCompute { name } => {
                assert_eq!(name.to_string(), "invoices.customer_totals")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn alter_compute_set_measure() {
        match parse_ok(
            "ALTER COMPUTE invoices.customer_totals ALTER COLUMN invoice_total SET MEASURE SUM(amount)",
        ) {
            Statement::AlterCompute { name, alterations } => {
                assert_eq!(name.to_string(), "invoices.customer_totals");
                assert_eq!(
                    alterations,
                    vec![ComputeAlteration::SetMeasure {
                        column: "invoice_total".into(),
                        kind: UnboundMeasureKind::Sum("amount".into()),
                    }]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn alter_table_set_measure_is_alter_compute() {
        match parse_ok(
            "ALTER TABLE invoices.customer_totals ALTER COLUMN invoice_total SET MEASURE SUM(amount)",
        ) {
            Statement::AlterCompute { name, alterations } => {
                assert_eq!(name.to_string(), "invoices.customer_totals");
                assert!(matches!(
                    alterations[0],
                    ComputeAlteration::SetMeasure { .. }
                ));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn alter_compute_stateless_set_filter() {
        match parse_ok("ALTER COMPUTE positive_orders ALTER COLUMN amount SET FILTER amount > 10") {
            Statement::AlterCompute { name, alterations } => {
                assert!(name.is_bare(), "{name}");
                assert_eq!(name.name.as_str(), "positive_orders");
                assert!(matches!(
                    alterations[0],
                    ComputeAlteration::SetFilter { .. }
                ));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn alter_table_type_is_not_compute_sugar() {
        match parse_ok("ALTER TABLE invoices.orders ALTER COLUMN n TYPE BIGINT") {
            Statement::AlterTable(stmt) => {
                assert!(matches!(
                    stmt.op,
                    crate::discover::schema_alter::SchemaAlterOp::Promote { .. }
                ));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn peek_compute_alter_does_not_scan_source() {
        let src = include_str!("mod.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let peek = prod
            .split("fn peek_compute_alter")
            .nth(1)
            .expect("peek_compute_alter")
            .split("fn parse_compute_alterations")
            .next()
            .expect("peek body");
        assert!(
            !peek.contains("self.source"),
            "ALTER TABLE compute sugar must peek tokens, not scan the source string"
        );
        assert!(
            !peek.contains("contains("),
            "ALTER TABLE compute sugar must peek tokens, not scan the source string"
        );
    }
}
