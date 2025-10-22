use super::model::{SemanticField, SemanticFieldRole, SemanticModel};
use crate::helpers::configuration::Config;
use crate::discover::stats::NamespaceStats;

fn classify_field(name: &str, stats: Option<&crate::discover::stats::FieldStats>) -> SemanticFieldRole {
    let n = name.to_lowercase();
    if n.ends_with("_id") || n == "id" { return SemanticFieldRole::Id; }
    if n.contains("timestamp") || n.contains("event_time") || n.contains("created_at") { return SemanticFieldRole::Timestamp; }
    if let Some(s) = stats {
        if s.min_numeric.is_some() || s.max_numeric.is_some() {
            if let Some(d) = s.approx_distinct { if d <= 32 { return SemanticFieldRole::Categorical; } }
            return SemanticFieldRole::Metric;
        }
        if s.max_len.unwrap_or(0) > 64 { return SemanticFieldRole::FreeText; }
        if let Some(d) = s.approx_distinct { if d <= 64 { return SemanticFieldRole::Categorical; } }
        return SemanticFieldRole::FreeText;
    }
    if n.contains("name") || n.contains("desc") || n.contains("text") { return SemanticFieldRole::FreeText; }
    SemanticFieldRole::Categorical
}

pub fn infer_semantic_model(namespace: &str) -> SemanticModel {
    let path = Config::get_stats_local_path(namespace);
    let ns_stats: Option<NamespaceStats> = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok());
    let mut fields: Vec<SemanticField> = Vec::new();
    if let Some(ns) = ns_stats {
        for (fname, fstats) in ns.fields.iter() {
            let role = classify_field(fname, Some(fstats));
            fields.push(SemanticField { name: fname.clone(), role });
        }
    }
    let mut model = SemanticModel { namespace: namespace.to_string(), fields, dimensions: Vec::new(), metrics: Vec::new() };
    for f in &model.fields {
        match f.role {
            SemanticFieldRole::Id | SemanticFieldRole::Timestamp | SemanticFieldRole::Categorical => model.dimensions.push(f.name.clone()),
            SemanticFieldRole::Metric => model.metrics.push(f.name.clone()),
            SemanticFieldRole::FreeText => {}
        }
    }
    model
}


