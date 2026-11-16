//! SQL → unbound compute. Bind FieldIds in [`super::bind`].

use std::time::Duration;

use datafusion::sql::sqlparser::ast::{
    Distinct, DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
    GroupByExpr, Ident as SqlIdent, ObjectName, ObjectNamePart, OrderByKind, Query as SqlQuery,
    Select, SelectItem, SetExpr, SetOperator, TableFactor, TableWithJoins, Value, WindowType,
};
use datafusion::sql::sqlparser::dialect::PostgreSqlDialect;
use datafusion::sql::sqlparser::parser::{Parser, ParserError};
use datafusion::sql::sqlparser::tokenizer::Token;

use super::Fqn;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UnboundError {
    #[error("{0}")]
    Message(String),
}

impl From<ParserError> for UnboundError {
    fn from(err: ParserError) -> Self {
        Self::Message(err.to_string())
    }
}

impl From<String> for UnboundError {
    fn from(err: String) -> Self {
        Self::Message(err)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnboundRelation {
    pub pipeline: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnboundSelect {
    pub filter: Option<String>,
    pub derive: Vec<(String, String)>,
    pub star: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnboundMeasureKind {
    Sum(String),
    Count,
    Min(String),
    Max(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnboundMeasure {
    pub name: String,
    pub kind: UnboundMeasureKind,
}

/// `ALTER COMPUTE` / table-backed `ALTER TABLE` sugar against a catalog name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComputeAlteration {
    SetMeasure {
        column: String,
        kind: UnboundMeasureKind,
    },
    DropColumn {
        column: String,
    },
    AddMeasure {
        name: String,
        kind: UnboundMeasureKind,
    },
    RenameColumn {
        from: String,
        to: String,
    },
    SetFilter {
        expr: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnboundCompute {
    Stateless {
        name: Fqn,
        target: UnboundRelation,
        select: UnboundSelect,
    },
    View {
        name: Fqn,
        input: UnboundRelation,
        select: UnboundSelect,
    },
    Latest {
        name: Fqn,
        input: UnboundRelation,
        keys: Vec<String>,
        /// `(column, descending)` from `ORDER BY`. Required for DISTINCT ON.
        order: Vec<(String, bool)>,
    },
    KeyedTable {
        name: Fqn,
        input: UnboundRelation,
        keys: Vec<String>,
        measures: Vec<UnboundMeasure>,
    },
    WindowedTable {
        name: Fqn,
        input: UnboundRelation,
        keys: Vec<String>,
        trunc_unit: Duration,
        event_time: String,
        measures: Vec<UnboundMeasure>,
    },
    StreamTableJoin {
        name: Fqn,
        stream: UnboundRelation,
        table: Option<Fqn>,
        keys: Vec<String>,
    },
}

pub fn parse_watermark_delay(raw: &str) -> Result<Duration, UnboundError> {
    let raw = raw.trim().to_ascii_lowercase();
    let mut parts = raw.split_whitespace();
    let n: u64 = parts
        .next()
        .ok_or_else(|| UnboundError::Message("watermark_delay must be '<n> <unit>'".into()))?
        .parse()
        .map_err(|_| UnboundError::Message("watermark_delay count must be an integer".into()))?;
    let unit = parts
        .next()
        .ok_or_else(|| UnboundError::Message("watermark_delay must include a unit".into()))?;
    if parts.next().is_some() {
        return Err(UnboundError::Message(
            "watermark_delay must be '<n> <unit>'".into(),
        ));
    }
    let secs = match unit {
        "second" | "seconds" | "s" => n,
        "minute" | "minutes" | "m" => n.saturating_mul(60),
        "hour" | "hours" | "h" => n.saturating_mul(3600),
        "day" | "days" | "d" => n.saturating_mul(86400),
        other => {
            return Err(UnboundError::Message(format!(
                "watermark_delay unit '{other}' is not a Duration"
            )))
        }
    };
    Ok(Duration::from_secs(secs))
}

pub fn desugar_create(
    name: Fqn,
    as_select: &str,
    is_view: bool,
) -> Result<UnboundCompute, UnboundError> {
    let query = parse_query(as_select)?;
    reject_banned_ast(&query)?;
    classify(name, &query, is_view)
}

/// Ingest `SELECT` (filter / derive only). GROUP BY is not Stateless.
pub fn desugar_stateless(name: Fqn, sql: &str) -> Result<UnboundCompute, UnboundError> {
    let query = parse_query(sql)?;
    reject_banned_ast(&query)?;
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(UnboundError::Message(
            "Stateless ingest SQL must be a SELECT".into(),
        ));
    };
    if select.from.len() != 1 || !select.from[0].joins.is_empty() {
        return Err(UnboundError::Message(
            "Stateless FROM must be one pipeline.namespace".into(),
        ));
    }
    if has_group_by(&select.group_by) {
        return Err(UnboundError::Message(
            "GROUP BY is not Stateless; use CREATE TABLE AS SELECT".into(),
        ));
    }
    if select.distinct.is_some() {
        return Err(UnboundError::Message(
            "DISTINCT is not Stateless; use CREATE TABLE".into(),
        ));
    }
    let input = relation_from_table(&select.from[0])?;
    let (star, derive, measures, over_keys) = classify_projection(&select.projection)?;
    if !measures.is_empty() || over_keys.is_some() {
        return Err(UnboundError::Message(
            "aggregates are not Stateless; use CREATE TABLE AS SELECT".into(),
        ));
    }
    if !star && derive.is_empty() {
        return Err(UnboundError::Message(
            "Stateless ingest SQL is filter and derive only; schema is ALTER TABLE".into(),
        ));
    }
    Ok(UnboundCompute::Stateless {
        name,
        target: input,
        select: UnboundSelect {
            filter: select.selection.as_ref().map(|e| e.to_string()),
            derive,
            star,
        },
    })
}

pub fn canonicalize_sql_expr(sql: &str) -> Result<String, UnboundError> {
    let dialect = PostgreSqlDialect {};
    let mut parser = Parser::new(&dialect).try_with_sql(sql)?;
    let expr = parser.parse_expr()?;
    if matches!(parser.peek_token().token, Token::SemiColon) {
        parser.next_token();
    }
    match parser.peek_token().token {
        Token::EOF => Ok(expr.to_string()),
        other => Err(UnboundError::Message(format!(
            "trailing tokens after expression are not v1 declared compute ({other})"
        ))),
    }
}

fn parse_query(sql: &str) -> Result<SqlQuery, UnboundError> {
    let dialect = PostgreSqlDialect {};
    let mut parser = Parser::new(&dialect).try_with_sql(sql)?;
    let query = *parser.parse_query()?;
    if matches!(parser.peek_token().token, Token::SemiColon) {
        parser.next_token();
    }
    match parser.peek_token().token {
        Token::EOF => Ok(query),
        other => Err(UnboundError::Message(format!(
            "trailing tokens after SELECT are not v1 declared compute ({other})"
        ))),
    }
}

fn reject_banned_ast(query: &SqlQuery) -> Result<(), UnboundError> {
    if query.with.is_some() {
        return Err(UnboundError::Message(
            "WITH is not v1 declared compute".into(),
        ));
    }
    reject_banned_set_expr(query.body.as_ref())
}

fn reject_banned_set_expr(expr: &SetExpr) -> Result<(), UnboundError> {
    match expr {
        SetExpr::SetOperation {
            op: SetOperator::Except,
            ..
        } => Err(UnboundError::Message(
            "EXCEPT is not v1 declared compute".into(),
        )),
        SetExpr::SetOperation { left, right, .. } => {
            reject_banned_set_expr(left)?;
            reject_banned_set_expr(right)
        }
        SetExpr::Query(inner) => reject_banned_ast(inner),
        SetExpr::Select(select) => {
            for from in &select.from {
                reject_banned_table(&from.relation)?;
                if !from.joins.is_empty() {
                    return Err(UnboundError::Message(
                        "JOIN in CREATE is not v1 declared compute; query it ad-hoc".into(),
                    ));
                }
            }
            if let Some(selection) = &select.selection {
                reject_banned_expr(selection)?;
            }
            for item in &select.projection {
                match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        reject_banned_expr(expr)?;
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn reject_banned_table(factor: &TableFactor) -> Result<(), UnboundError> {
    match factor {
        TableFactor::Derived { lateral: true, .. } => {
            Err(UnboundError::Message("LATERAL is not supported".into()))
        }
        TableFactor::Function { lateral, name, .. } => {
            if *lateral {
                return Err(UnboundError::Message("LATERAL is not supported".into()));
            }
            let fname = fn_name(name);
            if banned_window_fn(&fname) {
                return Err(UnboundError::Message(format!(
                    "{fname} windows are not v1; use date_trunc"
                )));
            }
            Ok(())
        }
        TableFactor::Table { name, .. } => {
            object_to_relation(name)?;
            Ok(())
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => reject_banned_table(&table_with_joins.relation),
        _ => Ok(()),
    }
}

fn reject_banned_expr(expr: &Expr) -> Result<(), UnboundError> {
    match expr {
        Expr::Function(func) => {
            let fname = fn_name(&func.name);
            if banned_window_fn(&fname) {
                return Err(UnboundError::Message(format!(
                    "{fname} windows are not v1; use date_trunc"
                )));
            }
            if let FunctionArguments::List(list) = &func.args {
                if matches!(list.duplicate_treatment, Some(DuplicateTreatment::Distinct)) {
                    return Err(UnboundError::Message(
                        "COUNT(DISTINCT) is not v1 declared compute".into(),
                    ));
                }
                for arg in &list.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(inner))
                        | FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(inner),
                            ..
                        }
                        | FunctionArg::ExprNamed {
                            arg: FunctionArgExpr::Expr(inner),
                            ..
                        } => reject_banned_expr(inner)?,
                        _ => {}
                    }
                }
            }
            if let Some(WindowType::WindowSpec(spec)) = &func.over {
                if spec.window_frame.is_some() {
                    return Err(UnboundError::Message(
                        "OVER frames (ROWS n PRECEDING) are not v1".into(),
                    ));
                }
            }
            Ok(())
        }
        Expr::Nested(inner) => reject_banned_expr(inner),
        Expr::BinaryOp { left, right, .. } => {
            reject_banned_expr(left)?;
            reject_banned_expr(right)
        }
        _ => Ok(()),
    }
}

fn banned_window_fn(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "hop" | "session" | "cumulate"
    )
}

fn classify(name: Fqn, query: &SqlQuery, is_view: bool) -> Result<UnboundCompute, UnboundError> {
    if matches!(
        query.body.as_ref(),
        SetExpr::SetOperation {
            op: SetOperator::Except | SetOperator::Union | SetOperator::Intersect,
            ..
        }
    ) {
        return Err(UnboundError::Message(
            "EXCEPT / UNION / INTERSECT of pipelines is not v1 declared compute".into(),
        ));
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(UnboundError::Message("CREATE requires a SELECT".into()));
    };
    if select.from.len() != 1 {
        return Err(UnboundError::Message(
            "declared compute FROM must be one relation in the same pipeline".into(),
        ));
    }
    if !select.from[0].joins.is_empty() {
        return Err(UnboundError::Message(
            "JOIN in CREATE is not v1 declared compute; query it ad-hoc".into(),
        ));
    }
    let input = relation_from_table(&select.from[0])?;
    if input.pipeline != name.pipeline.as_str() {
        return Err(UnboundError::Message(
            "CREATE TABLE FROM another pipeline is not v1; query it ad-hoc".into(),
        ));
    }
    if is_view {
        if matches!(
            select.distinct,
            Some(Distinct::On(_)) | Some(Distinct::Distinct)
        ) {
            return Err(UnboundError::Message(
                "CREATE VIEW cannot DISTINCT; use CREATE TABLE".into(),
            ));
        }
        if has_group_by(&select.group_by) {
            return Err(UnboundError::Message(
                "CREATE VIEW cannot GROUP BY; use CREATE TABLE AS SELECT".into(),
            ));
        }
        reject_unmodeled_view_clauses(query, select)?;
    }
    let filter = select.selection.as_ref().map(|e| e.to_string());
    let (star, derive, measures, over_keys) = classify_projection(&select.projection)?;
    if star && !measures.is_empty() {
        return Err(UnboundError::Message(
            "SELECT * on KeyedTable is illegal; name each measure".into(),
        ));
    }
    if is_view {
        if !measures.is_empty() || over_keys.is_some() {
            return Err(UnboundError::Message(
                "CREATE VIEW cannot aggregate; use CREATE TABLE AS SELECT".into(),
            ));
        }
        if !star {
            return Err(UnboundError::Message(
                "CREATE VIEW must SELECT * (optional derived columns AS …)".into(),
            ));
        }
        return Ok(UnboundCompute::View {
            name,
            input,
            select: UnboundSelect {
                filter,
                derive,
                star,
            },
        });
    }
    if let Some(Distinct::On(exprs)) = &select.distinct {
        let keys = exprs
            .iter()
            .map(expr_column)
            .collect::<Result<Vec<_>, _>>()?;
        if keys.is_empty() {
            return Err(UnboundError::Message("DISTINCT ON requires keys".into()));
        }
        let order = order_by_cols(query)?;
        if order.is_empty() {
            return Err(UnboundError::Message(
                "DISTINCT ON requires ORDER BY".into(),
            ));
        }
        return Ok(UnboundCompute::Latest {
            name,
            input,
            keys,
            order,
        });
    }
    if matches!(select.distinct, Some(Distinct::Distinct)) {
        let keys = distinct_keys(&select.projection)?;
        return Ok(UnboundCompute::KeyedTable {
            name,
            input,
            keys,
            measures: vec![UnboundMeasure {
                name: "n".into(),
                kind: UnboundMeasureKind::Count,
            }],
        });
    }
    if let Some(keys) = over_keys {
        return Ok(UnboundCompute::StreamTableJoin {
            name,
            stream: input,
            table: None,
            keys,
        });
    }
    let group_exprs = group_exprs(&select.group_by)?;
    if measures.iter().any(|m| {
        matches!(
            m.kind,
            UnboundMeasureKind::Sum(_) | UnboundMeasureKind::Min(_) | UnboundMeasureKind::Max(_)
        )
    }) && group_exprs.is_empty()
    {
        return Err(UnboundError::Message(
            "GROUP BY-less SUM is not declared compute; use CREATE TABLE … GROUP BY".into(),
        ));
    }
    if group_exprs.is_empty() {
        return Err(UnboundError::Message(
            "CREATE TABLE AS SELECT must GROUP BY, DISTINCT ON, or DISTINCT".into(),
        ));
    }
    if let Some((unit, ts)) = split_window_group(&group_exprs)? {
        let keys: Vec<String> = group_exprs
            .iter()
            .filter(|e| date_trunc_of(e).is_none() && tumble_of(e).is_none())
            .map(expr_column)
            .collect::<Result<_, _>>()?;
        return Ok(UnboundCompute::WindowedTable {
            name,
            input,
            keys,
            trunc_unit: unit,
            event_time: ts,
            measures,
        });
    }
    let keys = group_exprs
        .iter()
        .map(expr_column)
        .collect::<Result<Vec<_>, _>>()?;
    if measures.is_empty() {
        return Err(UnboundError::Message(
            "CREATE TABLE GROUP BY requires at least one measure".into(),
        ));
    }
    Ok(UnboundCompute::KeyedTable {
        name,
        input,
        keys,
        measures,
    })
}

fn reject_unmodeled_view_clauses(query: &SqlQuery, select: &Select) -> Result<(), UnboundError> {
    if query.order_by.is_some() {
        return Err(UnboundError::Message("CREATE VIEW cannot ORDER BY".into()));
    }
    if query.limit_clause.is_some() {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot LIMIT / OFFSET".into(),
        ));
    }
    if query.fetch.is_some() {
        return Err(UnboundError::Message("CREATE VIEW cannot FETCH".into()));
    }
    if !query.locks.is_empty() {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot FOR UPDATE".into(),
        ));
    }
    if !query.pipe_operators.is_empty() {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot pipe operators".into(),
        ));
    }
    if query.for_clause.is_some() {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot FOR XML / FOR JSON".into(),
        ));
    }
    if query.settings.is_some() || query.format_clause.is_some() {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot SETTINGS / FORMAT".into(),
        ));
    }
    if !select.connect_by.is_empty() {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot CONNECT BY".into(),
        ));
    }
    if !select.optimizer_hints.is_empty() {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot optimizer hints".into(),
        ));
    }
    if select.select_modifiers.is_some() {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot SELECT modifiers".into(),
        ));
    }
    if select.value_table_mode.is_some() {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot SELECT AS VALUE / STRUCT".into(),
        ));
    }
    if select.having.is_some() {
        return Err(UnboundError::Message("CREATE VIEW cannot HAVING".into()));
    }
    if select.qualify.is_some() {
        return Err(UnboundError::Message("CREATE VIEW cannot QUALIFY".into()));
    }
    if !select.lateral_views.is_empty() {
        return Err(UnboundError::Message("CREATE VIEW cannot LATERAL".into()));
    }
    if select.top.is_some() {
        return Err(UnboundError::Message("CREATE VIEW cannot TOP".into()));
    }
    if select.into.is_some() {
        return Err(UnboundError::Message("CREATE VIEW cannot INTO".into()));
    }
    if select.exclude.is_some() {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot EXCLUDE; SELECT *".into(),
        ));
    }
    if !select.named_window.is_empty() {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot named WINDOW".into(),
        ));
    }
    if select.prewhere.is_some() {
        return Err(UnboundError::Message("CREATE VIEW cannot PREWHERE".into()));
    }
    if !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
    {
        return Err(UnboundError::Message(
            "CREATE VIEW cannot CLUSTER / DISTRIBUTE / SORT BY".into(),
        ));
    }
    Ok(())
}

