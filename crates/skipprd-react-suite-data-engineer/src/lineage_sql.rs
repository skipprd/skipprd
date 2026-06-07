use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use datafusion::sql::sqlparser::ast::{
    CreateTable, CreateView, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, Join,
    JoinConstraint, JoinOperator, ObjectName, Query, Select, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, Statement, TableFactor, TableWithJoins,
};
use datafusion::sql::sqlparser::dialect::{BigQueryDialect, GenericDialect, MsSqlDialect, SnowflakeDialect};
use datafusion::sql::sqlparser::parser::Parser;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SqlLineageAnalysis {
    #[serde(default)]
    pub tables: Vec<String>,
    #[serde(default)]
    pub output_tables: Vec<String>,
    #[serde(default)]
    pub table_aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub selected_fields: Vec<String>,
    #[serde(default)]
    pub selected_outputs: Vec<SqlSelectedOutput>,
    #[serde(default)]
    pub aggregate_fields: Vec<String>,
    #[serde(default)]
    pub join_fields: Vec<String>,
    #[serde(default)]
    pub filter_fields: Vec<String>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct SqlSelectedOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_field: Option<String>,
    #[serde(default)]
    pub source_fields: Vec<String>,
    #[serde(default)]
    pub aggregate: bool,
    #[serde(default)]
    pub wildcard: bool,
}

pub fn analyze_select_sql(sql: &str) -> SqlLineageAnalysis {
    let mut analysis = SqlLineageAnalysis::default();
    let statements = parse_sql_with_supported_dialects(sql, &mut analysis);

    for statement in statements {
        match statement {
            Statement::Query(query) => visit_query(&query, &mut analysis),
            Statement::CreateTable(CreateTable { name, query, .. }) => {
                analysis.output_tables.push(name_to_string(&name));
                if let Some(query) = query.as_deref() {
                    visit_query(query, &mut analysis);
                }
            }
            Statement::CreateView(CreateView { name, query, .. }) => {
                analysis.output_tables.push(name_to_string(&name));
                visit_query(&query, &mut analysis);
            }
            other => analysis
                .diagnostics
                .push(format!("non-query statement ignored for lineage: {other}")),
        }
    }
    analysis.sort_dedup();
    analysis
}

