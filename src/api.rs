//! Engine session: owns `Config` and the current pipeline.
//!
//! CLI and Python both call this type. Process statics are not the product API.

use std::io;

use arrow::array::RecordBatch;
use arrow::compute::concat_batches;

use crate::doctor::{self, DoctorResult};
use crate::helpers::configuration::Config;
use crate::sqlrt::session::{collect_user_sql, coordinating_query_context};
use crate::sqlrt::tables::register_namespace_view;

/// `config` is the runnable (resolved) config. It is the session's only config authority.
#[derive(Clone, Debug)]
pub struct Session {
    pub config: Config,
    pub pipeline: Option<String>,
}

impl Session {
    pub fn from_config(config: Config, pipeline: Option<&str>) -> Self {
        Self {
            config,
            pipeline: pipeline.map(str::to_string),
        }
    }

    fn pipeline_names(&self) -> Vec<String> {
        match &self.pipeline {
            Some(name) => vec![name.clone()],
            None => self.config.pipelines.keys().cloned().collect(),
        }
    }

    fn bound(&self, pipeline: &str) -> Config {
        self.config.bind_pipeline(pipeline)
    }

    pub async fn doctor(&self) -> DoctorResult {
        doctor::run(&self.config, self.pipeline.as_deref())
    }

    pub async fn discover(&self, output_mode: &str) -> io::Result<()> {
        for name in self.pipeline_names() {
            let bound = self.bound(&name);
            bound.init().await;
            crate::engine::run_discover(&bound, output_mode).await?;
        }
        Ok(())
    }

    pub async fn migrate(&self, down: bool) -> io::Result<Vec<String>> {
        let direction = if down {
            crate::migration::MigrationDirection::Down
        } else {
            crate::migration::MigrationDirection::Up
        };
        let mut applied = Vec::new();
        for name in self.pipeline_names() {
            let bound = self.bound(&name);
            bound.init().await;
            let ids = crate::migration::apply_pending(&bound, direction)
                .await
                .map_err(io::Error::other)?;
            applied.extend(ids);
        }
        Ok(applied)
    }

    pub async fn sync(&self, once: bool, output_mode: &str) -> io::Result<()> {
        for name in self.pipeline_names() {
            let bound = self.bound(&name);
            bound.init().await;
            crate::engine::run_sync(&bound, output_mode, once).await?;
        }
        Ok(())
    }

    fn query_config(&self) -> Config {
        match &self.pipeline {
            Some(name) => self.bound(name),
            None => self.config.clone(),
        }
    }

    pub async fn query(&self, sql: &str) -> io::Result<Vec<RecordBatch>> {
        self.query_warm(sql, false).await
    }

    pub async fn query_warm(&self, sql: &str, warmup: bool) -> io::Result<Vec<RecordBatch>> {
        let bound = self.query_config();
        if !crate::sqlrt::query::sql_uses_record_batch_collect(sql) {
            return crate::sqlrt::query::query_collect_warm(&bound, sql, warmup).await;
        }
        crate::sqlrt::query::query_collect_warm(&bound, sql, warmup).await
    }

    pub async fn query_with_options(
        &self,
        sql: &str,
        options: crate::sqlrt::query::QueryExecutionOptions,
    ) {
        crate::sqlrt::query::query_with_options(&self.query_config(), sql, options).await
    }

    pub async fn df(&self, name: Option<&str>) -> io::Result<Vec<RecordBatch>> {
        let (pipeline, namespace) =
            resolve_df_name(self.pipeline.as_deref(), name).map_err(io::Error::other)?;
        let bound = self.bound(&pipeline);
        bound.init().await;
        if namespace == "*" {
            let namespaces = crate::sqlrt::registry::list_namespaces(&bound, &pipeline).await;
            let mut all: Vec<RecordBatch> = Vec::new();
            let mut schema = None;
            for ns in namespaces {
                let batches = collect_namespace(&bound, &pipeline, &ns).await?;
                for b in batches {
                    if schema.is_none() {
                        schema = Some(b.schema());
                    }
                    all.push(b);
                }
            }
            match schema {
                None => Ok(Vec::new()),
                Some(s) => concat_batches(&s, &all)
                    .map(|b| vec![b])
                    .map_err(|e| io::Error::other(e.to_string())),
            }
        } else {
            collect_namespace(&bound, &pipeline, &namespace).await
        }
    }
}