fn has_group_by(group_by: &GroupByExpr) -> bool {
    match group_by {
        GroupByExpr::All(_) => true,
        GroupByExpr::Expressions(exprs, _) => !exprs.is_empty(),
    }
}

fn group_exprs(group_by: &GroupByExpr) -> Result<Vec<Expr>, UnboundError> {
    match group_by {
        GroupByExpr::All(_) => Err(UnboundError::Message(
            "GROUP BY ALL is not supported".into(),
        )),
        GroupByExpr::Expressions(exprs, _) => Ok(exprs.clone()),
    }
}

fn relation_from_table(from: &TableWithJoins) -> Result<UnboundRelation, UnboundError> {
    match &from.relation {
        TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_hints,
        } => {
            if sample.is_some() {
                return Err(UnboundError::Message(
                    "TABLESAMPLE is not v1 declared compute".into(),
                ));
            }
            if *with_ordinality {
                return Err(UnboundError::Message(
                    "WITH ORDINALITY is not v1 declared compute".into(),
                ));
            }
            if args.is_some() {
                return Err(UnboundError::Message(
                    "table-valued functions are not v1 declared compute".into(),
                ));
            }
            if !with_hints.is_empty() || version.is_some() || !partitions.is_empty() {
                return Err(UnboundError::Message(
                    "FROM hints / version / PARTITION are not v1 declared compute".into(),
                ));
            }
            if json_path.is_some() || !index_hints.is_empty() {
                return Err(UnboundError::Message(
                    "FROM json_path / index hints are not v1 declared compute".into(),
                ));
            }
            if let Some(alias) = alias {
                if !alias.columns.is_empty() || alias.at.is_some() {
                    return Err(UnboundError::Message(
                        "FROM alias column lists are not v1 declared compute".into(),
                    ));
                }
            }
            object_to_relation(name)
        }
        TableFactor::Function { name, .. } if fn_name(name).eq_ignore_ascii_case("tumble") => Err(
            UnboundError::Message("use date_trunc in GROUP BY or TUMBLE(ts, unit)".into()),
        ),
        TableFactor::TableFunction { .. } => Err(UnboundError::Message(
            "TABLE(TUMBLE) sugar requires date_trunc in GROUP BY or TUMBLE(ts, unit)".into(),
        )),
        other => Err(UnboundError::Message(format!("unsupported FROM {other}"))),
    }
}

