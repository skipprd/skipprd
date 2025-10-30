use std::collections::{BTreeMap, BTreeSet};

use datafusion::prelude::SessionContext;

// use crate::helpers::configuration::Config; // no longer needed here

#[derive(Clone, Debug, Default)]
pub struct Intent {
    pub entities: Vec<String>,
    pub measures: Vec<String>,
    pub filters: Vec<String>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
    pub aggregation: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct CandidateDataset {
    pub namespace: String,
    pub fields: BTreeSet<String>,
    pub root_description: Option<String>,
    pub score_hint: f32,
}

#[derive(Clone, Debug, Default)]
pub struct JoinEdge {
    pub left_ns: String,
    pub right_ns: String,
    pub left_key: String,
    pub right_key: String,
}

#[derive(Clone, Debug, Default)]
pub struct PlanContext {
    pub candidates: Vec<CandidateDataset>,
    pub joins: Vec<JoinEdge>,
}

/// SQL prefilter over catalog to shortlist namespaces and fields.
pub async fn shortlist_candidates(ctx: &SessionContext, user_q: &str, limit: usize) -> Vec<CandidateDataset> {
    // Register semantic/catalog
    crate::sql::query::register_catalog(ctx).await;
    println!("{} ASK: loading catalog/semantic for candidate search", chrono::Utc::now().to_rfc3339());

    // Tokenize query and build broad OR filter across tokens
    let q = user_q.to_lowercase();
    let mut tokens: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|s| s.to_string())
        .collect();
    // simple stopword filter
    let stop: std::collections::BTreeSet<&str> = [
        "the","and","for","are","was","were","what","whats","who","when","where","why","how",
        "this","that","with","from","into","most","type","types","kind","kinds","of","to","in"
    ].into_iter().collect();
    tokens.retain(|t| !stop.contains(t.as_str()));
    tokens.sort(); tokens.dedup();
    println!("{} PLANNER: tokens for candidate search = {}", chrono::Utc::now().to_rfc3339(), tokens.join(","));

    let filter = if tokens.is_empty() {
        // very permissive fallback: match anything, we'll cap later
        "TRUE".to_string()
    } else {
        let mut parts: Vec<String> = Vec::new();
        for t in &tokens {
            let t_esc = t.replace("'", "''");
            parts.push(format!("(lower(c.field) LIKE '%{0}%' OR lower(coalesce(c.description,'')) LIKE '%{0}%' OR lower(coalesce(c.synonyms,'')) LIKE '%{0}%')", t_esc));
        }
        parts.join(" OR ")
    };
    let sql = format!(
        "SELECT c.namespace, c.field, c.description, c.synonyms, coalesce(c.role,'') AS role, coalesce(c.dimensions,'') AS dims, coalesce(c.metrics,'') AS mets FROM catalog c WHERE {}",
        filter
    );
    let mut map: BTreeMap<String, CandidateDataset> = BTreeMap::new();
    if let Ok(df) = ctx.sql(&sql).await { if let Ok(batches) = df.collect().await { for b in batches { for row in 0..b.num_rows() {
        let ns = crate::sql::tui::value_to_string(b.column(0).as_ref(), row);
        let field = crate::sql::tui::value_to_string(b.column(1).as_ref(), row);
        let desc = crate::sql::tui::value_to_string(b.column(2).as_ref(), row);
        let syn = crate::sql::tui::value_to_string(b.column(3).as_ref(), row);
        let role = crate::sql::tui::value_to_string(b.column(4).as_ref(), row);
        let dims = crate::sql::tui::value_to_string(b.column(5).as_ref(), row);
        let mets = crate::sql::tui::value_to_string(b.column(6).as_ref(), row);

        let entry = map.entry(ns.clone()).or_insert_with(|| CandidateDataset { namespace: ns.clone(), fields: BTreeSet::new(), root_description: None, score_hint: 0.0 });
        entry.fields.insert(field);
        if !desc.is_empty() || !syn.is_empty() { entry.score_hint += 0.1; }
        if role.eq_ignore_ascii_case("Id") || role.eq_ignore_ascii_case("Categorical") || role.eq_ignore_ascii_case("Metric") { entry.score_hint += 0.05; }

        if !tokens.is_empty() {
            let dims_l = dims.to_lowercase();
            let mets_l = mets.to_lowercase();
            if tokens.iter().any(|t| dims_l.contains(t)) { entry.score_hint += 0.05; }
            if tokens.iter().any(|t| mets_l.contains(t)) { entry.score_hint += 0.05; }
        }
    } } } }


    // Enrich with dataset root-level description by querying the catalog table only (S3-backed)
    for (_, v) in map.iter_mut() {
        let sql_desc = format!("SELECT coalesce(description,'') FROM catalog WHERE namespace='{}' LIMIT 1", v.namespace.replace("'", "''"));
        if let Ok(df) = ctx.sql(&sql_desc).await { if let Ok(b) = df.collect().await { if let Some(first) = b.first() { let s = crate::sql::tui::value_to_string(first.column(0).as_ref(), 0); if !s.is_empty() { v.root_description = Some(s); v.score_hint += 0.2; } } } }
    }

    let mut vals: Vec<CandidateDataset> = map.into_values().collect();
    vals.sort_by(|a, b| b.score_hint.total_cmp(&a.score_hint));
    vals.truncate(limit);
    println!("{} ASK: candidate shortlist built (limit = {})", chrono::Utc::now().to_rfc3339(), limit);
    vals
}

/// Simple join inference: match identical field names with id-like roles across candidates.
pub async fn infer_joins(ctx: &SessionContext, cands: &[CandidateDataset]) -> Vec<JoinEdge> {
    // Build role map from unified catalog
    let mut role_map: BTreeMap<(String,String), String> = BTreeMap::new();
    if let Ok(df) = ctx.sql("SELECT namespace, field, coalesce(role,'') AS role FROM catalog").await { if let Ok(batches) = df.collect().await { for b in batches { for row in 0..b.num_rows() {
        let ns = crate::sql::tui::value_to_string(b.column(0).as_ref(), row);
        let f = crate::sql::tui::value_to_string(b.column(1).as_ref(), row);
        let r = crate::sql::tui::value_to_string(b.column(2).as_ref(), row);
        role_map.insert((ns, f), r);
    } } } }

    let mut edges: Vec<JoinEdge> = Vec::new();
    for i in 0..cands.len() {
        for j in (i+1)..cands.len() {
            let a = &cands[i];
            let b = &cands[j];
            for fa in &a.fields {
                if b.fields.contains(fa) {
                    let ra = role_map.get(&(a.namespace.clone(), fa.clone())).cloned().unwrap_or_default();
                    let rb = role_map.get(&(b.namespace.clone(), fa.clone())).cloned().unwrap_or_default();
                    if ra.eq_ignore_ascii_case("Id") || rb.eq_ignore_ascii_case("Id") || ra.eq_ignore_ascii_case("Categorical") || rb.eq_ignore_ascii_case("Categorical") {
                        edges.push(JoinEdge { left_ns: a.namespace.clone(), right_ns: b.namespace.clone(), left_key: fa.clone(), right_key: fa.clone() });
                    }
                }
            }
        }
    }
    edges
}


