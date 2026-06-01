use serde_json::{json, Value};

pub fn build_content_brief(
    site: &str,
    run_date: &str,
    cluster_id: &str,
    target_keyword: &str,
    secondary_keywords: &[String],
    opportunity_score: f64,
    location_code: u32,
    language_code: &str,
    device: &str,
) -> Value {
    let brief_id = format!("brief_{cluster_id}_{}", slugify(target_keyword));
    let page_type = if target_keyword.to_lowercase().starts_with("how ") {
        "how_to"
    } else if target_keyword.to_lowercase().starts_with("best ") {
        "comparison"
    } else {
        "landing_page"
    };
    json!({
        "site": site,
        "run_date": run_date,
        "brief_id": brief_id,
        "cluster_id": cluster_id,
        "target_keyword": target_keyword,
        "secondary_keywords": secondary_keywords,
        "recommended_url": format!("/{}", slugify(target_keyword)),
        "page_type": page_type,
        "title_recommendations": title_recommendations(target_keyword),
        "outline": outline_for_keyword(target_keyword),
        "questions_to_answer": questions_for_keyword(target_keyword),
        "schema_types": schema_types_for_keyword(target_keyword),
        "internal_link_targets": [],
        "external_citation_targets": [],
        "opportunity_score": opportunity_score,
        "location_code": location_code,
        "language_code": language_code,
        "device": device,
    })
}

fn slugify(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn title_recommendations(keyword: &str) -> Vec<String> {
    vec![
        format!("{keyword}: Complete Guide"),
        format!("Best Options for {keyword}"),
    ]
}

fn outline_for_keyword(keyword: &str) -> Vec<String> {
    vec![
        format!("What is {keyword}?"),
        format!("Why {keyword} matters"),
        "Key features to compare".into(),
        "Step-by-step recommendations".into(),
        "FAQ".into(),
    ]
}

fn questions_for_keyword(keyword: &str) -> Vec<String> {
    vec![
        format!("What is the best {keyword}?"),
        format!("How do I choose a {keyword}?"),
        format!("Is {keyword} worth it?"),
    ]
}

fn schema_types_for_keyword(keyword: &str) -> Vec<String> {
    if keyword.contains('?') || keyword.to_lowercase().starts_with("how ") {
        vec!["FAQPage".into(), "Article".into()]
    } else if keyword.to_lowercase().starts_with("best ") {
        vec!["ItemList".into(), "Article".into()]
    } else {
        vec!["Article".into()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_brief_with_schema() {
        let brief = build_content_brief(
            "picnic.com",
            "2026-05-30",
            "cluster_0001",
            "how to use meal planning app",
            &["meal planning app free".into()],
            72.0,
            2840,
            "en",
            "desktop",
        );
        assert!(brief["brief_id"].as_str().unwrap().starts_with("brief_"));
        assert!(brief["schema_types"].as_array().unwrap().len() >= 1);
    }
}