fn object_to_relation(name: &ObjectName) -> Result<UnboundRelation, UnboundError> {
    let mut parts = Vec::new();
    for part in &name.0 {
        match part {
            ObjectNamePart::Identifier(id) => parts.push(ident_value(id)),
            ObjectNamePart::Function(_) => {
                return Err(UnboundError::Message(
                    "relation name cannot be a function".into(),
                ))
            }
        }
    }
    match parts.as_slice() {
        [a, b] => {
            if a.eq_ignore_ascii_case("source") || b.eq_ignore_ascii_case("_source") {
                return Err(UnboundError::Message(
                    "FROM source / ._source is not Skippr SQL; use pipeline.namespace".into(),
                ));
            }
            Ok(UnboundRelation {
                pipeline: a.clone(),
                name: b.clone(),
            })
        }
        [a] if a.eq_ignore_ascii_case("source") => Err(UnboundError::Message(
            "FROM source / ._source is not Skippr SQL; use pipeline.namespace".into(),
        )),
        _ => Err(UnboundError::Message(
            "relation must be pipeline.namespace".into(),
        )),
    }
}

fn order_by_cols(query: &SqlQuery) -> Result<Vec<(String, bool)>, UnboundError> {
    let Some(order) = &query.order_by else {
        return Ok(Vec::new());
    };
    match &order.kind {
        OrderByKind::All(_) => Err(UnboundError::Message(
            "ORDER BY ALL is not supported".into(),
        )),
        OrderByKind::Expressions(exprs) => exprs
            .iter()
            .map(|e| {
                let desc = e.options.asc == Some(false);
                Ok((expr_column(&e.expr)?, desc))
            })
            .collect(),
    }
}

