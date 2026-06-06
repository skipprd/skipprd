use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tokio::time::sleep;
use tracing::info;

use crate::client::PageSpeedClient;
use crate::config::{DataSourceGooglePageSpeedPluginConfig, Strategy};
use crate::parse::{parse_pagespeed_response, site_run_daily_row};
use crate::sampling::{apply_request_budget, sample_urls, SamplingInput};
use crate::streams::{
    ALL_NAMESPACES, NAMESPACE_AUDIT_DAILY, NAMESPACE_CHECK_DAILY, NAMESPACE_FIELD_ORIGIN_DAILY,
    NAMESPACE_PAGE_DAILY, NAMESPACE_SITE_RUN_DAILY,
};

const DISCOVER_MAX_URLS: u32 = 1;

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|m| m.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

pub struct DataSourceGooglePageSpeedPlugin {
    config: DataSourceGooglePageSpeedPluginConfig,
    client: PageSpeedClient,
}

impl DataSourceGooglePageSpeedPlugin {
    pub fn new(config: DataSourceGooglePageSpeedPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let api_key = config.resolve_api_key()?;
        let client =
            PageSpeedClient::new(api_key, config.categories.clone(), config.locale.clone());
        Ok(Self { config, client })
    }

    fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
        let site = FieldPath::single("site");
        let run_date = FieldPath::single("run_date");
        let (primary_key, partition_key) = match namespace {
            NAMESPACE_SITE_RUN_DAILY | NAMESPACE_FIELD_ORIGIN_DAILY => {
                (vec![site.clone(), run_date.clone()], vec![run_date.clone()])
            }
            NAMESPACE_PAGE_DAILY => (
                vec![
                    site.clone(),
                    FieldPath::single("canonical_url"),
                    FieldPath::single("strategy"),
                    run_date.clone(),
                ],
                vec![run_date.clone()],
            ),
            NAMESPACE_AUDIT_DAILY => (
                vec![
                    site.clone(),
                    FieldPath::single("canonical_url"),
                    FieldPath::single("audit_id"),
                    FieldPath::single("strategy"),
                    run_date.clone(),
                ],
                vec![run_date.clone()],
            ),
            NAMESPACE_CHECK_DAILY => (
                vec![
                    site.clone(),
                    FieldPath::single("canonical_url"),
                    FieldPath::single("issue_code"),
                    FieldPath::single("strategy"),
                    run_date.clone(),
                ],
                vec![run_date.clone()],
            ),
            _ => (vec![site.clone(), run_date.clone()], vec![run_date.clone()]),
        };
        SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key,
            cursor: Some(run_date.clone()),
            partition_key,
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "PageSpeed Insights daily lab/field snapshot".into(),
            semantics: Some(SourceSemantics::MutableReport),
        }
    }

    fn submit_rows(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        run_date: &str,
        rows: Vec<serde_json::Value>,
    ) -> Result<(), std::io::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let payload = rows
            .into_iter()
            .map(|row| serde_json::to_string(&row))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join("\n");
        let bytes = payload.len();
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key: OffsetKey::new(namespace, run_date.to_string()),
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!("google-pagespeed://{}", self.config.site),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceGooglePageSpeedPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        ALL_NAMESPACES
            .iter()
            .map(|ns| Self::namespace_contract(ns))
            .collect()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;
        let discover = runtime_is_discover_mode();
        let run_date = Utc::now().format("%Y-%m-%d").to_string();
        let max_urls = if discover {
            DISCOVER_MAX_URLS
        } else {
            self.config.max_urls
        };
        let strategies = if discover {
            vec![Strategy::Mobile]
        } else {
            self.config.strategies.clone()
        };
        if discover {
            info!("Google PageSpeed discover: 1 URL, mobile only");
        }

        let fixture_dir = std::env::var("SKIPPR_GOOGLE_PAGESPEED_FIXTURE_DIR")
            .ok()
            .filter(|d| !d.trim().is_empty());
        let urls = sample_urls(SamplingInput {
            site: &self.config.site,
            url_mode: self.config.url_mode,
            url_list: &self.config.url_list,
            max_urls,
            respect_robots: self.config.respect_robots,
            fixture_dir: fixture_dir.as_deref(),
        })?;
        let jobs = apply_request_budget(urls, &strategies, self.config.max_requests_per_run);
        let delay = Duration::from_secs_f64(60.0 / self.config.requests_per_minute.max(1) as f64);

        let mut page_rows = Vec::new();
        let mut audit_rows = Vec::new();
        let mut issue_rows = Vec::new();
        let mut field_origin_row = None;
        let mut field_available = 0u32;
        let mut mobile_scores = Vec::new();

        for (url, strategy) in jobs {
            sleep(delay).await;
            let body = self.client.run_pagespeed(&url, &strategy).await?;
            let parsed = parse_pagespeed_response(
                &body,
                &self.config.site,
                &url,
                &run_date,
                &strategy,
                self.config.top_audits_per_page,
            );
            if parsed.page_rows[0]["field_data_available"].as_bool() == Some(true) {
                field_available += 1;
            }
            if strategy == Strategy::Mobile {
                if let Some(score) = parsed.page_rows[0]["lh_performance"].as_f64() {
                    mobile_scores.push(score);
                }
            }
            if field_origin_row.is_none() {
                field_origin_row = parsed.field_origin_row;
            }
            page_rows.extend(parsed.page_rows);
            audit_rows.extend(parsed.audit_rows);
            issue_rows.extend(parsed.issue_rows);
        }

        let urls_tested = page_rows.len() as u32;
        let field_pct = if urls_tested == 0 {
            0.0
        } else {
            (field_available as f64 / urls_tested as f64) * 100.0
        };
        let median_mobile = if mobile_scores.is_empty() {
            None
        } else {
            mobile_scores.sort_by(|a, b| a.partial_cmp(b).unwrap());
            Some(mobile_scores[mobile_scores.len() / 2])
        };

        self.submit_rows(ctx.as_ref(), NAMESPACE_PAGE_DAILY, &run_date, page_rows)?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_AUDIT_DAILY, &run_date, audit_rows)?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_CHECK_DAILY, &run_date, issue_rows)?;
        if let Some(row) = field_origin_row {
            self.submit_rows(
                ctx.as_ref(),
                NAMESPACE_FIELD_ORIGIN_DAILY,
                &run_date,
                vec![row],
            )?;
        }
        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_SITE_RUN_DAILY,
            &run_date,
            vec![site_run_daily_row(
                &self.config.site,
                &run_date,
                urls_tested,
                field_pct,
                median_mobile,
            )],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Strategy;
    use crate::sampling::UrlMode;
    use crate::streams::NAMESPACE_COUNT;

    #[test]
    fn namespace_contract_count_matches_catalog() {
        let plugin = DataSourceGooglePageSpeedPlugin::new(DataSourceGooglePageSpeedPluginConfig {
            site: "https://example.com".into(),
            api_key: Some("fixture".into()),
            url_mode: UrlMode::TldSample,
            url_list: vec![],
            max_urls: 1,
            strategies: vec![Strategy::Mobile],
            categories: vec!["performance".into()],
            locale: "en_US".into(),
            max_requests_per_run: 2,
            requests_per_minute: 60,
            respect_robots: true,
            top_audits_per_page: 5,
            max_concurrent_requests: 1,
        })
        .unwrap();
        assert_eq!(plugin.source_namespace_contracts().len(), NAMESPACE_COUNT);
    }
}