impl SqlLineageAnalysis {
    fn sort_dedup(&mut self) {
        self.tables = sorted_unique(std::mem::take(&mut self.tables));
        self.output_tables = sorted_unique(std::mem::take(&mut self.output_tables));
        self.table_aliases = std::mem::take(&mut self.table_aliases)
            .into_iter()
            .map(|(alias, table)| {
                (
                    normalize_ident_part(&alias),
                    normalize_relation_name(&table),
                )
            })
            .collect();
        self.selected_fields = sorted_unique(std::mem::take(&mut self.selected_fields));
        self.selected_outputs = std::mem::take(&mut self.selected_outputs)
            .into_iter()
            .map(|mut output| {
                output.output_field = output
                    .output_field
                    .map(|field| normalize_field_path(&field))
                    .filter(|field| !field.is_empty());
                output.source_fields = sorted_unique(output.source_fields);
                output
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        self.aggregate_fields = sorted_unique(std::mem::take(&mut self.aggregate_fields));
        self.join_fields = sorted_unique(std::mem::take(&mut self.join_fields));
        self.filter_fields = sorted_unique(std::mem::take(&mut self.filter_fields));
        self.diagnostics.sort();
        self.diagnostics.dedup();
    }
}

fn visit_query(query: &Query, out: &mut SqlLineageAnalysis) {
    visit_set_expr(&query.body, out);
}

fn visit_set_expr(set_expr: &SetExpr, out: &mut SqlLineageAnalysis) {
    match set_expr {
        SetExpr::Select(select) => visit_select(select, out),
        SetExpr::Query(query) => visit_query(query, out),
        SetExpr::SetOperation { left, right, .. } => {
            visit_set_expr(left, out);
            visit_set_expr(right, out);
        }
        _ => {}
    }
}

fn parse_sql_with_supported_dialects(
    sql: &str,
    analysis: &mut SqlLineageAnalysis,
) -> Vec<Statement> {
    let generic = GenericDialect {};
    match Parser::parse_sql(&generic, sql) {
        Ok(statements) => return statements,
        Err(first_error) => {
            let bigquery = BigQueryDialect {};
            if let Ok(statements) = Parser::parse_sql(&bigquery, sql) {
                return statements;
            }
            let snowflake = SnowflakeDialect {};
            if let Ok(statements) = Parser::parse_sql(&snowflake, sql) {
                return statements;
            }
            let mssql = MsSqlDialect {};
            if let Ok(statements) = Parser::parse_sql(&mssql, sql) {
                return statements;
            }
            analysis
                .diagnostics
                .push(format!("failed to parse SQL for lineage: {first_error}"));
            Vec::new()
        }
    }
}

fn visit_select(select: &Select, out: &mut SqlLineageAnalysis) {
    for table in &select.from {
        visit_table_with_joins(table, out);
    }

    for item in &select.projection {
        match item {
            SelectItem::UnnamedExpr(expr) => {
                collect_expr_fields(expr, &out.table_aliases, &mut out.selected_fields);
                collect_aggregate_fields(expr, &out.table_aliases, &mut out.aggregate_fields);
                out.selected_outputs.push(selected_output_from_expr(
                    expr,
                    None,
                    &out.table_aliases,
                ));
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                collect_expr_fields(expr, &out.table_aliases, &mut out.selected_fields);
                collect_aggregate_fields(expr, &out.table_aliases, &mut out.aggregate_fields);
                out.selected_outputs.push(selected_output_from_expr(
                    expr,
                    Some(&alias.value),
                    &out.table_aliases,
                ));
            }
            SelectItem::QualifiedWildcard(kind, _) => {
                let relation = match kind {
                    SelectItemQualifiedWildcardKind::ObjectName(name) => {
                        resolve_relation_or_alias(&name_to_string(name), &out.table_aliases)
                    }
                    SelectItemQualifiedWildcardKind::Expr(expr) => {
                        collect_expr_fields(expr, &out.table_aliases, &mut out.selected_fields);
                        "*".to_string()
                    }
                };
                out.selected_fields.push(relation.clone());
                out.selected_outputs.push(SqlSelectedOutput {
                    output_field: None,
                    source_fields: vec![format!("{relation}.*")],
                    aggregate: false,
                    wildcard: true,
                });
            }
            SelectItem::Wildcard(_) => {
                out.selected_fields.push("*".to_string());
                out.selected_outputs.push(SqlSelectedOutput {
                    output_field: None,
                    source_fields: vec!["*".to_string()],
                    aggregate: false,
                    wildcard: true,
                });
            }
        }
    }

    if let Some(selection) = select.selection.as_ref() {
        collect_expr_fields(selection, &out.table_aliases, &mut out.filter_fields);
    }
}

fn selected_output_from_expr(
    expr: &Expr,
    alias: Option<&str>,
    aliases: &BTreeMap<String, String>,
) -> SqlSelectedOutput {
    let mut source_fields = Vec::new();
    collect_expr_fields(expr, aliases, &mut source_fields);
    let mut aggregate_fields = Vec::new();
    collect_aggregate_fields(expr, aliases, &mut aggregate_fields);
    SqlSelectedOutput {
        output_field: alias
            .map(normalize_field_path)
            .filter(|field| !field.is_empty())
            .or_else(|| inferred_output_field(expr)),
        source_fields,
        aggregate: !aggregate_fields.is_empty(),
        wildcard: false,
    }
}

fn inferred_output_field(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(normalize_field_path(&ident.value)),
        Expr::CompoundIdentifier(idents) => idents
            .last()
            .map(|ident| normalize_field_path(&ident.value))
            .filter(|field| !field.is_empty()),
        Expr::Nested(expr) | Expr::Cast { expr, .. } => inferred_output_field(expr),
        _ => None,
    }
}

fn visit_table_with_joins(table: &TableWithJoins, out: &mut SqlLineageAnalysis) {
    visit_table_factor(&table.relation, out);
    for join in &table.joins {
        visit_join(join, out);
    }
}

fn visit_join(join: &Join, out: &mut SqlLineageAnalysis) {
    visit_table_factor(&join.relation, out);
    collect_join_operator_fields(&join.join_operator, out);
}

fn visit_table_factor(factor: &TableFactor, out: &mut SqlLineageAnalysis) {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let table = name_to_string(name);
            if let Some(alias) = alias {
                out.table_aliases
                    .insert(normalize_ident_part(&alias.name.value), table.clone());
            }
            out.tables.push(table);
        }
        TableFactor::Derived { subquery, .. } => visit_query(subquery, out),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => visit_table_with_joins(table_with_joins, out),
        _ => {}
    }
}