fn ident_value(id: &SqlIdent) -> String {
    id.value.clone()
}

fn fn_name(name: &ObjectName) -> String {
    name.0
        .last()
        .and_then(|p| match p {
            ObjectNamePart::Identifier(id) => Some(id.value.clone()),
            ObjectNamePart::Function(_) => None,
        })
        .unwrap_or_default()
}

fn classify_projection(
    items: &[SelectItem],
) -> Result<
    (
        bool,
        Vec<(String, String)>,
        Vec<UnboundMeasure>,
        Option<Vec<String>>,
    ),
    UnboundError,
> {
    let mut star = false;
    let mut derive = Vec::new();
    let mut measures = Vec::new();
    let mut over_keys = None;
    for item in items {
        match item {
            SelectItem::Wildcard(opts) | SelectItem::QualifiedWildcard(_, opts) => {
                if opts.opt_ilike.is_some()
                    || opts.opt_exclude.is_some()
                    || opts.opt_except.is_some()
                    || opts.opt_replace.is_some()
                    || opts.opt_rename.is_some()
                    || opts.opt_alias.is_some()
                {
                    return Err(UnboundError::Message(
                        "SELECT * options (ILIKE / EXCLUDE / EXCEPT / REPLACE / RENAME / AS) are not v1 declared compute".into(),
                    ));
                }
                star = true;
            }
            SelectItem::ExprWithAliases { .. } => {
                return Err(UnboundError::Message(
                    "SELECT aliases lists are not v1 declared compute".into(),
                ));
            }
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                let alias = match item {
                    SelectItem::ExprWithAlias { alias, .. } => Some(ident_value(alias)),
                    _ => None,
                };
                if let Some((kind, over)) = measure_of(expr)? {
                    if kind_is_avg(expr) {
                        let base = alias.clone().unwrap_or_else(|| "avg".into());
                        measures.push(UnboundMeasure {
                            name: format!("{base}_sum"),
                            kind: UnboundMeasureKind::Sum(measure_arg_name(expr)?),
                        });
                        measures.push(UnboundMeasure {
                            name: format!("{base}_count"),
                            kind: UnboundMeasureKind::Count,
                        });
                    } else {
                        let name = alias.ok_or_else(|| {
                            UnboundError::Message("measures must be named (AS …)".into())
                        })?;
                        measures.push(UnboundMeasure { name, kind });
                    }
                    if let Some(keys) = over {
                        over_keys = Some(keys);
                    }
                } else if matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
                    || date_trunc_of(expr).is_some()
                    || tumble_of(expr).is_some()
                {
                } else {
                    let name = alias.ok_or_else(|| {
                        UnboundError::Message("derived columns must be named (AS …)".into())
                    })?;
                    derive.push((name, expr.to_string()));
                }
            }
        }
    }
    Ok((star, derive, measures, over_keys))
}

fn kind_is_avg(expr: &Expr) -> bool {
    match expr {
        Expr::Function(func) => fn_name(&func.name).eq_ignore_ascii_case("avg"),
        _ => false,
    }
}

