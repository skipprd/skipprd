use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

pub fn cluster_keywords_by_serp_overlap(
    site: &str,
    run_date: &str,
    keyword_domains: &HashMap<String, HashSet<String>>,
    keyword_volumes: &HashMap<String, u64>,
    location_code: u32,
    language_code: &str,
    device: &str,
) -> Vec<Value> {
    let keywords: Vec<String> = keyword_domains.keys().cloned().collect();
    let mut assigned: HashSet<String> = HashSet::new();
    let mut rows = Vec::new();
    let mut cluster_index = 0usize;

    for primary in &keywords {
        if assigned.contains(primary) {
            continue;
        }
        let primary_domains = keyword_domains
            .get(primary)
            .cloned()
            .unwrap_or_default();
        if primary_domains.is_empty() {
            continue;
        }
        cluster_index += 1;
        let cluster_id = format!("cluster_{cluster_index:04}");
        let cluster_label = primary.clone();
        let mut members = vec![primary.clone()];
        assigned.insert(primary.clone());

        for other in &keywords {
            if assigned.contains(other) || other == primary {
                continue;
            }
            let other_domains = keyword_domains
                .get(other)
                .cloned()
                .unwrap_or_default();
            let overlap = jaccard(&primary_domains, &other_domains);
            if overlap >= 0.35 {
                members.push(other.clone());
                assigned.insert(other.clone());
            }
        }

        let aggregate_volume: u64 = members
            .iter()
            .filter_map(|k| keyword_volumes.get(k))
            .sum();

        for keyword in members {
            let overlap_score = keyword_domains
                .get(&keyword)
                .map(|d| jaccard(&primary_domains, d))
                .unwrap_or(0.0);
            rows.push(json!({
                "site": site,
                "run_date": run_date,
                "cluster_id": cluster_id,
                "cluster_label": cluster_label,
                "primary_keyword": primary,
                "keyword": keyword,
                "cluster_method": "serp_overlap",
                "serp_overlap_score": overlap_score,
                "semantic_similarity_score": null,
                "aggregate_volume": aggregate_volume,
                "best_opportunity_score": null,
                "recommended_page_type": recommend_page_type(&keyword),
                "location_code": location_code,
                "language_code": language_code,
                "device": device,
            }));
        }
    }
    rows
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn recommend_page_type(keyword: &str) -> &'static str {
    let lower = keyword.to_lowercase();
    if lower.starts_with("how ") || lower.contains("guide") {
        "how_to"
    } else if lower.starts_with("best ") || lower.contains(" vs ") {
        "comparison"
    } else if lower.contains("?") {
        "faq"
    } else {
        "landing_page"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clusters_overlapping_keywords() {
        let mut domains = HashMap::new();
        domains.insert(
            "kw a".into(),
            ["a.com".into(), "b.com".into()].into_iter().collect(),
        );
        domains.insert(
            "kw b".into(),
            ["a.com".into(), "b.com".into(), "c.com".into()]
                .into_iter()
                .collect(),
        );
        domains.insert("kw c".into(), ["x.com".into()].into_iter().collect());
        let volumes = HashMap::from([
            ("kw a".into(), 100u64),
            ("kw b".into(), 200u64),
            ("kw c".into(), 50u64),
        ]);
        let rows = cluster_keywords_by_serp_overlap(
            "example.com",
            "2026-05-30",
            &domains,
            &volumes,
            2840,
            "en",
            "desktop",
        );
        assert!(rows.len() >= 3);
    }
}
