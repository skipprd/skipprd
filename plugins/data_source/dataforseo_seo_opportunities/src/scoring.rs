use serde_json::{json, Value};

use crate::config::ScoringConfig;

#[derive(Debug, Clone, Default)]
pub struct OpportunityInputs {
    pub keyword: String,
    pub search_volume: u64,
    pub keyword_difficulty: Option<u32>,
    pub cpc: Option<f64>,
    pub weakness_score: f64,
    pub forum_count: u32,
    pub ugc_count: u32,
    pub low_authority_count: u32,
    pub is_question: bool,
    pub intent: Option<String>,
    pub kgr: Option<f64>,
    pub allintitle_count: Option<u64>,
    pub own_rank: Option<u32>,
    pub has_matching_page: bool,
    pub ai_citation_score: f64,
}

pub fn compute_opportunity_score(
    site: &str,
    run_date: &str,
    inputs: &OpportunityInputs,
    scoring: &ScoringConfig,
    location_code: u32,
    language_code: &str,
    device: &str,
) -> Value {
    let mut positive = Vec::new();
    let mut negative = Vec::new();

    let mut keyword_value_score = 0.0;
    if inputs.search_volume >= scoring.min_search_volume as u64 {
        keyword_value_score += (inputs.search_volume as f64).ln_1p().min(20.0);
        positive.push("search_volume".to_string());
    } else {
        negative.push("low_search_volume".to_string());
    }
    if let Some(cpc) = inputs.cpc {
        if cpc >= 0.5 {
            keyword_value_score += (cpc * 5.0).min(15.0);
            positive.push("commercial_cpc".to_string());
        }
    }

    let mut difficulty_penalty = 0.0;
    if let Some(kd) = inputs.keyword_difficulty {
        if kd > scoring.max_keyword_difficulty {
            difficulty_penalty += ((kd - scoring.max_keyword_difficulty) as f64 * 1.2).min(40.0);
            negative.push("high_keyword_difficulty".to_string());
        } else {
            positive.push("acceptable_keyword_difficulty".to_string());
        }
    }

    let serp_weakness_score = inputs.weakness_score;
    if inputs.forum_count > 0 || inputs.ugc_count > 0 {
        positive.push("forum_or_ugc_in_serp".to_string());
    }
    if inputs.low_authority_count > 0 {
        positive.push("low_authority_domains_ranking".to_string());
    }

    if scoring.prefer_question_keywords && inputs.is_question {
        keyword_value_score += 5.0;
        positive.push("question_keyword".to_string());
    }

    if let Some(kgr) = inputs.kgr {
        if kgr <= 0.25 {
            keyword_value_score += 12.0;
            positive.push("golden_kgr".to_string());
        } else if kgr <= 1.0 {
            keyword_value_score += 6.0;
            positive.push("good_kgr".to_string());
        } else {
            difficulty_penalty += 8.0;
            negative.push("high_kgr".to_string());
        }
    }

    let content_fit_score = if inputs.has_matching_page {
        if inputs.own_rank.map(|r| r <= 20).unwrap_or(false) {
            35.0
        } else {
            20.0
        }
    } else {
        10.0
    };

    let backlink_gap_score = if inputs.low_authority_count >= 2 {
        15.0
    } else {
        5.0
    };

    let ai_citation_score = inputs.ai_citation_score;

    let raw = serp_weakness_score * 0.35
        + keyword_value_score * 0.25
        + content_fit_score * 0.15
        + backlink_gap_score * 0.10
        + ai_citation_score * 0.15
        - difficulty_penalty;

    let opportunity_score = raw.clamp(0.0, 100.0);
    let recommended_action = recommend_action(inputs, opportunity_score);

    json!({
        "site": site,
        "run_date": run_date,
        "keyword": inputs.keyword,
        "serp_weakness_score": serp_weakness_score,
        "keyword_value_score": keyword_value_score,
        "difficulty_penalty": difficulty_penalty,
        "content_fit_score": content_fit_score,
        "backlink_gap_score": backlink_gap_score,
        "ai_citation_score": ai_citation_score,
        "opportunity_score": opportunity_score,
        "top_positive_factors": positive,
        "top_negative_factors": negative,
        "recommended_action": recommended_action,
        "search_volume": inputs.search_volume,
        "keyword_difficulty": inputs.keyword_difficulty,
        "kgr": inputs.kgr,
        "allintitle_count": inputs.allintitle_count,
        "location_code": location_code,
        "language_code": language_code,
        "device": device,
    })
}

fn recommend_action(inputs: &OpportunityInputs, score: f64) -> &'static str {
    if score < 25.0 {
        return "monitor_only";
    }
    if inputs.ai_citation_score >= 40.0 && inputs.is_question {
        return "win_snippet";
    }
    if inputs.has_matching_page {
        return "refresh_existing_page";
    }
    if inputs.low_authority_count >= 2 && score >= 50.0 {
        return "create_new_page";
    }
    if score >= 40.0 {
        return "create_new_page";
    }
    "monitor_only"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_kgr_boosts_score() {
        let inputs = OpportunityInputs {
            keyword: "meal planning app free".into(),
            search_volume: 2400,
            keyword_difficulty: Some(28),
            cpc: Some(1.25),
            weakness_score: 45.0,
            forum_count: 1,
            ugc_count: 0,
            low_authority_count: 2,
            is_question: false,
            intent: Some("commercial".into()),
            kgr: Some(0.017),
            allintitle_count: Some(42),
            own_rank: None,
            has_matching_page: false,
            ai_citation_score: 20.0,
        };
        let row = compute_opportunity_score(
            "picnic.com",
            "2026-05-30",
            &inputs,
            &ScoringConfig::default(),
            2840,
            "en",
            "desktop",
        );
        assert!(row["opportunity_score"].as_f64().unwrap_or(0.0) > 0.0);
        let positives = row["top_positive_factors"].as_array().unwrap();
        assert!(positives.iter().any(|v| v.as_str() == Some("golden_kgr")));
        assert!(positives
            .iter()
            .any(|v| v.as_str() == Some("low_authority_domains_ranking")));
    }
}