fn measure_arg_name(expr: &Expr) -> Result<String, UnboundError> {
    let Expr::Function(func) = expr else {
        return Err(UnboundError::Message("expected aggregate function".into()));
    };
    first_arg_column(func)
}

fn measure_of(
    expr: &Expr,
) -> Result<Option<(UnboundMeasureKind, Option<Vec<String>>)>, UnboundError> {
    let Expr::Function(func) = expr else {
        return Ok(None);
    };
    let Some(kind) = function_measure(func)? else {
        return Ok(None);
    };
    match &func.over {
        Some(WindowType::WindowSpec(spec)) => {
            let keys = window_partition_keys(spec)?;
            Ok(Some((kind, Some(keys))))
        }
        Some(WindowType::NamedWindow(_)) => Err(UnboundError::Message(
            "named OVER windows are not v1".into(),
        )),
        None => Ok(Some((kind, None))),
    }
}

fn function_measure(func: &Function) -> Result<Option<UnboundMeasureKind>, UnboundError> {
    let fname = fn_name(&func.name);
    let kind = match fname.to_ascii_lowercase().as_str() {
        "sum" => UnboundMeasureKind::Sum(first_arg_column(func)?),
        "count" => UnboundMeasureKind::Count,
        "min" => UnboundMeasureKind::Min(first_arg_column(func)?),
        "max" => UnboundMeasureKind::Max(first_arg_column(func)?),
        "avg" => UnboundMeasureKind::Sum(first_arg_column(func)?),
        _ => return Ok(None),
    };
    Ok(Some(kind))
}

fn first_arg_column(func: &Function) -> Result<String, UnboundError> {
    let FunctionArguments::List(list) = &func.args else {
        return Err(UnboundError::Message("aggregate needs an argument".into()));
    };
    let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))) = list.args.first() else {
        if matches!(
            list.args.first(),
            Some(FunctionArg::Unnamed(FunctionArgExpr::Wildcard))
        ) {
            return Ok("*".into());
        }
        return Err(UnboundError::Message("aggregate needs a column".into()));
    };
    expr_column(expr)
}

fn window_partition_keys(
    spec: &datafusion::sql::sqlparser::ast::WindowSpec,
) -> Result<Vec<String>, UnboundError> {
    if spec.window_frame.is_some() {
        return Err(UnboundError::Message(
            "OVER frames (ROWS n PRECEDING) are not v1".into(),
        ));
    }
    if spec.partition_by.is_empty() {
        return Err(UnboundError::Message(
            "SUM() OVER requires PARTITION BY keys matching an existing KeyedTable".into(),
        ));
    }
    spec.partition_by.iter().map(expr_column).collect()
}

fn expr_column(expr: &Expr) -> Result<String, UnboundError> {
    match expr {
        Expr::Identifier(id) => Ok(ident_value(id)),
        Expr::CompoundIdentifier(parts) => Ok(parts
            .last()
            .map(ident_value)
            .ok_or_else(|| UnboundError::Message("empty identifier".into()))?),
        Expr::Nested(inner) => expr_column(inner),
        other => {
            if date_trunc_of(other).is_some() || tumble_of(other).is_some() {
                return Err(UnboundError::Message(
                    "window expression is not a key column".into(),
                ));
            }
            Err(UnboundError::Message(format!(
                "GROUP BY / key must be a column, got {other}"
            )))
        }
    }
}

fn distinct_keys(items: &[SelectItem]) -> Result<Vec<String>, UnboundError> {
    let mut keys = Vec::new();
    for item in items {
        match item {
            SelectItem::UnnamedExpr(Expr::Identifier(id)) => keys.push(ident_value(id)),
            SelectItem::ExprWithAlias {
                expr: Expr::Identifier(id),
                ..
            } => keys.push(ident_value(id)),
            SelectItem::Wildcard(_)
            | SelectItem::QualifiedWildcard(_, _)
            | SelectItem::ExprWithAliases { .. } => {
                return Err(UnboundError::Message(
                    "DISTINCT * is not presence; list key columns".into(),
                ))
            }
            _ => {
                return Err(UnboundError::Message(
                    "DISTINCT keys must be columns".into(),
                ))
            }
        }
    }
    if keys.is_empty() {
        return Err(UnboundError::Message("DISTINCT requires keys".into()));
    }
    Ok(keys)
}

fn split_window_group(exprs: &[Expr]) -> Result<Option<(Duration, String)>, UnboundError> {
    let mut found = None;
    for expr in exprs {
        if let Some((unit, ts)) = date_trunc_of(expr).or_else(|| tumble_of(expr)) {
            if found.is_some() {
                return Err(UnboundError::Message(
                    "one date_trunc / TUMBLE window per GROUP BY".into(),
                ));
            }
            found = Some((unit, ts));
        }
    }
    Ok(found)
}

fn date_trunc_of(expr: &Expr) -> Option<(Duration, String)> {
    let Expr::Function(func) = expr else {
        return None;
    };
    if !fn_name(&func.name).eq_ignore_ascii_case("date_trunc") {
        return None;
    }
    let FunctionArguments::List(list) = &func.args else {
        return None;
    };
    let unit = arg_lit(list.args.first()?)?;
    let ts = match list.args.get(1)? {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => expr_column(e).ok()?,
        _ => return None,
    };
    Some((trunc_unit(&unit)?, ts))
}

fn tumble_of(expr: &Expr) -> Option<(Duration, String)> {
    let Expr::Function(func) = expr else {
        return None;
    };
    if !fn_name(&func.name).eq_ignore_ascii_case("tumble") {
        return None;
    }
    let FunctionArguments::List(list) = &func.args else {
        return None;
    };
    let ts = match list.args.first()? {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => expr_column(e).ok()?,
        _ => return None,
    };
    let unit = arg_lit(list.args.get(1)?)?;
    Some((
        trunc_unit(&unit).or_else(|| interval_to_duration(&unit))?,
        ts,
    ))
}

fn arg_lit(arg: &FunctionArg) -> Option<String> {
    match arg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(v))) => match &v.value {
            Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) => Some(s.clone()),
            other => Some(other.to_string()),
        },
        FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Identifier(id))) => Some(ident_value(id)),
        _ => None,
    }
}

