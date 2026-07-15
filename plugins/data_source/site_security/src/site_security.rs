use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use skippr_plugin_data_source_site_quality::sampling::{
    homepage_url, normalize_site_origin, resolve_url_list,
    resolve_url_list_async, HttpSitemapFetcher, StaticSitemapFetcher, UrlMode,
};
use skippr_plugin_shared_api_source::merge_crawl_progress;
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::source_contract::SourceNamespaceContract;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::config::DataSourceSiteSecurityPluginConfig;
use crate::issue::{map_issues, origin_checks};
use crate::lighthouse::{checks_from_lighthouse, load_lighthouse_audits};
use crate::streams::{
    namespace_contract, ALL_NAMESPACES, NAMESPACE_CHECK_DAILY, NAMESPACE_COOKIE_ENTRY,
    NAMESPACE_PAGE_SCAN_DAILY, NAMESPACE_SITE_RUN_DAILY, NAMESPACE_STORAGE_ENTRY,
    NAMESPACE_THIRD_PARTY_SCRIPT, NAMESPACE_TLS_DAILY,
};
use crate::tls::{probe_origin, site_origin_for_probe, tls_row};
use crate::worker::{build_job_request, WorkerClient, WorkerJobResult};

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

fn data_dir() -> std::path::PathBuf {
    std::env::var("DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("./data"))
}

pub struct DataSourceSiteSecurityPlugin {
    config: DataSourceSiteSecurityPluginConfig,
    origin: String,
}

impl DataSourceSiteSecurityPlugin {
    pub fn new(config: DataSourceSiteSecurityPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let origin = normalize_site_origin(&config.site)?;
        Ok(Self { config, origin })
    }

    fn run_date(&self) -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    async fn resolve_urls(&self, discover: bool) -> Result<Vec<String>, std::io::Error> {
        if discover {
            return Ok(vec![homepage_url(&self.origin)]);
        }
        if std::env::var(crate::worker::FIXTURE_ENV)
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
            && self.config.url_mode == UrlMode::UrlList
        {
            let fetcher = StaticSitemapFetcher {
                robots_txt: None,
                sitemaps: HashMap::new(),
            };
            return resolve_url_list(
                &self.origin,
                self.config.url_mode,
                &self.config.url_list,
                self.config.max_pages_per_run,
                &fetcher,
                self.config.respect_robots,
            );
        }
        if self.config.url_mode == UrlMode::UrlList {
            let fetcher = StaticSitemapFetcher {
                robots_txt: None,
                sitemaps: HashMap::new(),
            };
            return resolve_url_list(
                &self.origin,
                self.config.url_mode,
                &self.config.url_list,
                self.config.max_pages_per_run,
                &fetcher,
                self.config.respect_robots,
            );
        }
        let fetcher = HttpSitemapFetcher::new()?;
        resolve_url_list_async(
            &self.origin,
            self.config.url_mode,
            &self.config.url_list,
            self.config.max_pages_per_run,
            &fetcher,
            self.config.respect_robots,
        )
        .await
    }

    fn page_scan_row(&self, run_date: &str, page_url: &str, result: &WorkerJobResult) -> Value {
        let headers = result.headers.as_ref();
        let dom = result.dom.as_ref();
        json!({
            "site": self.origin,
            "page_url": page_url,
            "run_date": run_date,
            "final_url": result.final_url,
            "status": result.status,
            "cookie_count": result.cookie_count,
            "local_storage_key_count": result.local_storage_key_count,
            "session_storage_key_count": result.session_storage_key_count,
            "third_party_script_count": result.third_party_script_count,
            "has_csp": headers.map(|h| h.has_csp),
            "has_hsts": headers.map(|h| h.has_hsts),
            "has_x_frame_options": headers.map(|h| h.has_x_frame_options),
            "has_x_content_type_options": headers.map(|h| h.has_x_content_type_options),
            "inline_script_count": dom.map(|d| d.inline_script_count),
            "scripts_without_sri": dom.map(|d| d.scripts_without_sri),
            "mixed_content_count": result.mixed_content_urls.len() as u32,
        })
    }

    fn storage_rows(&self, run_date: &str, page_url: &str, result: &WorkerJobResult) -> Vec<Value> {
        let mut rows = Vec::new();
        for entry in result
            .local_storage
            .iter()
            .chain(result.session_storage.iter())
        {
            rows.push(json!({
                "site": self.origin,
                "page_url": page_url,
                "run_date": run_date,
                "storage_kind": entry.storage_kind,
                "entry_name": entry.entry_name,
                "value_length": entry.value_length,
                "pii_hints": entry.pii_hints.join(","),
            }));
        }
        rows
    }

    fn cookie_rows(&self, run_date: &str, page_url: &str, result: &WorkerJobResult) -> Vec<Value> {
        result
            .jar_cookies
            .iter()
            .map(|c| {
                json!({
                    "site": self.origin,
                    "page_url": page_url,
                    "run_date": run_date,
                    "entry_name": c.entry_name,
                    "domain": c.domain,
                    "path": c.path,
                    "secure": c.secure,
                    "http_only": c.http_only,
                    "same_site": c.same_site,
                    "value_length": c.value_length,
                    "pii_hints": c.pii_hints.join(","),
                })
            })
            .collect()
    }

    fn script_rows(&self, run_date: &str, page_url: &str, result: &WorkerJobResult) -> Vec<Value> {
        result
            .scripts
            .iter()
            .map(|s| {
                json!({
                    "site": self.origin,
                    "page_url": page_url,
                    "run_date": run_date,
                    "script_url": s.script_url,
                    "script_host": s.script_host,
                    "is_third_party": s.is_third_party,
                    "async": s.async_attr,
                    "defer": s.defer,
                    "has_integrity": s.has_integrity,
                })
            })
            .collect()
    }

    fn submit_namespace(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        run_date: &str,
        rows: Vec<Value>,
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
        let offset_key = OffsetKey::new(namespace, run_date.to_string());
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key,
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!("site-security://{namespace}"),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceSiteSecurityPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        ALL_NAMESPACES
            .iter()
            .map(|ns| namespace_contract(ns))
            .inspect(|c| {
                c.validate()
                    .expect("invalid site_security namespace contract");
            })
            .collect()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;
        for contract in self.source_namespace_contracts() {
            contract
                .validate()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        let discover = runtime_is_discover_mode();
        let run_date = self.run_date();
        info!(
            site = %self.origin,
            discover,
            url_mode = ?self.config.url_mode,
            max_pages = self.config.max_pages_per_run,
            "Site Security sync: resolving URL sample"
        );
        let urls = self.resolve_urls(discover).await?;
        let devices = if discover {
            vec![self.config.devices[0].clone()]
        } else {
            self.config.devices.clone()
        };

        let lh_audits = if self.config.import_lighthouse_from_site_quality && !discover {
            load_lighthouse_audits(&data_dir(), &self.origin, &run_date)
        } else {
            Vec::new()
        };
        if !lh_audits.is_empty() {
            info!(
                count = lh_audits.len(),
                "Site Security: reusing site-quality Lighthouse audits"
            );
        }

        let probe_origin_url = site_origin_for_probe(&self.origin);
        let tls_probe = if discover {
            crate::tls::TlsProbeResult::default()
        } else {
            probe_origin(&probe_origin_url).await
        };

        let worker = WorkerClient::new(&self.config)?;

        let mut page_scan_rows = Vec::new();
        let mut storage_rows = Vec::new();
        let mut cookie_rows = Vec::new();
        let mut script_rows = Vec::new();
        let mut check_rows = Vec::new();
        let mut pages_ok = 0u32;
        let mut pages_failed = 0u32;

        if !discover {
            check_rows.extend(origin_checks(&self.origin, &run_date, &tls_probe));
        }

        for url in &urls {
            for device in &devices {
                let job = build_job_request(&self.config, url, device);
                let result = worker.run_job(&job).await?;
                worker.throttle_delay().await;

                if result.ok {
                    pages_ok += 1;
                } else {
                    pages_failed += 1;
                }

                page_scan_rows.push(self.page_scan_row(&run_date, url, &result));
                if result.ok {
                    storage_rows.extend(self.storage_rows(&run_date, url, &result));
                    cookie_rows.extend(self.cookie_rows(&run_date, url, &result));
                    script_rows.extend(self.script_rows(&run_date, url, &result));
                }
                check_rows.extend(map_issues(
                    &self.origin,
                    url,
                    &run_date,
                    &result,
                    &self.config,
                ));
                if !lh_audits.is_empty() {
                    check_rows.extend(checks_from_lighthouse(
                        &self.origin,
                        url,
                        &run_date,
                        &lh_audits,
                    ));
                }
            }
        }

        let site_run = merge_crawl_progress(json!({
            "site": self.origin,
            "run_date": run_date,
            "pages_scanned": pages_ok + pages_failed,
            "pages_ok": pages_ok,
            "pages_failed": pages_failed,
            "storage_entries": storage_rows.len(),
            "script_entries": script_rows.len(),
            "cookie_entries": cookie_rows.len(),
        }));

        self.submit_namespace(
            ctx.as_ref(),
            NAMESPACE_SITE_RUN_DAILY,
            &run_date,
            vec![site_run],
        )?;
        if !discover {
            self.submit_namespace(
                ctx.as_ref(),
                NAMESPACE_TLS_DAILY,
                &run_date,
                vec![tls_row(&self.origin, &run_date, &tls_probe)],
            )?;
        }
        self.submit_namespace(
            ctx.as_ref(),
            NAMESPACE_PAGE_SCAN_DAILY,
            &run_date,
            page_scan_rows,
        )?;
        self.submit_namespace(
            ctx.as_ref(),
            NAMESPACE_STORAGE_ENTRY,
            &run_date,
            storage_rows,
        )?;
        self.submit_namespace(ctx.as_ref(), NAMESPACE_COOKIE_ENTRY, &run_date, cookie_rows)?;
        self.submit_namespace(
            ctx.as_ref(),
            NAMESPACE_THIRD_PARTY_SCRIPT,
            &run_date,
            script_rows,
        )?;
        self.submit_namespace(ctx.as_ref(), NAMESPACE_CHECK_DAILY, &run_date, check_rows)?;

        info!(
            site = %self.origin,
            pages_ok,
            pages_failed,
            "Site Security sync complete"
        );
        Ok(())
    }
}