fn collect_join_operator_fields(operator: &JoinOperator, out: &mut SqlLineageAnalysis) {
    let constraint = match operator {
        JoinOperator::Inner(constraint)
        | JoinOperator::LeftOuter(constraint)
        | JoinOperator::RightOuter(constraint)
        | JoinOperator::FullOuter(constraint)
        | JoinOperator::LeftSemi(constraint)
        | JoinOperator::RightSemi(constraint)
        | JoinOperator::LeftAnti(constraint)
        | JoinOperator::RightAnti(constraint) => constraint,
        _ => return,
    };
    match constraint {
        JoinConstraint::On(expr) => {
            collect_expr_fields(&expr, &out.table_aliases, &mut out.join_fields)
        }
        JoinConstraint::Using(object_names) => {
            out.join_fields
                .extend(object_names.iter().map(|name| name_to_string(name)));
        }
        _ => {}
    }
}

fn collect_aggregate_fields(
    expr: &Expr,
    aliases: &BTreeMap<String, String>,
    out: &mut Vec<String>,
) {
    match expr {
        Expr::Function(func) => {
            if is_aggregate_function(&func.name.to_string()) {
                for arg in function_args(&func.args) {
                    collect_function_arg_fields(arg, aliases, out);
                }
            } else {
                for arg in function_args(&func.args) {
                    if let Some(expr) = function_arg_expr(arg) {
                        collect_aggregate_fields(expr, aliases, out);
                    }
                }
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_aggregate_fields(left, aliases, out);
            collect_aggregate_fields(right, aliases, out);
        }
        Expr::Nested(expr) | Expr::Cast { expr, .. } => collect_aggregate_fields(expr, aliases, out),
        _ => {}
    }
}

fn collect_function_arg_fields(
    arg: &FunctionArg,
    aliases: &BTreeMap<String, String>,
    out: &mut Vec<String>,
) {
    if let Some(expr) = function_arg_expr(arg) {
        collect_expr_fields(expr, aliases, out);
    }
}

fn function_arg_expr(arg: &FunctionArg) -> Option<&Expr> {
    match arg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(expr),
        FunctionArg::Named {
            arg: FunctionArgExpr::Expr(expr),
            ..
        } => Some(expr),
        _ => None,
    }
}

fn collect_expr_fields(expr: &Expr, aliases: &BTreeMap<String, String>, out: &mut Vec<String>) {
    match expr {
        Expr::Identifier(ident) => out.push(normalize_ident_part(&ident.value)),
        Expr::CompoundIdentifier(idents) => {
            out.push(resolve_compound_identifier(idents, aliases));
        }
        Expr::Function(func) => {
            for arg in function_args(&func.args) {
                collect_function_arg_fields(arg, aliases, out);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_expr_fields(left, aliases, out);
            collect_expr_fields(right, aliases, out);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr) => collect_expr_fields(expr, aliases, out),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_expr_fields(expr, aliases, out);
            collect_expr_fields(low, aliases, out);
            collect_expr_fields(high, aliases, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_expr_fields(expr, aliases, out);
            for item in list {
                collect_expr_fields(item, aliases, out);
            }
        }
        Expr::InSubquery { expr, subquery, .. } => {
            collect_expr_fields(expr, aliases, out);
            visit_query(subquery, &mut SqlLineageAnalysis::default());
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                collect_expr_fields(operand, aliases, out);
            }
            for case_when in conditions {
                collect_expr_fields(&case_when.condition, aliases, out);
                collect_expr_fields(&case_when.result, aliases, out);
            }
            if let Some(result) = else_result {
                collect_expr_fields(result, aliases, out);
            }
        }
        _ => {}
    }
}

fn is_aggregate_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "sum"
            | "avg"
            | "count"
            | "min"
            | "max"
            | "median"
            | "stddev"
            | "stddev_pop"
            | "stddev_samp"
            | "variance"
            | "var_pop"
            | "var_samp"
    )
}

