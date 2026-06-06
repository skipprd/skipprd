pub const NAMESPACE_SITE_RUN_DAILY: &str = "dataforseo_seo_opportunities.site_run_daily";
pub const NAMESPACE_SEED_KEYWORD_DAILY: &str = "dataforseo_seo_opportunities.seed_keyword_daily";
pub const NAMESPACE_KEYWORD_SUGGESTION_DAILY: &str =
    "dataforseo_seo_opportunities.keyword_suggestion_daily";
pub const NAMESPACE_KEYWORD_METRIC_DAILY: &str =
    "dataforseo_seo_opportunities.keyword_metric_daily";
pub const NAMESPACE_SERP_RESULT_DAILY: &str = "dataforseo_seo_opportunities.serp_result_daily";
pub const NAMESPACE_SERP_FEATURE_DAILY: &str = "dataforseo_seo_opportunities.serp_feature_daily";
pub const NAMESPACE_WEAK_SPOT_DAILY: &str = "dataforseo_seo_opportunities.weak_spot_daily";
pub const NAMESPACE_KEYWORD_CLUSTER_DAILY: &str =
    "dataforseo_seo_opportunities.keyword_cluster_daily";
pub const NAMESPACE_COMPETITOR_KEYWORD_DAILY: &str =
    "dataforseo_seo_opportunities.competitor_keyword_daily";
pub const NAMESPACE_COMPETITOR_SITEMAP_URL_DAILY: &str =
    "dataforseo_seo_opportunities.competitor_sitemap_url_daily";
pub const NAMESPACE_ALLINTITLE_DAILY: &str = "dataforseo_seo_opportunities.allintitle_daily";
pub const NAMESPACE_RANK_TRACKING_DAILY: &str = "dataforseo_seo_opportunities.rank_tracking_daily";
pub const NAMESPACE_AI_CITATION_OPPORTUNITY_DAILY: &str =
    "dataforseo_seo_opportunities.ai_citation_opportunity_daily";
pub const NAMESPACE_CONTENT_BRIEF_DAILY: &str = "dataforseo_seo_opportunities.content_brief_daily";
pub const NAMESPACE_OPPORTUNITY_SCORE_DAILY: &str =
    "dataforseo_seo_opportunities.opportunity_score_daily";

pub const ALL_NAMESPACES: &[&str] = &[
    NAMESPACE_SITE_RUN_DAILY,
    NAMESPACE_SEED_KEYWORD_DAILY,
    NAMESPACE_KEYWORD_SUGGESTION_DAILY,
    NAMESPACE_KEYWORD_METRIC_DAILY,
    NAMESPACE_SERP_RESULT_DAILY,
    NAMESPACE_SERP_FEATURE_DAILY,
    NAMESPACE_WEAK_SPOT_DAILY,
    NAMESPACE_KEYWORD_CLUSTER_DAILY,
    NAMESPACE_COMPETITOR_KEYWORD_DAILY,
    NAMESPACE_COMPETITOR_SITEMAP_URL_DAILY,
    NAMESPACE_ALLINTITLE_DAILY,
    NAMESPACE_RANK_TRACKING_DAILY,
    NAMESPACE_AI_CITATION_OPPORTUNITY_DAILY,
    NAMESPACE_CONTENT_BRIEF_DAILY,
    NAMESPACE_OPPORTUNITY_SCORE_DAILY,
];

pub const NAMESPACE_COUNT: usize = ALL_NAMESPACES.len();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_count_matches() {
        assert_eq!(ALL_NAMESPACES.len(), NAMESPACE_COUNT);
    }
}
