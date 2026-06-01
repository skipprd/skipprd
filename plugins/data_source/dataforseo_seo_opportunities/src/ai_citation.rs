use serde_json::{json, Value};

pub fn detect_ai_citation_opportunities(
    site: &str,
    run_date: &str,
    own_domain: &str,
    keyword: &str,
    serp_features: &[Value],
    serp_results: &[Value],
    location_code: u32,
    language_code: &str,
    device: &str,
) -> Vec<Value> {
    let mut rows = Vec::new();
    let query_intent = infer_query_intent(keyword);
    let answer_surfaces = collect_answer_surfaces(serp_features);
    let cited_domains = collect_cited_domains(serp_features, serp_results);
    let missing_brand = !cited_domains.iter().any(|d| d == own_domain);

    if answer_surfaces.is_empty() && !is_answer_oriented(keyword) {
        return rows;
    }

    let citation_gap_score = compute_citation_gap_score(
        &answer_surfaces,
        &cited_domains,
        own_domain,
        keyword,
    );

    if citation_gap_score < 15.0 {
        return rows;
    }

    rows.push(json!({
        "site": site,
        "run_date": run_date,
        "query": keyword,
        "opportunity_type": opportunity_type(keyword, &answer_surfaces),
        "answer_surface": answer_surfaces,
        "current_cited_domains": cited_domains,
        "missing_brand": missing_brand,
        "citation_gap_score": citation_gap_score,
        "query_intent": query_intent,
        "source_url_candidates": source_url_candidates(serp_results, own_domain),
        "needed_evidence_type": needed_evidence_type(keyword),
        "recommended_content_asset": recommended_content_asset(keyword),
        "confidence": confidence_from_score(citation_gap_score),
        "location_code": location_code,
        "language_code": language_code,
        "device": device,
    }));
    rows
}

pub fn ai_citation_score_from_features(
    keyword: &str,
    serp_features: &[Value],
    own_domain: &str,
) -> f64 {
    let answer_surfaces = collect_answer_surfaces(serp_features);
    let cited_domains = collect_cited_domains(serp_features, &[]);
    compute_citation_gap_score(&answer_surfaces, &cited_domains, own_domain, keyword)
}

fn collect_answer_surfaces(features: &[Value]) -> Vec<String> {
    features
        .iter()
        .filter_map(|f| f.get("feature_type").and_then(|v| v.as_str()))
        .map(str::to_string)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
}

fn collect_cited_domains(features: &[Value], results: &[Value]) -> Vec<String> {
    let mut domains = Vec::new();
    for feature in features {
        if let Some(domain) = feature.get("owned_by_domain").and_then(|v| v.as_str()) {
            if !domain.is_empty() {
                domains.push(domain.to_lowercase());
            }
        }
        if let Some(url) = feature.get("source_url").and_then(|v| v.as_str()) {
            if let Some(domain) = crate::target::domain_from_url(url) {
                domains.push(domain);
            }
        }
    }
    for result in results.iter().take(5) {
        if let Some(domain) = result.get("domain").and_then(|v| v.as_str()) {
            domains.push(domain.to_lowercase());
        }
    }
    domains.sort();
    domains.dedup();
    domains
}

fn compute_citation_gap_score(
    answer_surfaces: &[String],
    cited_domains: &[String],
    own_domain: &str,
    keyword: &str,
) -> f64 {
    let mut score: f64 = 0.0;
    if answer_surfaces
        .iter()
        .any(|s| s == "people_also_ask" || s == "featured_snippet" || s == "ai_overview")
    {
        score += 25.0;
    }
    if is_answer_oriented(keyword) {
        score += 20.0;
    }
    if !cited_domains.iter().any(|d| d == own_domain) {
        score += 15.0;
    }
    if cited_domains.len() <= 3 {
        score += 10.0;
    }
    score.min(100.0)
}

fn is_answer_oriented(keyword: &str) -> bool {
    let lower = keyword.to_lowercase();
    lower.starts_with("how ")
        || lower.starts_with("what ")
        || lower.starts_with("why ")
        || lower.starts_with("best ")
        || lower.contains(" vs ")
        || lower.contains('?')
}

fn infer_query_intent(keyword: &str) -> &'static str {
    let lower = keyword.to_lowercase();
    if lower.contains("buy") || lower.contains("pricing") || lower.starts_with("best ") {
        "commercial"
    } else if lower.starts_with("how ") || lower.starts_with("what ") {
        "informational"
    } else if lower.contains(" near ") {
        "local"
    } else {
        "informational"
    }
}

fn opportunity_type(keyword: &str, surfaces: &[String]) -> &'static str {
    if surfaces.iter().any(|s| s == "featured_snippet") {
        "featured_snippet"
    } else if surfaces.iter().any(|s| s == "people_also_ask") {
        "people_also_ask"
    } else if surfaces.iter().any(|s| s == "ai_overview") {
        "ai_overview"
    } else if keyword.to_lowercase().starts_with("best ") {
        "listicle"
    } else {
        "answer_engine"
    }
}

fn source_url_candidates(results: &[Value], own_domain: &str) -> Vec<String> {
    results
        .iter()
        .filter(|r| {
            r.get("domain")
                .and_then(|v| v.as_str())
                .map(|d| d != own_domain)
                .unwrap_or(true)
        })
        .filter_map(|r| r.get("url").and_then(|v| v.as_str()).map(str::to_string))
        .take(5)
        .collect()
}

fn needed_evidence_type(keyword: &str) -> &'static str {
    let lower = keyword.to_lowercase();
    if lower.contains("stat") || lower.contains("data") {
        "original_data"
    } else if lower.starts_with("best ") {
        "comparison_table"
    } else if lower.starts_with("how ") {
        "step_by_step"
    } else {
        "concise_answer"
    }
}

fn recommended_content_asset(keyword: &str) -> &'static str {
    let lower = keyword.to_lowercase();
    if lower.starts_with("best ") {
        "comparison_page"
    } else if lower.starts_with("how ") {
        "how_to_guide"
    } else if lower.contains('?') {
        "faq_hub"
    } else {
        "explainer_page"
    }
}

fn confidence_from_score(score: f64) -> &'static str {
    if score >= 60.0 {
        "high"
    } else if score >= 35.0 {
        "medium"
    } else {
        "low"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_paa_opportunity() {
        let features = vec![json!({
            "feature_type": "people_also_ask",
            "owned_by_domain": "example.com",
            "source_url": "https://example.com/answer"
        })];
        let rows = detect_ai_citation_opportunities(
            "picnic.com",
            "2026-05-30",
            "picnic.com",
            "how to use meal planning app",
            &features,
            &[],
            2840,
            "en",
            "desktop",
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0]["citation_gap_score"].as_f64().unwrap_or(0.0) > 0.0);
    }
}