fn trunc_unit(raw: &str) -> Option<Duration> {
    let u = raw
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .to_ascii_lowercase();
    match u.as_str() {
        "second" | "seconds" => Some(Duration::from_secs(1)),
        "minute" | "minutes" => Some(Duration::from_secs(60)),
        "hour" | "hours" => Some(Duration::from_secs(3600)),
        "day" | "days" => Some(Duration::from_secs(86400)),
        "week" | "weeks" => Some(Duration::from_secs(86400 * 7)),
        _ => None,
    }
}

fn interval_to_duration(raw: &str) -> Option<Duration> {
    let u = raw.trim().trim_matches('\'').to_ascii_lowercase();
    if let Some(rest) = u.strip_prefix("interval ") {
        return trunc_unit(rest.split_whitespace().last().unwrap_or(rest));
    }
    trunc_unit(&u)
}

/// Legal CREATE VIEW bodies (`is_view = true`). Cartesian of filter × derive
/// count, plus extra WHERE shapes. Star is required.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct ViewLegal {
    pub name: &'static str,
    pub sql: &'static str,
    pub has_filter: bool,
    pub derive_count: usize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct ViewIllegal {
    pub name: &'static str,
    pub sql: &'static str,
    pub needle: &'static str,
}

#[cfg(test)]
pub(crate) const VIEW_LEGAL: &[ViewLegal] = &[
    ViewLegal {
        name: "star",
        sql: "SELECT * FROM invoices.orders",
        has_filter: false,
        derive_count: 0,
    },
    ViewLegal {
        name: "star_filter",
        sql: "SELECT * FROM invoices.orders WHERE amount > 0",
        has_filter: true,
        derive_count: 0,
    },
    ViewLegal {
        name: "star_derive",
        sql: "SELECT *, amount * 2 AS doubled FROM invoices.orders",
        has_filter: false,
        derive_count: 1,
    },
    ViewLegal {
        name: "star_filter_derive",
        sql: "SELECT *, amount * 2 AS doubled FROM invoices.orders WHERE amount > 0",
        has_filter: true,
        derive_count: 1,
    },
    ViewLegal {
        name: "star_two_derives",
        sql: "SELECT *, amount * 2 AS doubled, amount + 1 AS bumped FROM invoices.orders",
        has_filter: false,
        derive_count: 2,
    },
    ViewLegal {
        name: "star_filter_two_derives",
        sql: "SELECT *, amount * 2 AS doubled, amount + 1 AS bumped FROM invoices.orders WHERE amount > 0",
        has_filter: true,
        derive_count: 2,
    },
    ViewLegal {
        name: "qualified_star",
        sql: "SELECT invoices.orders.* FROM invoices.orders",
        has_filter: false,
        derive_count: 0,
    },
    ViewLegal {
        name: "filter_and",
        sql: "SELECT * FROM invoices.orders WHERE amount > 0 AND amount < 100",
        has_filter: true,
        derive_count: 0,
    },
    ViewLegal {
        name: "filter_or",
        sql: "SELECT * FROM invoices.orders WHERE amount > 0 OR amount < 0",
        has_filter: true,
        derive_count: 0,
    },
    ViewLegal {
        name: "filter_paren",
        sql: "SELECT * FROM invoices.orders WHERE (amount > 0)",
        has_filter: true,
        derive_count: 0,
    },
];