fn function_args(args: &FunctionArguments) -> &[FunctionArg] {
    match args {
        FunctionArguments::List(list) => &list.args,
        _ => &[],
    }
}

fn name_to_string(name: &ObjectName) -> String {
    normalize_relation_name(
        &name
            .0
            .iter()
            .filter_map(|part| part.as_ident().map(|ident| ident.value.as_str()))
            .collect::<Vec<_>>()
            .join("."),
    )
}

fn resolve_compound_identifier(
    idents: &[Ident],
    aliases: &BTreeMap<String, String>,
) -> String {
    let parts = idents
        .iter()
        .map(|ident| normalize_ident_part(&ident.value))
        .collect::<Vec<_>>();
    if let Some((first, rest)) = parts.split_first() {
        if let Some(table) = aliases.get(first) {
            if rest.is_empty() {
                return table.clone();
            }
            return format!("{}.{}", table, rest.join("."));
        }
    }
    parts.join(".")
}

fn resolve_relation_or_alias(value: &str, aliases: &BTreeMap<String, String>) -> String {
    aliases
        .get(&normalize_ident_part(value))
        .cloned()
        .unwrap_or_else(|| normalize_relation_name(value))
}

fn normalize_relation_name(value: &str) -> String {
    value
        .split('.')
        .map(normalize_ident_part)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(".")
}

fn normalize_field_path(value: &str) -> String {
    value
        .split('.')
        .map(normalize_ident_part)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(".")
}

fn normalize_ident_part(value: &str) -> String {
    value
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

fn sorted_unique(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tables_columns_and_aggregates() {
        let got = analyze_select_sql(
            "select c.customer_id, sum(o.amount) as total from db.sales.orders o join db.sales.customers c on o.customer_id = c.customer_id where o.status = 'paid' group by c.customer_id",
        );
        assert!(got.tables.contains(&"db.sales.orders".to_string()));
        assert!(got.tables.contains(&"db.sales.customers".to_string()));
        assert!(got
            .selected_fields
            .contains(&"db.sales.customers.customer_id".to_string()));
        assert!(got
            .aggregate_fields
            .contains(&"db.sales.orders.amount".to_string()));
        assert!(got.selected_outputs.iter().any(|output| {
            output.output_field.as_deref() == Some("total")
                && output
                    .source_fields
                    .contains(&"db.sales.orders.amount".to_string())
                && output.aggregate
        }));
    }

    #[test]
    fn parse_errors_are_diagnostics_not_panics() {
        let got = analyze_select_sql("select (");
        assert!(!got.diagnostics.is_empty());
    }

    #[test]
    fn normalizes_provider_specific_quoting() {
        let bigquery = analyze_select_sql("select o.`Amount` from `proj.ds.orders` o");
        assert!(bigquery.tables.contains(&"proj.ds.orders".to_string()));
        assert!(bigquery
            .selected_fields
            .contains(&"proj.ds.orders.amount".to_string()));

        let snowflake = analyze_select_sql(
            "select c.\"CustomerId\" from \"ANALYTICS\".\"RAW\".\"CUSTOMERS\" c",
        );
        assert!(snowflake
            .tables
            .contains(&"analytics.raw.customers".to_string()));

        let mssql = analyze_select_sql("select c.[CustomerId] from [dbo].[Customers] c");
        assert!(mssql.tables.contains(&"dbo.customers".to_string()));
    }

    #[test]
    fn extracts_create_table_as_select_lineage() {
        let got = analyze_select_sql(
            "create table analytics.gold.orders as select order_id from analytics.silver.orders",
        );
        assert!(got
            .output_tables
            .contains(&"analytics.gold.orders".to_string()));
        assert!(got.tables.contains(&"analytics.silver.orders".to_string()));
    }

    #[test]
    fn extracts_create_view_as_select_lineage() {
        let got = analyze_select_sql(
            "create or replace view analytics.gold.orders as (select order_id from analytics.silver.orders)",
        );
        assert!(got
            .output_tables
            .contains(&"analytics.gold.orders".to_string()));
        assert!(got.tables.contains(&"analytics.silver.orders".to_string()));
    }
}
