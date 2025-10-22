#[derive(Clone, Debug, Default)]
pub struct SqlPlan {
    pub sql: String,
}

/// Stub SQL planner using semantic model.
pub fn plan_sql(_question: &str, _namespace: Option<&str>) -> Result<SqlPlan, String> {
    Ok(SqlPlan { sql: String::new() })
}