#[cfg(test)]
pub(crate) const VIEW_ILLEGAL: &[ViewIllegal] = &[
    ViewIllegal {
        name: "no_star",
        sql: "SELECT amount FROM invoices.orders",
        needle: "SELECT *",
    },
    ViewIllegal {
        name: "column_list",
        sql: "SELECT customer_id, amount FROM invoices.orders",
        needle: "SELECT *",
    },
    ViewIllegal {
        name: "unaliased_derive",
        sql: "SELECT *, amount * 2 FROM invoices.orders",
        needle: "named",
    },
    ViewIllegal {
        name: "group_by",
        sql: "SELECT * FROM invoices.orders GROUP BY customer_id",
        needle: "GROUP BY",
    },
    ViewIllegal {
        name: "distinct",
        sql: "SELECT DISTINCT * FROM invoices.orders",
        needle: "DISTINCT",
    },
    ViewIllegal {
        name: "distinct_on",
        sql: "SELECT DISTINCT ON (customer_id) * FROM invoices.orders ORDER BY customer_id, created_at DESC",
        needle: "DISTINCT",
    },
    ViewIllegal {
        name: "join",
        sql: "SELECT * FROM invoices.orders o JOIN invoices.other t ON o.customer_id = t.customer_id",
        needle: "JOIN",
    },
    ViewIllegal {
        name: "sum",
        sql: "SELECT SUM(amount) AS invoice_total FROM invoices.orders",
        needle: "aggregate",
    },
    ViewIllegal {
        name: "count",
        sql: "SELECT COUNT(*) AS n FROM invoices.orders",
        needle: "aggregate",
    },
    ViewIllegal {
        name: "avg",
        sql: "SELECT AVG(amount) AS avg_amount FROM invoices.orders",
        needle: "aggregate",
    },
    ViewIllegal {
        name: "over",
        sql: "SELECT SUM(amount) OVER (PARTITION BY customer_id) AS invoice_total FROM invoices.orders",
        needle: "aggregate",
    },
    ViewIllegal {
        name: "from_source",
        sql: "SELECT * FROM source",
        needle: "source",
    },
    ViewIllegal {
        name: "from_source_suffix",
        sql: "SELECT * FROM invoices._source",
        needle: "source",
    },
    ViewIllegal {
        name: "cross_pipeline",
        sql: "SELECT * FROM crm.accounts",
        needle: "ad-hoc",
    },
    ViewIllegal {
        name: "union",
        sql: "SELECT * FROM invoices.orders UNION SELECT * FROM invoices.orders",
        needle: "UNION",
    },
    ViewIllegal {
        name: "except",
        sql: "SELECT * FROM invoices.orders EXCEPT SELECT * FROM invoices.orders",
        needle: "EXCEPT",
    },
    ViewIllegal {
        name: "intersect",
        sql: "SELECT * FROM invoices.orders INTERSECT SELECT * FROM invoices.orders",
        needle: "INTERSECT",
    },
    ViewIllegal {
        name: "having",
        sql: "SELECT * FROM invoices.orders HAVING amount > 0",
        needle: "HAVING",
    },
    ViewIllegal {
        name: "limit",
        sql: "SELECT * FROM invoices.orders LIMIT 1",
        needle: "LIMIT",
    },
    ViewIllegal {
        name: "offset",
        sql: "SELECT * FROM invoices.orders OFFSET 1",
        needle: "OFFSET",
    },
    ViewIllegal {
        name: "order_by",
        sql: "SELECT * FROM invoices.orders ORDER BY amount",
        needle: "ORDER BY",
    },
    ViewIllegal {
        name: "with_cte",
        sql: "WITH src AS (SELECT * FROM invoices.orders) SELECT * FROM src",
        needle: "WITH",
    },
    ViewIllegal {
        name: "subquery_from",
        sql: "SELECT * FROM (SELECT * FROM invoices.orders) AS t",
        needle: "FROM",
    },
    ViewIllegal {
        name: "two_from",
        sql: "SELECT * FROM invoices.orders, invoices.orders",
        needle: "one relation",
    },
    ViewIllegal {
        name: "hop",
        sql: "SELECT *, HOP(created_at, '1 hour') AS w FROM invoices.orders",
        needle: "hop",
    },
    ViewIllegal {
        name: "fetch",
        sql: "SELECT * FROM invoices.orders FETCH FIRST 1 ROW ONLY",
        needle: "FETCH",
    },
    ViewIllegal {
        name: "for_update",
        sql: "SELECT * FROM invoices.orders FOR UPDATE",
        needle: "FOR UPDATE",
    },
    ViewIllegal {
        name: "named_window",
        sql: "SELECT * FROM invoices.orders WINDOW w AS (PARTITION BY customer_id)",
        needle: "WINDOW",
    },
    ViewIllegal {
        name: "qualify",
        sql: "SELECT * FROM invoices.orders QUALIFY amount > 0",
        needle: "QUALIFY",
    },
    ViewIllegal {
        name: "lateral",
        sql: "SELECT * FROM invoices.orders LATERAL VIEW explode(amount) t AS x",
        needle: "LATERAL",
    },
    ViewIllegal {
        name: "into",
        sql: "SELECT * INTO tmp FROM invoices.orders",
        needle: "INTO",
    },
    ViewIllegal {
        name: "top",
        sql: "SELECT TOP 1 * FROM invoices.orders",
        needle: "TOP",
    },
    ViewIllegal {
        name: "prewhere",
        sql: "SELECT * FROM invoices.orders PREWHERE amount > 0",
        needle: "trailing",
    },
    ViewIllegal {
        name: "cluster_by",
        sql: "SELECT * FROM invoices.orders CLUSTER BY customer_id",
        needle: "CLUSTER",
    },
    ViewIllegal {
        name: "sort_by",
        sql: "SELECT * FROM invoices.orders SORT BY amount",
        needle: "SORT",
    },
    ViewIllegal {
        name: "for_xml",
        sql: "SELECT * FROM invoices.orders FOR XML AUTO",
        needle: "FOR XML",
    },
    ViewIllegal {
        name: "connect_by",
        sql: "SELECT * FROM invoices.orders CONNECT BY customer_id = customer_id",
        needle: "CONNECT BY",
    },
    ViewIllegal {
        name: "tablesample",
        sql: "SELECT * FROM invoices.orders TABLESAMPLE SYSTEM (10)",
        needle: "TABLESAMPLE",
    },
    ViewIllegal {
        name: "star_alias",
        sql: "SELECT * AS foo FROM invoices.orders",
        needle: "SELECT * options",
    },
    ViewIllegal {
        name: "semicolon_then_select",
        sql: "SELECT * FROM invoices.orders; SELECT 1",
        needle: "trailing",
    },
    ViewIllegal {
        name: "from_col_list",
        sql: "SELECT * FROM invoices.orders AS t(customer_id, amount, created_at)",
        needle: "alias column",
    },
    ViewIllegal {
        name: "with_ordinality",
        sql: "SELECT * FROM invoices.orders WITH ORDINALITY",
        needle: "ORDINALITY",
    },
];