/// `None` → all namespaces (`*`) for the session pipeline.
/// `"orders"` → `{session_pipeline}.orders`.
/// `"bikehire.orders"` → FQN (same two-part split as `skipprd query`).
pub fn resolve_df_name(
    session_pipeline: Option<&str>,
    name: Option<&str>,
) -> Result<(String, String), String> {
    match name {
        None => {
            let pipeline = session_pipeline.ok_or_else(|| {
                "df() requires a pipeline when listing all namespaces".to_string()
            })?;
            Ok((pipeline.to_string(), "*".to_string()))
        }
        Some(raw) => {
            let parts: Vec<&str> = raw.split('.').collect();
            match parts.as_slice() {
                [p, n] => Ok(((*p).to_string(), (*n).to_string())),
                [n] => {
                    let pipeline = session_pipeline
                        .ok_or_else(|| "df(\"namespace\") requires Session.pipeline".to_string())?;
                    Ok((pipeline.to_string(), (*n).to_string()))
                }
                _ => Err(format!(
                    "df name must be namespace or pipeline.namespace, got {raw}"
                )),
            }
        }
    }
}

async fn collect_namespace(
    config: &Config,
    pipeline: &str,
    namespace: &str,
) -> io::Result<Vec<RecordBatch>> {
    let ctx = coordinating_query_context()
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    register_namespace_view(&ctx, config, pipeline, namespace)
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    let _ = config;
    let sql = format!("SELECT * FROM \"{}\".\"{}\"", pipeline, namespace);
    let df = ctx
        .sql(&sql)
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    collect_user_sql(df)
        .await
        .map_err(|e| io::Error::other(e.to_string()))
}