#[cfg(test)]
pub(crate) const VIEW_ILLEGAL_REQUIRED: &[&str] = &[
    "no_star",
    "column_list",
    "unaliased_derive",
    "group_by",
    "distinct",
    "distinct_on",
    "join",
    "sum",
    "count",
    "avg",
    "over",
    "from_source",
    "from_source_suffix",
    "cross_pipeline",
    "union",
    "except",
    "intersect",
    "having",
    "limit",
    "offset",
    "order_by",
    "with_cte",
    "subquery_from",
    "two_from",
    "hop",
    "fetch",
    "for_update",
    "named_window",
    "qualify",
    "lateral",
    "into",
    "top",
    "prewhere",
    "cluster_by",
    "sort_by",
    "for_xml",
    "connect_by",
    "tablesample",
    "star_alias",
    "semicolon_then_select",
    "from_col_list",
    "with_ordinality",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn fqn() -> Fqn {
        Fqn::from_parts("invoices", "customer_totals").unwrap()
    }

    #[test]
    fn keyed_group_by() {
        let got = desugar_create(
            fqn(),
            "SELECT customer_id, SUM(amount) AS invoice_total, COUNT(*) AS invoice_count FROM invoices.orders GROUP BY customer_id",
            false,
        )
        .unwrap();
        match got {
            UnboundCompute::KeyedTable { keys, measures, .. } => {
                assert_eq!(keys, vec!["customer_id"]);
                assert_eq!(measures.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn avg_desugars_to_sum_and_count() {
        let got = desugar_create(
            fqn(),
            "SELECT customer_id, AVG(amount) AS avg_amount FROM invoices.orders GROUP BY customer_id",
            false,
        )
        .unwrap();
        match got {
            UnboundCompute::KeyedTable { measures, .. } => {
                assert_eq!(measures.len(), 2);
                assert!(matches!(measures[0].kind, UnboundMeasureKind::Sum(_)));
                assert!(matches!(measures[1].kind, UnboundMeasureKind::Count));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn date_trunc_window() {
        let got = desugar_create(
            Fqn::from_parts("invoices", "hourly_totals").unwrap(),
            "SELECT customer_id, date_trunc('hour', created_at) AS window_start, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id, date_trunc('hour', created_at)",
            false,
        )
        .unwrap();
        match got {
            UnboundCompute::WindowedTable {
                trunc_unit,
                event_time,
                ..
            } => {
                assert_eq!(trunc_unit, Duration::from_secs(3600));
                assert_eq!(event_time, "created_at");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tumble_sugar_same_as_date_trunc() {
        let a = desugar_create(
            Fqn::from_parts("invoices", "hourly_totals").unwrap(),
            "SELECT customer_id, date_trunc('hour', created_at), SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id, date_trunc('hour', created_at)",
            false,
        )
        .unwrap();
        let b = desugar_create(
            Fqn::from_parts("invoices", "hourly_totals").unwrap(),
            "SELECT customer_id, TUMBLE(created_at, 'hour'), SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id, TUMBLE(created_at, 'hour')",
            false,
        )
        .unwrap();
        match (a, b) {
            (
                UnboundCompute::WindowedTable {
                    trunc_unit: u1,
                    event_time: t1,
                    ..
                },
                UnboundCompute::WindowedTable {
                    trunc_unit: u2,
                    event_time: t2,
                    ..
                },
            ) => {
                assert_eq!(u1, u2);
                assert_eq!(t1, t2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn distinct_on_is_latest() {
        let got = desugar_create(
            Fqn::from_parts("invoices", "customers").unwrap(),
            "SELECT DISTINCT ON (customer_id) * FROM invoices.orders ORDER BY customer_id, created_at DESC",
            false,
        )
        .unwrap();
        match got {
            UnboundCompute::Latest { keys, order, .. } => {
                assert_eq!(keys, vec!["customer_id"]);
                assert_eq!(
                    order,
                    vec![("customer_id".into(), false), ("created_at".into(), true)]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn distinct_on_without_order_by_fails() {
        let err = desugar_create(
            Fqn::from_parts("invoices", "customers").unwrap(),
            "SELECT DISTINCT ON (customer_id) * FROM invoices.orders",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("ORDER BY"), "{err}");
    }

    #[test]
    fn join_in_create_fails() {
        let err = desugar_create(
            Fqn::from_parts("invoices", "enriched").unwrap(),
            "SELECT o.customer_id, SUM(o.amount) OVER (PARTITION BY o.customer_id) AS invoice_total FROM invoices.orders o JOIN invoices.other t ON o.customer_id = t.customer_id",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("JOIN"), "{err}");
    }

    #[test]
    fn hop_function_fails_closed() {
        let err = desugar_create(
            Fqn::from_parts("invoices", "hourly").unwrap(),
            "SELECT customer_id, HOP(created_at, '1 hour'), SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id, HOP(created_at, '1 hour')",
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("hop") || err.to_string().contains("HOP"),
            "{err}"
        );
    }

    #[test]
    fn banned_sql_walks_ast() {
        let prod = include_str!("parse.rs")
            .split("\nmod tests {")
            .next()
            .expect("production module");
        assert!(prod.contains("fn reject_banned_ast"));
        assert!(!prod.contains("fn reject_banned_sql"));
    }

    fn view_fqn() -> Fqn {
        Fqn::from_parts("invoices", "positive").unwrap()
    }

    #[test]
    fn view_legal_cartesian_is_complete() {
        use std::collections::BTreeSet;
        let combos: BTreeSet<_> = VIEW_LEGAL
            .iter()
            .map(|c| (c.has_filter, c.derive_count))
            .collect();
        for filter in [false, true] {
            for n in 0..=2 {
                assert!(
                    combos.contains(&(filter, n)),
                    "missing CREATE VIEW legal perm filter={filter} derive={n}"
                );
            }
        }
        let names: BTreeSet<_> = VIEW_ILLEGAL.iter().map(|c| c.name).collect();
        for required in VIEW_ILLEGAL_REQUIRED {
            assert!(names.contains(required), "missing VIEW illegal {required}");
        }
    }

    #[test]
    fn view_legal_permutations() {
        for case in VIEW_LEGAL {
            let got = desugar_create(view_fqn(), case.sql, true)
                .unwrap_or_else(|err| panic!("{}: {err}", case.name));
            match got {
                UnboundCompute::View { select, .. } => {
                    assert!(select.star, "{}", case.name);
                    assert_eq!(select.filter.is_some(), case.has_filter, "{}", case.name);
                    assert_eq!(select.derive.len(), case.derive_count, "{}", case.name);
                }
                other => panic!("{}: {other:?}", case.name),
            }
        }
    }

    #[test]
    fn view_illegal_permutations() {
        for case in VIEW_ILLEGAL {
            let err = match desugar_create(view_fqn(), case.sql, true) {
                Err(err) => err.to_string(),
                Ok(got) => panic!("{} unexpectedly succeeded: {got:?}", case.name),
            };
            let msg = err;
            assert!(
                msg.to_ascii_lowercase()
                    .contains(&case.needle.to_ascii_lowercase()),
                "{}: expected {:?} in {msg}",
                case.name,
                case.needle
            );
        }
    }

    #[test]
    fn rejects_from_source() {
        let err = desugar_create(fqn(), "SELECT * FROM source", true).unwrap_err();
        assert!(err.to_string().contains("source"), "{err}");
    }

    #[test]
    fn rejects_group_by_less_sum() {
        let err = desugar_create(
            fqn(),
            "SELECT SUM(amount) AS invoice_total FROM invoices.orders",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("GROUP BY"), "{err}");
    }

    #[test]
    fn rejects_count_distinct() {
        let err = desugar_create(
            fqn(),
            "SELECT customer_id, COUNT(DISTINCT invoice_id) AS n FROM invoices.orders GROUP BY customer_id",
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("COUNT(DISTINCT") || err.to_string().contains("DISTINCT"),
            "{err}"
        );
    }

    #[test]
    fn rejects_cross_pipeline_create() {
        let err = desugar_create(
            fqn(),
            "SELECT customer_id, SUM(amount) AS invoice_total FROM crm.accounts GROUP BY customer_id",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("ad-hoc"), "{err}");
    }

    #[test]
    fn watermark_delay_parses_duration() {
        assert_eq!(
            parse_watermark_delay("5 minutes").unwrap(),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn canonicalize_sql_expr_rejects_trailing_tokens() {
        assert!(canonicalize_sql_expr("amount > 0").is_ok());
        let err = canonicalize_sql_expr("amount > 0; status = 'paid'").unwrap_err();
        assert!(err.to_string().contains("trailing tokens"), "{err}");
    }
}