/// `PIPELINE_NAME` maps onto `Session.pipeline` when `--pipeline` is omitted. Not product identity.
pub fn cli_named_pipeline(flag: Option<&str>) -> Option<String> {
    flag.map(str::to_string).or_else(|| {
        let env_name = Config::getenv("PIPELINE_NAME", "");
        if env_name.is_empty() {
            None
        } else {
            Some(env_name)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{resolve_df_name, Session};
    use crate::helpers::configuration::Config;
    use serde_json::json;

    #[test]
    fn df_unqualified_uses_session_pipeline() {
        let (p, n) = resolve_df_name(Some("bikehire"), Some("orders")).unwrap();
        assert_eq!(p, "bikehire");
        assert_eq!(n, "orders");
    }

    #[test]
    fn df_fqn_splits_pipeline_namespace() {
        let (p, n) = resolve_df_name(Some("other"), Some("bikehire.orders")).unwrap();
        assert_eq!(p, "bikehire");
        assert_eq!(n, "orders");
    }

    #[test]
    fn df_all_requires_pipeline() {
        assert!(resolve_df_name(None, None).is_err());
        let (p, n) = resolve_df_name(Some("bikehire"), None).unwrap();
        assert_eq!(p, "bikehire");
        assert_eq!(n, "*");
    }

    #[test]
    fn two_sessions_do_not_share_pipeline() {
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "quickstart" },
            "pipelines": {
                "p1": { "data_source": "data_sources.sample" },
                "p2": { "data_source": "data_sources.sample" }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        let a = Session::from_config(config.clone(), Some("p1"));
        let b = Session::from_config(config, Some("p2"));
        assert_eq!(a.pipeline.as_deref(), Some("p1"));
        assert_eq!(b.pipeline.as_deref(), Some("p2"));
        assert_eq!(a.config.bind_pipeline("p1").get_pipeline_name(), "p1");
        assert_eq!(b.config.bind_pipeline("p2").get_pipeline_name(), "p2");
        assert_ne!(
            a.config.bind_pipeline("p1").get_pipeline_name(),
            b.config.bind_pipeline("p2").get_pipeline_name()
        );
    }

    #[test]
    fn session_pipeline_ignores_process_env() {
        std::env::set_var("PIPELINE_NAME", "envpipe");
        crate::helpers::configuration::Config::set_evncache("PIPELINE_NAME", "envpipe");
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "quickstart" },
            "pipelines": {
                "p1": { "data_source": "data_sources.sample" }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        let a = Session::from_config(config, Some("p1"));
        assert_eq!(a.pipeline.as_deref(), Some("p1"));
        assert_eq!(a.config.bind_pipeline("p1").get_pipeline_name(), "p1");
        std::env::remove_var("PIPELINE_NAME");
    }

    #[test]
    fn two_sessions_do_not_share_data_dir_via_env_cache() {
        crate::helpers::configuration::Config::reset_envcache();
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "quickstart" },
            "pipelines": {
                "p1": { "data_source": "data_sources.sample", "data_dir": "/tmp/skipprd-p1" },
                "p2": { "data_source": "data_sources.sample", "data_dir": "/tmp/skipprd-p2" }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        let a = Session::from_config(config.clone(), Some("p1"));
        let b = Session::from_config(config, Some("p2"));
        let a_dir = a.config.bind_pipeline("p1").get_pipeline_data_dir();
        let b_dir = b.config.bind_pipeline("p2").get_pipeline_data_dir();
        assert_eq!(a_dir, "/tmp/skipprd-p1");
        assert_eq!(b_dir, "/tmp/skipprd-p2");
    }

    #[test]
    fn two_sessions_do_not_share_input_plugin_via_env_cache() {
        crate::helpers::configuration::Config::reset_envcache();
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "quickstart" },
            "pipelines": {
                "p1": { "data_source": "data_sources.files" },
                "p2": { "data_source": "data_sources.bucket" }
            },
            "data_sources": {
                "files": { "File": { "path": "a.json" } },
                "bucket": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        let a = Session::from_config(config.clone(), Some("p1"));
        let b = Session::from_config(config, Some("p2"));
        assert_eq!(
            a.config
                .bind_pipeline("p1")
                .get_pipeline_input_plugin_name(),
            "File"
        );
        assert_eq!(
            b.config
                .bind_pipeline("p2")
                .get_pipeline_input_plugin_name(),
            "S3"
        );
    }

    #[test]
    fn session_has_no_clustered_sync_or_query() {
        let api = include_str!("api.rs");
        let session_impl = api.split("#[cfg(test)]").next().unwrap();
        assert!(
            !session_impl.contains("run_clustered("),
            "clustered WAL sync is deleted from Session"
        );
        assert!(
            !session_impl.contains("clustered_query_collect"),
            "clustered WAL query is deleted from Session"
        );
        let main = include_str!("main.rs");
        assert!(!main.contains("run_clustered"));
        assert!(!main.contains("validate_clustered"));
    }

    #[test]
    fn cli_query_dispatch_goes_through_session() {
        let main = include_str!("main.rs");
        let query_arm = main
            .split("Mode::Query(options)")
            .nth(1)
            .unwrap()
            .split("Mode::Schema(")
            .next()
            .unwrap();
        assert!(
            !query_arm.contains("&session.config"),
            "CLI Query must not pass session.config into sqlrt"
        );
        assert!(
            query_arm.contains("session.query_warm("),
            "CLI Query collect path must call Session.query_warm so --warmup can time a ready skipprd"
        );
        assert!(
            query_arm.contains("options.warmup"),
            "CLI Query must pass --warmup into the collect path"
        );
        assert!(
            query_arm.contains("sql_uses_record_batch_collect"),
            "CLI Query collect path must not swallow SHOW/DDL into RecordBatch JSON"
        );
        assert!(
            query_arm.contains("sql_uses_shared_extension_collect"),
            "CLI ALTER/ENABLE/DISABLE must share Session.query with Python"
        );
        assert!(
            query_arm.contains(".query_with_options("),
            "CLI Query watch/TUI/REPL must call Session.query_with_options"
        );
    }

    #[test]
    fn cli_discover_maps_pipeline_name_env() {
        let main = include_str!("main.rs");
        let discover_arm = main
            .split("Mode::Discover(options)")
            .nth(1)
            .unwrap()
            .split("Mode::Metadata")
            .next()
            .unwrap();
        assert!(
            discover_arm.contains("cli_named_pipeline"),
            "CLI discover must map PIPELINE_NAME onto Session.pipeline"
        );
    }

    #[test]
    fn cli_doctor_omitted_pipeline_checks_the_whole_file() {
        let cli = include_str!("cli/mod.rs");
        let doctor_opts = cli
            .split("pub struct DoctorOptions")
            .nth(1)
            .unwrap()
            .split("pub struct MigrateOptions")
            .next()
            .unwrap();
        assert!(
            doctor_opts.contains("pub pipeline: Option<String>"),
            "doctor takes optional --pipeline"
        );
        let main = include_str!("main.rs");
        let doctor_arm = main
            .split("Mode::Doctor(options)")
            .nth(1)
            .unwrap()
            .split("Mode::Df(options)")
            .next()
            .unwrap();
        assert!(
            doctor_arm.contains("options.pipeline.as_deref()"),
            "CLI doctor must pass --pipeline onto Session"
        );
        assert!(
            !doctor_arm.contains("cli_named_pipeline"),
            "omitted --pipeline must check the whole file, not PIPELINE_NAME"
        );
    }

    #[test]
    fn cli_docs_cover_doctor_and_df() {
        let doctor = include_str!("../docs/docs/cli/doctor.md");
        let df = include_str!("../docs/docs/cli/df.md");
        assert!(doctor.contains("skipprd doctor"));
        assert!(doctor.contains("--pipeline"));
        assert!(df.contains("skipprd df"));
        let overview = include_str!("../docs/docs/cli/overview.md");
        assert!(overview.contains("](/cli/doctor)"));
        assert!(overview.contains("](/cli/df)"));
        let connect = include_str!("../docs/docs/cli/connect.md");
        assert!(connect.contains("skipprd connect"));
        assert!(overview.contains("](/cli/connect)"));
    }
}
