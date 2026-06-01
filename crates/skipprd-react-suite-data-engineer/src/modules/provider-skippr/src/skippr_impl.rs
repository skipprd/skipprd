use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use react_core::scope::RequestScope;
use react_suite_data_engineer::de_config::{ElToolResolved, WarehouseKind, WarehouseResolved};
use react_suite_data_engineer::providers::{
    SkipprDiscoverResult, SkipprFieldSchema, SkipprNamespaceStatus, SkipprPipelineConfig,
    SkipprPipelineStatus, SkipprProvider, SkipprSyncResult,
};

#[derive(Clone)]
pub struct SkipprCliProvider {
    pub binary: String,
    pub data_dir: PathBuf,
    pub tenant: String,
    pub workspace: String,
    pub el_config: ElToolResolved,
    pub warehouse: WarehouseResolved,
    pub storage_mode: Option<String>,
    pub storage_bucket: Option<String>,
}

fn skippr_binary_looks_like_path(binary: &str) -> bool {
    let binary = binary.trim();
    Path::new(binary).is_absolute()
        || binary.contains('/')
        || binary.contains('\\')
        || binary.ends_with(".exe")
}

fn find_skippr_binary_on_path(name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        #[cfg(windows)]
        let candidates = [
            dir.join(format!("{}.exe", name.trim_end_matches(".exe"))),
            dir.join(name),
        ];
        #[cfg(not(windows))]
        let candidates = [dir.join(name)];
        for candidate in candidates {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve the skipprd executable to an absolute path when possible.
///
/// Windows `CreateProcess` searches the child `current_dir` before `PATH`; model runs
/// with `cwd` under `.skippr/{tenant}/{pipeline}`, so a bare `skipprd` name can fail
/// even when the binary is on `PATH`.
pub fn resolve_skippr_binary(configured: &str) -> String {
    let configured = configured.trim();
    let fallback = if configured.is_empty() {
        "skipprd"
    } else {
        configured
    };

    if skippr_binary_looks_like_path(fallback) {
        let path = PathBuf::from(fallback);
        if path.is_file() {
            return path.to_string_lossy().into_owned();
        }
    }

    for key in ["SKIPPRD_BINARY", "SKIPPR_BINARY"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            let path = PathBuf::from(value);
            if path.is_file() {
                return path.to_string_lossy().into_owned();
            }
        }
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            #[cfg(windows)]
            let candidates = [
                parent.join(format!("{}.exe", fallback.trim_end_matches(".exe"))),
                parent.join(fallback),
            ];
            #[cfg(not(windows))]
            let candidates = [parent.join(fallback)];
            for candidate in candidates {
                if candidate.is_file() {
                    return candidate.to_string_lossy().into_owned();
                }
            }

            let fallback_stem = PathBuf::from(fallback)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or(fallback);
            let current_stem = current_exe
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            if fallback_stem == "skipprd" && current_stem == "skippr" && current_exe.is_file() {
                return current_exe.to_string_lossy().into_owned();
            }
        }
    }

    if let Some(found) = find_skippr_binary_on_path(fallback) {
        return found.to_string_lossy().into_owned();
    }

    fallback.to_string()
}

impl SkipprCliProvider {
    pub fn new(
        el_config: ElToolResolved,
        warehouse: WarehouseResolved,
        data_dir: PathBuf,
        tenant: String,
        workspace: String,
        storage_mode: Option<String>,
        storage_bucket: Option<String>,
    ) -> Self {
        let binary = resolve_skippr_binary(&el_config.skippr_binary);
        if binary != el_config.skippr_binary {
            tracing::info!(
                configured = %el_config.skippr_binary,
                resolved = %binary,
                "resolved skippr EL binary path"
            );
        }
        Self {
            binary,
            data_dir,
            tenant,
            workspace,
            el_config,
            warehouse,
            storage_mode,
            storage_bucket,
        }
    }

    fn data_dir_path(&self) -> PathBuf {
        if self.data_dir.is_absolute() {
            return self.data_dir.clone();
        }

        std::env::current_dir()
            .map(|cwd| cwd.join(&self.data_dir))
            .unwrap_or_else(|_| self.data_dir.clone())
    }

    fn skippr_yml_path(&self) -> PathBuf {
        self.data_dir_path().join("skippr.yml")
    }

    fn env_vars(&self) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert(
            "DATA_DIR".into(),
            self.data_dir_path().to_string_lossy().to_string(),
        );
        let config_file = std::env::var("SKIPPR_CONFIG_FILE")
            .ok()
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .unwrap_or_else(|| self.skippr_yml_path());
        env.insert(
            "SKIPPR_CONFIG_FILE".into(),
            config_file.to_string_lossy().to_string(),
        );
        if !self.tenant.is_empty() {
            env.insert("TENANT".into(), self.tenant.clone());
        }
        if !self.workspace.is_empty() {
            env.insert("WORKSPACE_NAME".into(), self.workspace.clone());
        }
        if let Some(storage_mode) = self.storage_mode.as_deref().filter(|v| !v.is_empty()) {
            env.insert("SKIPPRD_EL_STORAGE_MODE".into(), storage_mode.to_string());
        }
        if let Some(bucket) = self.storage_bucket.as_deref().filter(|v| !v.is_empty()) {
            env.insert("SKIPPR_S3_BUCKET".into(), bucket.to_string());
        }

        let input = &self.el_config.skippr_input;
        if let Some(kind) = input.get("kind").and_then(|v| v.as_str()) {
            let plugin_name = skippr_plugin_name(kind);
            env.insert("DATA_SOURCE_PLUGIN_NAME".into(), plugin_name);

            if kind.eq_ignore_ascii_case("mssql") {
                if let Some(cs) = input.get("connection_string").and_then(|v| v.as_str()) {
                    let resolved = resolve_env_ref(cs);
                    env.insert("MSSQL_CONNECTION_STRING".into(), resolved);
                }
            }

            if kind.eq_ignore_ascii_case("s3") {
                if let Some(b) = input.get("s3_bucket").and_then(|v| v.as_str()) {
                    env.insert("DATA_SOURCE_S3_BUCKET".into(), resolve_env_ref(b));
                }
                if let Some(p) = input.get("s3_prefix").and_then(|v| v.as_str()) {
                    env.insert("DATA_SOURCE_S3_PREFIX".into(), resolve_env_ref(p));
                }
            }

            if kind.eq_ignore_ascii_case("mysql") {
                if let Some(cs) = input.get("connection_string").and_then(|v| v.as_str()) {
                    env.insert("MYSQL_CONNECTION_STRING".into(), resolve_env_ref(cs));
                }
            }

            if kind.eq_ignore_ascii_case("postgres") {
                if let Some(v) = input.get("host").and_then(|v| v.as_str()) {
                    env.insert("POSTGRES_HOST".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("user").and_then(|v| v.as_str()) {
                    env.insert("POSTGRES_USER".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("password").and_then(|v| v.as_str()) {
                    env.insert("POSTGRES_PASSWORD".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("database").and_then(|v| v.as_str()) {
                    env.insert("POSTGRES_DATABASE".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("connection_string").and_then(|v| v.as_str()) {
                    env.insert("POSTGRES_CONNECTION_STRING".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("redshift") {
                if let Some(v) = input.get("cluster_identifier").and_then(|v| v.as_str()) {
                    env.insert("REDSHIFT_CLUSTER_IDENTIFIER".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("database").and_then(|v| v.as_str()) {
                    env.insert("REDSHIFT_DATABASE".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("region").and_then(|v| v.as_str()) {
                    env.insert("AWS_DEFAULT_REGION".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("mongodb") {
                if let Some(v) = input.get("connection_string").and_then(|v| v.as_str()) {
                    env.insert("MONGODB_CONNECTION_STRING".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("dynamodb") {
                if let Some(v) = input.get("table_name").and_then(|v| v.as_str()) {
                    env.insert("DYNAMODB_TABLE_NAME".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("region").and_then(|v| v.as_str()) {
                    env.insert("AWS_DEFAULT_REGION".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("clickhouse") {
                if let Some(v) = input.get("url").and_then(|v| v.as_str()) {
                    env.insert("CLICKHOUSE_URL".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("database").and_then(|v| v.as_str()) {
                    env.insert("CLICKHOUSE_DATABASE".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("user").and_then(|v| v.as_str()) {
                    env.insert("CLICKHOUSE_USER".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("motherduck") {
                if let Some(v) = input.get("motherduck_token").and_then(|v| v.as_str()) {
                    env.insert("MOTHERDUCK_TOKEN".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("database").and_then(|v| v.as_str()) {
                    env.insert("MOTHERDUCK_DATABASE".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("sftp") {
                if let Some(v) = input.get("host").and_then(|v| v.as_str()) {
                    env.insert("SFTP_HOST".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("username").and_then(|v| v.as_str()) {
                    env.insert("SFTP_USERNAME".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("password").and_then(|v| v.as_str()) {
                    env.insert("SFTP_PASSWORD".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("remote_path").and_then(|v| v.as_str()) {
                    env.insert("SFTP_REMOTE_PATH".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("kafka") {
                if let Some(v) = input.get("brokers").and_then(|v| v.as_str()) {
                    env.insert("KAFKA_BROKERS".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("topic").and_then(|v| v.as_str()) {
                    env.insert("KAFKA_TOPIC".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("sqs") {
                if let Some(v) = input.get("queue_url").and_then(|v| v.as_str()) {
                    env.insert("SQS_QUEUE_URL".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("region").and_then(|v| v.as_str()) {
                    env.insert("AWS_DEFAULT_REGION".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("kinesis") {
                if let Some(v) = input.get("stream_name").and_then(|v| v.as_str()) {
                    env.insert("KINESIS_STREAM_NAME".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("region").and_then(|v| v.as_str()) {
                    env.insert("AWS_DEFAULT_REGION".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("amqp") {
                if let Some(v) = input.get("connection_string").and_then(|v| v.as_str()) {
                    env.insert("AMQP_CONNECTION_STRING".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("queue").and_then(|v| v.as_str()) {
                    env.insert("AMQP_QUEUE".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("sns") {
                if let Some(v) = input.get("topic_arn").and_then(|v| v.as_str()) {
                    env.insert("SNS_TOPIC_ARN".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("sqs_queue_url").and_then(|v| v.as_str()) {
                    env.insert("SNS_SQS_QUEUE_URL".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("region").and_then(|v| v.as_str()) {
                    env.insert("AWS_DEFAULT_REGION".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("eventbridge") {
                if let Some(v) = input.get("event_bus_name").and_then(|v| v.as_str()) {
                    env.insert("EVENTBRIDGE_EVENT_BUS_NAME".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("sqs_queue_url").and_then(|v| v.as_str()) {
                    env.insert("EVENTBRIDGE_SQS_QUEUE_URL".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("region").and_then(|v| v.as_str()) {
                    env.insert("AWS_DEFAULT_REGION".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("mqtt") {
                if let Some(v) = input.get("broker_url").and_then(|v| v.as_str()) {
                    env.insert("MQTT_BROKER_URL".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("topic").and_then(|v| v.as_str()) {
                    env.insert("MQTT_TOPIC".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("websocket") {
                if let Some(v) = input.get("url").and_then(|v| v.as_str()) {
                    env.insert("WEBSOCKET_URL".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("socket") {
                if let Some(v) = input.get("mode").and_then(|v| v.as_str()) {
                    env.insert("SOCKET_MODE".into(), resolve_env_ref(v));
                }
                if let Some(v) = input.get("address").and_then(|v| v.as_str()) {
                    env.insert("SOCKET_ADDRESS".into(), resolve_env_ref(v));
                }
            }

            if kind.eq_ignore_ascii_case("delta_lake") {
                if let Some(v) = input.get("table_uri").and_then(|v| v.as_str()) {
                    env.insert("DELTA_TABLE_URI".into(), resolve_env_ref(v));
                }
            }
        }

        match self.warehouse.kind {
            WarehouseKind::Snowflake => {
                env.insert("DATA_OUTPUT_PLUGIN_NAME".into(), "Snowflake".into());
                insert_snowflake_env(&self.warehouse, &mut env);
            }
            WarehouseKind::Athena => {
                env.insert("DATA_OUTPUT_PLUGIN_NAME".into(), "Athena".into());
                if let Some(v) = self
                    .warehouse
                    .extras
                    .get("region")
                    .and_then(|v| v.as_str())
                    .map(resolve_env_ref)
                {
                    env.insert("AWS_DEFAULT_REGION".into(), v);
                }
                if let Some(v) = self
                    .warehouse
                    .extras
                    .get("workgroup")
                    .and_then(|v| v.as_str())
                    .map(resolve_env_ref)
                {
                    env.insert("ATHENA_WORKGROUP".into(), v);
                }
                if let Some(v) = self
                    .warehouse
                    .extras
                    .get("result_s3")
                    .and_then(|v| v.as_str())
                    .map(resolve_env_ref)
                {
                    env.insert("ATHENA_RESULT_S3".into(), v);
                }
            }
            WarehouseKind::Bigquery => {
                env.insert("DATA_OUTPUT_PLUGIN_NAME".into(), "Bigquery".into());
                if let Some(v) = std::env::var("BIGQUERY_PROJECT")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                {
                    env.insert("BIGQUERY_PROJECT".into(), v);
                } else if !self.warehouse.container.is_empty() {
                    env.insert(
                        "BIGQUERY_PROJECT".into(),
                        resolve_env_ref(&self.warehouse.container),
                    );
                }
                if let Some(v) = std::env::var("BIGQUERY_DATASET")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                {
                    env.insert("BIGQUERY_DATASET".into(), v);
                } else if !self.warehouse.namespace.is_empty() {
                    env.insert(
                        "BIGQUERY_DATASET".into(),
                        resolve_env_ref(&self.warehouse.namespace),
                    );
                }
                if let Some(loc) = self
                    .warehouse
                    .extras
                    .get("location")
                    .and_then(|v| v.as_str())
                {
                    env.insert("BIGQUERY_LOCATION".into(), loc.to_string());
                }
            }
            WarehouseKind::Postgres => {
                env.insert("DATA_OUTPUT_PLUGIN_NAME".into(), "Postgres".into());
                if let Some(v) = std::env::var("POSTGRES_HOST")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                {
                    env.insert("POSTGRES_HOST".into(), v);
                }
                if let Some(v) = std::env::var("POSTGRES_PORT")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                {
                    env.insert("POSTGRES_PORT".into(), v);
                }
                if let Some(v) = std::env::var("POSTGRES_USER")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                {
                    env.insert("POSTGRES_USER".into(), v);
                }
                if let Some(v) = std::env::var("POSTGRES_PASSWORD")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                {
                    env.insert("POSTGRES_PASSWORD".into(), v);
                }
                if let Some(v) = std::env::var("POSTGRES_DATABASE")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                {
                    env.insert("POSTGRES_DATABASE".into(), v);
                } else if !self.warehouse.container.is_empty() {
                    env.insert(
                        "POSTGRES_DATABASE".into(),
                        resolve_env_ref(&self.warehouse.container),
                    );
                }
                if let Some(v) = std::env::var("POSTGRES_SCHEMA")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                {
                    env.insert("POSTGRES_SCHEMA".into(), v);
                } else if !self.warehouse.namespace.is_empty() {
                    env.insert(
                        "POSTGRES_SCHEMA".into(),
                        resolve_env_ref(&self.warehouse.namespace),
                    );
                }
                if let Some(v) = std::env::var("POSTGRES_SSLMODE")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                {
                    env.insert("POSTGRES_SSLMODE".into(), v);
                }
            }
            WarehouseKind::Databricks => {
                env.insert("DATA_OUTPUT_PLUGIN_NAME".into(), "Databricks".into());
            }
            WarehouseKind::Synapse => {
                env.insert("DATA_OUTPUT_PLUGIN_NAME".into(), "Synapse".into());
            }
            WarehouseKind::Redshift => {
                env.insert("DATA_OUTPUT_PLUGIN_NAME".into(), "Redshift".into());
            }
            WarehouseKind::Clickhouse => {
                env.insert("DATA_OUTPUT_PLUGIN_NAME".into(), "Clickhouse".into());
            }
            WarehouseKind::Motherduck => {
                env.insert("DATA_OUTPUT_PLUGIN_NAME".into(), "Motherduck".into());
            }
            WarehouseKind::Mssql => {}
        }

        if let Some(ref cdc) = self.el_config.cdc {
            if cdc.enabled {
                env.insert("CDC_ENABLED".into(), "true".into());
                if !cdc.business_key_columns.is_empty() {
                    env.insert(
                        "CDC_BUSINESS_KEY_COLUMNS".into(),
                        cdc.business_key_columns.join(","),
                    );
                }
            }
        }

        if let Some(ref ss) = self.el_config.schema_sink {
            if let Some(kind) = ss.get("kind").and_then(|v| v.as_str()) {
                env.insert("DATA_SCHEMA_PLUGIN_NAME".into(), skippr_plugin_name(kind));
                if kind.eq_ignore_ascii_case("glue") {
                    if let Some(v) = ss.get("glue_database_name").and_then(|v| v.as_str()) {
                        env.insert("GLUE_DATABASE_NAME".into(), resolve_env_ref(v));
                    }
                }
            }
        }

        env
    }

    async fn run_skippr(&self, args: &[&str]) -> Result<std::process::Output, String> {
        self.run_skippr_with_timeout(args, std::time::Duration::from_secs(120))
            .await
    }

    async fn run_skippr_with_timeout(
        &self,
        args: &[&str],
        timeout: std::time::Duration,
    ) -> Result<std::process::Output, String> {
        let env = self.env_vars();
        let data_dir = self.data_dir_path();
        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.arg("--log");
        cmd.arg("info");
        cmd.args(args);
        cmd.current_dir(&data_dir);
        cmd.env("RUST_BACKTRACE", "1");
        for (k, v) in &env {
            cmd.env(k, v);
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        tracing::info!(
            binary = %self.binary,
            args = ?args,
            data_dir = %data_dir.display(),
            timeout_secs = timeout.as_secs(),
            "spawning skippr"
        );

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn skippr: {}", e))?;

        let child_id = child.id();

        let stdout_handle = child.stdout.take();
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut stdout) = stdout_handle {
                use tokio::io::AsyncReadExt;
                let _ = stdout.read_to_end(&mut buf).await;
            }
            buf
        });

        let stderr_handle = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut buf = Vec::new();
            if let Some(stderr) = stderr_handle {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        tracing::info!(target: "skipprd", "{}", line);
                    }
                    buf.extend_from_slice(line.as_bytes());
                    buf.push(b'\n');
                }
            }
            buf
        });

        let result = tokio::time::timeout(timeout, child.wait()).await;

        match result {
            Ok(Ok(status)) => {
                let stdout_bytes = stdout_task.await.unwrap_or_default();
                let stderr_bytes = stderr_task.await.unwrap_or_default();
                let output = std::process::Output {
                    status: std::process::ExitStatus::from(status),
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                };
                if !output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    tracing::warn!(
                        exit_code = ?output.status.code(),
                        stdout = %stdout,
                        "skippr command failed"
                    );
                }
                Ok(output)
            }
            Ok(Err(e)) => Err(format!("skippr process error: {}", e)),
            Err(_) => {
                if let Some(pid) = child_id {
                    let _ = tokio::process::Command::new("kill")
                        .args(["-9", &pid.to_string()])
                        .status()
                        .await;
                }
                Err(format!(
                    "skippr command timed out after {}s (args: {:?})",
                    timeout.as_secs(),
                    args
                ))
            }
        }
    }

    /// Runs skipprd with streaming stdout. This is the external CLI subprocess
    /// contract, not the runtime plugin IPC contract in `src/runtime_plugins`.
    /// The adapter keeps only a typed sync summary instead of retaining raw events.
    async fn run_skippr_streaming(
        &self,
        args: &[&str],
        idle_timeout: std::time::Duration,
    ) -> Result<(SyncSummaryAccumulator, Vec<u8>, bool), String> {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let env = self.env_vars();
        let data_dir = self.data_dir_path();
        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.arg("--log");
        cmd.arg("info");
        cmd.args(args);
        cmd.current_dir(&data_dir);
        cmd.env("RUST_BACKTRACE", "1");
        for (k, v) in &env {
            cmd.env(k, v);
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        tracing::info!(
            binary = %self.binary,
            args = ?args,
            data_dir = %data_dir.display(),
            idle_timeout_secs = idle_timeout.as_secs(),
            "spawning skippr (streaming)"
        );

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn skippr: {}", e))?;

        let child_id = child.id();

        let stderr_handle = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(stderr) = stderr_handle {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        tracing::info!(target: "skipprd", "{}", line);
                    }
                    buf.extend_from_slice(line.as_bytes());
                    buf.push(b'\n');
                }
            }
            buf
        });

        let stdout_handle = child
            .stdout
            .take()
            .ok_or_else(|| "skippr stdout not available".to_string())?;
        let mut reader = BufReader::new(stdout_handle).lines();

        let mut summary = SyncSummaryAccumulator::default();
        let mut timed_out = false;

        loop {
            match tokio::time::timeout(idle_timeout, reader.next_line()).await {
                Ok(Ok(Some(line))) => {
                    if let Some(val) = parse_json_line(&line) {
                        if let Some(kind) = val.get("event").and_then(|v| v.as_str()) {
                            match kind {
                                "sync_status" => {
                                    let msgs =
                                        val.get("total_rows").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let bytes =
                                        val.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let rows_written = val
                                        .get("rows_written")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let elapsed =
                                        val.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let uploads = val
                                        .get("uploads_in_flight")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    tracing::info!(
                                        messages = msgs,
                                        source_bytes = bytes,
                                        rows_written = rows_written,
                                        uploads_in_flight = uploads,
                                        elapsed_ms = elapsed,
                                        "skippr sync heartbeat"
                                    );
                                }
                                _ => {
                                    tracing::debug!(event = kind, "skippr sync event");
                                }
                            }
                        }
                        summary.record(&val);
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "error reading skippr stdout");
                    break;
                }
                Err(_) => {
                    tracing::error!(
                        idle_timeout_secs = idle_timeout.as_secs(),
                        "skippr sync idle timeout — no output received"
                    );
                    timed_out = true;
                    if let Some(pid) = child_id {
                        let _ = tokio::process::Command::new("kill")
                            .args(["-9", &pid.to_string()])
                            .status()
                            .await;
                    }
                    break;
                }
            }
        }

        let exit_status = child.wait().await;
        let stderr_bytes = stderr_task.await.unwrap_or_default();

        if timed_out {
            return Err(format!(
                "skippr sync stalled — no output for {}s (args: {:?})",
                idle_timeout.as_secs(),
                args,
            ));
        }

        let success = exit_status.as_ref().map(|s| s.success()).unwrap_or(false);
        if !success {
            tracing::warn!(
                exit_code = ?exit_status.as_ref().ok().and_then(|s| s.code()),
                "skippr command failed"
            );
        }

        Ok((summary, stderr_bytes, success))
    }

    fn generate_skippr_yml(&self, config: &SkipprPipelineConfig) -> String {
        let input_block = &config.skippr_input;
        let input_kind = input_block
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown");
        let capitalized_kind = skippr_plugin_name(input_kind);

        let mut input_config = input_block.clone();
        let transform_block = if let Some(obj) = input_config.as_object_mut() {
            obj.remove("kind");
            obj.remove("transform")
        } else {
            None
        };
        resolve_env_refs_in_value(&mut input_config);

        let output_kind = skippr_plugin_name(&config.output_plugin.kind);
        let output_config = self.build_output_config(Some(config));

        let mut pipeline_block = serde_json::json!({
            "data_source": "data_sources.source",
            "data_sink": "data_sinks.destination"
        });

        if let Some(t) = transform_block {
            if t.is_object() {
                pipeline_block["transform"] = t;
            }
        }

        if let Some(ref cdc) = self.el_config.cdc {
            if cdc.enabled {
                let mut cdc_block = serde_json::Map::new();
                if !cdc.business_key_columns.is_empty() {
                    cdc_block.insert(
                        "business_key_columns".into(),
                        serde_json::Value::Array(
                            cdc.business_key_columns
                                .iter()
                                .map(|c| serde_json::Value::String(c.clone()))
                                .collect(),
                        ),
                    );
                }
                pipeline_block["cdc"] = serde_json::Value::Object(cdc_block);
            }
        }

        let mut dest_block = serde_json::json!({ output_kind: output_config });

        let has_schema_sink = config.schema_sink.is_some() || self.el_config.schema_sink.is_some();

        if has_schema_sink {
            dest_block["schema_sink"] = serde_json::Value::String("schema_sinks.schema".into());
        }

        let mut skippr_block = serde_json::Map::new();
        if !self.workspace.is_empty() {
            skippr_block.insert(
                "workspace".to_string(),
                serde_json::Value::String(self.workspace.clone()),
            );
        }
        if let Some(storage_mode) = self.storage_mode.as_deref().filter(|v| !v.is_empty()) {
            skippr_block.insert(
                "skipprd_el_storage_mode".to_string(),
                serde_json::Value::String(storage_mode.to_string()),
            );
        }
        if let Some(bucket) = self.storage_bucket.as_deref().filter(|v| !v.is_empty()) {
            skippr_block.insert(
                "skippr_s3_bucket".to_string(),
                serde_json::Value::String(bucket.to_string()),
            );
        }

        let mut root = serde_json::json!({
            "skippr": serde_json::Value::Object(skippr_block),
            "pipelines": {
                &config.pipeline_name: pipeline_block
            },
            "data_sources": {
                "source": {
                    capitalized_kind: input_config
                }
            },
            "data_sinks": {
                "destination": dest_block
            }
        });

        if has_schema_sink {
            let ss = config
                .schema_sink
                .as_ref()
                .map(|s| {
                    let mut m = serde_json::Map::new();
                    let kind = skippr_plugin_name(&s.kind);
                    let mut inner = serde_json::Map::new();
                    if let Some(ref db) = s.glue_database_name {
                        inner.insert(
                            "glue_database_name".into(),
                            serde_json::Value::String(db.clone()),
                        );
                    }
                    m.insert(kind, serde_json::Value::Object(inner));
                    serde_json::Value::Object(m)
                })
                .or_else(|| {
                    self.el_config.schema_sink.as_ref().map(|ss_val| {
                        let kind = ss_val
                            .get("kind")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let cap = skippr_plugin_name(kind);
                        let mut inner = serde_json::Map::new();
                        if let Some(v) = ss_val.get("glue_database_name").and_then(|v| v.as_str()) {
                            inner.insert(
                                "glue_database_name".into(),
                                serde_json::Value::String(v.to_string()),
                            );
                        }
                        let mut m = serde_json::Map::new();
                        m.insert(cap, serde_json::Value::Object(inner));
                        serde_json::Value::Object(m)
                    })
                });

            if let Some(ss_block) = ss {
                root["schema_sinks"] = serde_json::json!({ "schema": ss_block });
            }
        }

        let yml = serde_yaml::to_value(&root).unwrap_or_default();
        serde_yaml::to_string(&yml).unwrap_or_default()
    }

    fn build_output_config(
        &self,
        pipeline_config: Option<&SkipprPipelineConfig>,
    ) -> serde_json::Value {
        fn getenv(key: &str) -> Option<String> {
            std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        }
        fn extra_str(extras: &serde_json::Value, key: &str) -> Option<String> {
            extras
                .get(key)
                .and_then(|v| v.as_str())
                .map(resolve_env_ref)
        }
        fn parse_s3_uri(uri: &str) -> Option<(String, String)> {
            let trimmed = uri.trim();
            let path = trimmed.strip_prefix("s3://")?;
            let (bucket, prefix) = match path.split_once('/') {
                Some((bucket, prefix)) => (bucket.trim(), prefix.trim_matches('/')),
                None => (path.trim(), ""),
            };
            if bucket.is_empty() {
                return None;
            }
            Some((bucket.to_string(), prefix.to_string()))
        }

        match self.warehouse.kind {
            WarehouseKind::Athena => {
                let extras = &self.warehouse.extras;
                let mut cfg = serde_json::Map::new();
                let workgroup = getenv("ATHENA_WORKGROUP")
                    .or_else(|| extra_str(extras, "workgroup"))
                    .unwrap_or_else(|| "primary".to_string());
                let result_s3 =
                    getenv("ATHENA_RESULT_S3").or_else(|| extra_str(extras, "result_s3"));
                let (bucket, prefix) = result_s3
                    .as_deref()
                    .and_then(parse_s3_uri)
                    .unwrap_or_else(|| (String::new(), String::new()));

                cfg.insert(
                    "athena_workgroup_name".into(),
                    serde_json::Value::String(workgroup),
                );
                cfg.insert(
                    "s3_bucket".into(),
                    serde_json::Value::String(bucket.clone()),
                );
                cfg.insert("s3_prefix".into(), serde_json::Value::String(prefix));
                cfg.insert(
                    "athena_results_s3_bucket".into(),
                    serde_json::Value::String(bucket),
                );

                let glue_database_name = pipeline_config
                    .and_then(|cfg| cfg.schema_sink.as_ref())
                    .filter(|cfg| cfg.kind.eq_ignore_ascii_case("glue"))
                    .and_then(|cfg| cfg.glue_database_name.clone())
                    .or_else(|| {
                        self.el_config
                            .schema_sink
                            .as_ref()
                            .filter(|cfg| {
                                cfg.get("kind")
                                    .and_then(|v| v.as_str())
                                    .is_some_and(|kind| kind.eq_ignore_ascii_case("glue"))
                            })
                            .and_then(|cfg| cfg.get("glue_database_name"))
                            .and_then(|v| v.as_str())
                            .map(resolve_env_ref)
                    })
                    .or_else(|| {
                        Some(resolve_env_ref(&self.warehouse.namespace))
                            .filter(|value| !value.trim().is_empty())
                    });
                if let Some(v) = glue_database_name {
                    cfg.insert("glue_database_name".into(), serde_json::Value::String(v));
                }

                serde_json::Value::Object(cfg)
            }
            WarehouseKind::Snowflake => {
                let extras = &self.warehouse.extras;
                let mut cfg = serde_json::Map::new();
                if let Some(v) =
                    getenv("SNOWFLAKE_ACCOUNT").or_else(|| extra_str(extras, "account"))
                {
                    cfg.insert("account".into(), serde_json::Value::String(v));
                }
                if let Some(v) = getenv("SNOWFLAKE_USER").or_else(|| extra_str(extras, "user")) {
                    cfg.insert("user".into(), serde_json::Value::String(v));
                }
                if let Some(v) =
                    getenv("SNOWFLAKE_PASSWORD").or_else(|| extra_str(extras, "password"))
                {
                    cfg.insert("password".into(), serde_json::Value::String(v));
                }
                if let Some(v) =
                    getenv("SNOWFLAKE_WAREHOUSE").or_else(|| extra_str(extras, "warehouse"))
                {
                    cfg.insert("warehouse".into(), serde_json::Value::String(v));
                }
                if let Some(v) = getenv("SNOWFLAKE_ROLE").or_else(|| extra_str(extras, "role")) {
                    cfg.insert("role".into(), serde_json::Value::String(v));
                }
                if let Some(v) = getenv("SNOWFLAKE_DATABASE") {
                    cfg.insert("database".into(), serde_json::Value::String(v));
                } else if !self.warehouse.container.is_empty() {
                    cfg.insert(
                        "database".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.container)),
                    );
                }
                if let Some(v) = getenv("SNOWFLAKE_SCHEMA") {
                    cfg.insert("schema".into(), serde_json::Value::String(v));
                } else if !self.warehouse.namespace.is_empty() {
                    cfg.insert(
                        "schema".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.namespace)),
                    );
                }
                if let Some(v) = getenv("SNOWFLAKE_PRIVATE_KEY_PATH")
                    .or_else(|| extra_str(extras, "private_key_path"))
                {
                    cfg.insert(
                        "private_key_path".into(),
                        serde_json::Value::String(resolve_to_absolute(&v)),
                    );
                }
                if let Some(v) = getenv("SNOWFLAKE_STAGE").or_else(|| extra_str(extras, "stage")) {
                    cfg.insert("stage".into(), serde_json::Value::String(v));
                }
                if let Some(v) =
                    getenv("SNOWFLAKE_STAGING_URI").or_else(|| extra_str(extras, "staging_uri"))
                {
                    cfg.insert("staging_uri".into(), serde_json::Value::String(v));
                }
                if let Some(v) = getenv("SNOWFLAKE_STAGING_STORAGE_INTEGRATION")
                    .or_else(|| extra_str(extras, "staging_storage_integration"))
                {
                    cfg.insert(
                        "staging_storage_integration".into(),
                        serde_json::Value::String(v),
                    );
                }
                if let Some(v) = getenv("AZURE_STORAGE_SAS_TOKEN")
                    .or_else(|| getenv("SNOWFLAKE_STAGING_AZURE_SAS_TOKEN"))
                    .or_else(|| extra_str(extras, "staging_azure_sas_token"))
                {
                    cfg.insert(
                        "staging_azure_sas_token".into(),
                        serde_json::Value::String(v),
                    );
                }
                if let Some(v) = getenv("AZURE_STORAGE_ACCOUNT_KEY")
                    .or_else(|| getenv("SNOWFLAKE_STAGING_AZURE_ACCOUNT_KEY"))
                    .or_else(|| extra_str(extras, "staging_azure_account_key"))
                {
                    cfg.insert(
                        "staging_azure_account_key".into(),
                        serde_json::Value::String(v),
                    );
                }
                if let Some(v) = getenv("GOOGLE_APPLICATION_CREDENTIALS")
                    .or_else(|| getenv("SNOWFLAKE_STAGING_GCS_SERVICE_ACCOUNT_KEY_PATH"))
                    .or_else(|| extra_str(extras, "staging_gcs_service_account_key_path"))
                {
                    cfg.insert(
                        "staging_gcs_service_account_key_path".into(),
                        serde_json::Value::String(resolve_to_absolute(&v)),
                    );
                }
                serde_json::Value::Object(cfg)
            }
            WarehouseKind::Bigquery => {
                let extras = &self.warehouse.extras;
                let mut cfg = serde_json::Map::new();
                if let Some(v) = getenv("BIGQUERY_PROJECT") {
                    cfg.insert("project".into(), serde_json::Value::String(v));
                } else if !self.warehouse.container.is_empty() {
                    cfg.insert(
                        "project".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.container)),
                    );
                }
                if let Some(v) = getenv("BIGQUERY_DATASET") {
                    cfg.insert("dataset".into(), serde_json::Value::String(v));
                } else if !self.warehouse.namespace.is_empty() {
                    cfg.insert(
                        "dataset".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.namespace)),
                    );
                }
                if let Some(v) =
                    getenv("BIGQUERY_LOCATION").or_else(|| extra_str(extras, "location"))
                {
                    cfg.insert("location".into(), serde_json::Value::String(v));
                }
                if let Some(v) = getenv("GOOGLE_APPLICATION_CREDENTIALS") {
                    cfg.insert(
                        "credentials_path".into(),
                        serde_json::Value::String(resolve_to_absolute(&v)),
                    );
                }
                serde_json::Value::Object(cfg)
            }
            WarehouseKind::Postgres => {
                let mut cfg = serde_json::Map::new();
                if let Some(v) = getenv("POSTGRES_HOST") {
                    cfg.insert("host".into(), serde_json::Value::String(v));
                }
                if let Some(v) = getenv("POSTGRES_PORT") {
                    if let Ok(port) = v.parse::<u16>() {
                        cfg.insert("port".into(), serde_json::Value::Number(port.into()));
                    }
                }
                if let Some(v) = getenv("POSTGRES_USER") {
                    cfg.insert("user".into(), serde_json::Value::String(v));
                }
                if let Some(v) = getenv("POSTGRES_PASSWORD") {
                    cfg.insert("password".into(), serde_json::Value::String(v));
                }
                if let Some(v) = getenv("POSTGRES_DATABASE") {
                    cfg.insert("database".into(), serde_json::Value::String(v));
                } else if !self.warehouse.container.is_empty() {
                    cfg.insert(
                        "database".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.container)),
                    );
                }
                if let Some(v) = getenv("POSTGRES_SCHEMA") {
                    cfg.insert("schema".into(), serde_json::Value::String(v));
                } else if !self.warehouse.namespace.is_empty() {
                    cfg.insert(
                        "schema".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.namespace)),
                    );
                }
                if let Some(v) = getenv("POSTGRES_SSLMODE") {
                    cfg.insert("sslmode".into(), serde_json::Value::String(v));
                }
                serde_json::Value::Object(cfg)
            }
            WarehouseKind::Databricks => {
                let extras = &self.warehouse.extras;
                let mut cfg = serde_json::Map::new();
                if let Some(v) = extra_str(extras, "workspace_url") {
                    cfg.insert("workspace_url".into(), serde_json::Value::String(v));
                }
                if let Some(v) = extra_str(extras, "token") {
                    cfg.insert("token".into(), serde_json::Value::String(v));
                }
                if let Some(v) = extra_str(extras, "warehouse_id") {
                    cfg.insert("warehouse_id".into(), serde_json::Value::String(v));
                }
                if !self.warehouse.container.is_empty() {
                    cfg.insert(
                        "catalog".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.container)),
                    );
                }
                if !self.warehouse.namespace.is_empty() {
                    cfg.insert(
                        "schema".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.namespace)),
                    );
                }
                serde_json::Value::Object(cfg)
            }
            WarehouseKind::Synapse => {
                let extras = &self.warehouse.extras;
                let mut cfg = serde_json::Map::new();
                if let Some(v) = extra_str(extras, "connection_string") {
                    cfg.insert("connection_string".into(), serde_json::Value::String(v));
                }
                if !self.warehouse.namespace.is_empty() {
                    cfg.insert(
                        "schema".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.namespace)),
                    );
                }
                serde_json::Value::Object(cfg)
            }
            WarehouseKind::Redshift => {
                let extras = &self.warehouse.extras;
                let mut cfg = serde_json::Map::new();
                if !self.warehouse.container.is_empty() {
                    cfg.insert(
                        "database".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.container)),
                    );
                }
                for json_key in [
                    "cluster_identifier",
                    "workgroup_name",
                    "db_user",
                    "region",
                    "staging_s3_bucket",
                    "staging_s3_prefix",
                    "iam_role_arn",
                ] {
                    if let Some(v) = extra_str(extras, json_key) {
                        cfg.insert(json_key.into(), serde_json::Value::String(v));
                    }
                }
                serde_json::Value::Object(cfg)
            }
            WarehouseKind::Clickhouse => {
                let extras = &self.warehouse.extras;
                let mut cfg = serde_json::Map::new();
                if let Some(v) = extra_str(extras, "url") {
                    cfg.insert("url".into(), serde_json::Value::String(v));
                }
                if !self.warehouse.container.is_empty() {
                    cfg.insert(
                        "database".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.container)),
                    );
                }
                if let Some(v) = extra_str(extras, "user") {
                    cfg.insert("user".into(), serde_json::Value::String(v));
                }
                if let Some(v) = extra_str(extras, "password") {
                    cfg.insert("password".into(), serde_json::Value::String(v));
                }
                serde_json::Value::Object(cfg)
            }
            WarehouseKind::Motherduck => {
                let extras = &self.warehouse.extras;
                let mut cfg = serde_json::Map::new();
                if let Some(v) = extra_str(extras, "motherduck_token") {
                    cfg.insert("motherduck_token".into(), serde_json::Value::String(v));
                }
                if !self.warehouse.container.is_empty() {
                    cfg.insert(
                        "database".into(),
                        serde_json::Value::String(resolve_env_ref(&self.warehouse.container)),
                    );
                }
                serde_json::Value::Object(cfg)
            }
            WarehouseKind::Mssql => serde_json::json!({}),
        }
    }
}

fn skippr_plugin_name(kind: &str) -> String {
    match kind {
        "delta_lake" => "DeltaLake".to_string(),
        "google_analytics" => "GoogleAnalytics".to_string(),
        "google_search_console" => "GoogleSearchConsole".to_string(),
        "bing_webmaster_tools" => "BingWebmasterTools".to_string(),
        "google_pagespeed" => "GooglePageSpeed".to_string(),
        "site_quality" => "SiteQuality".to_string(),
        "ai_citations" => "AiCitations".to_string(),
        "google_serp_ranks" => "GoogleSerpRanks".to_string(),
        "seo_crawl" => "SeoCrawl".to_string(),
        "apple_search_ads" => "AppleSearchAds".to_string(),
        "dataforseo_backlinks" => "DataForSeoBacklinks".to_string(),
        "dataforseo_seo_opportunities" => "DataForSeoSeoOpportunities".to_string(),
        "meta_instagram_ads" => "MetaInstagramAds".to_string(),
        "http_client" => "HttpClient".to_string(),
        "http_server" => "HttpServer".to_string(),
        _ => capitalize_first(kind),
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn resolve_env_ref(value: &str) -> String {
    if value.starts_with("${") && value.ends_with('}') {
        let var_name = &value[2..value.len() - 1];
        std::env::var(var_name).unwrap_or_else(|_| value.to_string())
    } else {
        value.to_string()
    }
}

fn resolve_env_refs_in_value(val: &mut serde_json::Value) {
    match val {
        serde_json::Value::String(s) => {
            let resolved = resolve_env_ref(s);
            if resolved != *s {
                *s = resolved;
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                resolve_env_refs_in_value(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                resolve_env_refs_in_value(v);
            }
        }
        _ => {}
    }
}

fn resolve_to_absolute(path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return path.to_string();
    }
    std::env::current_dir()
        .ok()
        .map(|cwd| cwd.join(p).to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

fn insert_snowflake_env(wh: &WarehouseResolved, env: &mut HashMap<String, String>) {
    fn getenv(key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.trim().is_empty())
    }
    fn extra_str(extras: &serde_json::Value, key: &str) -> Option<String> {
        extras
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    if let Some(v) = getenv("SNOWFLAKE_ACCOUNT").or_else(|| extra_str(&wh.extras, "account")) {
        env.insert("SNOWFLAKE_ACCOUNT".into(), v);
    }
    if let Some(v) = getenv("SNOWFLAKE_USER").or_else(|| extra_str(&wh.extras, "user")) {
        env.insert("SNOWFLAKE_USER".into(), v);
    }
    if let Some(v) = getenv("SNOWFLAKE_PASSWORD").or_else(|| extra_str(&wh.extras, "password")) {
        env.insert("SNOWFLAKE_PASSWORD".into(), v);
    }
    if let Some(v) =
        getenv("SNOWFLAKE_PRIVATE_KEY_PATH").or_else(|| extra_str(&wh.extras, "private_key_path"))
    {
        env.insert("SNOWFLAKE_PRIVATE_KEY_PATH".into(), resolve_to_absolute(&v));
    }
    if let Some(v) = getenv("SNOWFLAKE_STAGE").or_else(|| extra_str(&wh.extras, "stage")) {
        env.insert("SNOWFLAKE_STAGE".into(), v);
    }
    if let Some(v) =
        getenv("SNOWFLAKE_STAGING_URI").or_else(|| extra_str(&wh.extras, "staging_uri"))
    {
        env.insert("SNOWFLAKE_STAGING_URI".into(), v);
    }
    if let Some(v) = getenv("SNOWFLAKE_STAGING_STORAGE_INTEGRATION")
        .or_else(|| extra_str(&wh.extras, "staging_storage_integration"))
    {
        env.insert("SNOWFLAKE_STAGING_STORAGE_INTEGRATION".into(), v);
    }
    if let Some(v) = getenv("SNOWFLAKE_WAREHOUSE").or_else(|| extra_str(&wh.extras, "warehouse")) {
        env.insert("SNOWFLAKE_WAREHOUSE".into(), v);
    }
    if let Some(v) = getenv("SNOWFLAKE_ROLE").or_else(|| extra_str(&wh.extras, "role")) {
        env.insert("SNOWFLAKE_ROLE".into(), v);
    }
    if let Some(v) = getenv("SNOWFLAKE_DATABASE") {
        env.insert("SNOWFLAKE_DATABASE".into(), v);
    } else if !wh.container.is_empty() {
        env.insert("SNOWFLAKE_DATABASE".into(), resolve_env_ref(&wh.container));
    }
    if let Some(v) = getenv("SNOWFLAKE_SCHEMA") {
        env.insert("SNOWFLAKE_SCHEMA".into(), v);
    } else if !wh.namespace.is_empty() {
        env.insert("SNOWFLAKE_SCHEMA".into(), resolve_env_ref(&wh.namespace));
    }
}

fn parse_discover_summary(stdout: &[u8]) -> Option<SkipprDiscoverResult> {
    let text = String::from_utf8_lossy(stdout);
    text.lines().filter_map(parse_json_line).find_map(|event| {
        if event.get("event").and_then(|v| v.as_str()) != Some("discover_complete") {
            return None;
        }

        Some(SkipprDiscoverResult {
            ok: event.get("ok").and_then(|v| v.as_bool()).unwrap_or(true),
            namespaces_count: event
                .get("namespaces_discovered")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            total_fields: event
                .get("total_fields")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            errors: Vec::new(),
        })
    })
}

fn parse_json_line(line: &str) -> Option<serde_json::Value> {
    serde_json::from_str(line.trim()).ok()
}

#[derive(Default)]
struct SyncSummaryAccumulator {
    tables_synced: usize,
    total_rows: u64,
    bytes: u64,
    rows_written: u64,
    errors: Vec<String>,
}

impl SyncSummaryAccumulator {
    fn record(&mut self, event: &serde_json::Value) {
        let Some(kind) = event.get("event").and_then(|v| v.as_str()) else {
            return;
        };

        match kind {
            "table_synced" | "namespace_synced" => {
                self.tables_synced += 1;
                self.update_counters(event);
            }
            "sync_complete" => {
                if let Some(count) = event.get("namespaces_synced").and_then(|v| v.as_u64()) {
                    self.tables_synced = count as usize;
                }
                self.update_counters(event);
            }
            "sync_status" | "batch_ingested" | "output_synced" => {
                self.update_counters(event);
            }
            "sync_error" | "error" => {
                if let Some(msg) = event
                    .get("error")
                    .or_else(|| event.get("message"))
                    .and_then(|v| v.as_str())
                {
                    self.errors.push(msg.to_string());
                }
            }
            _ => {}
        }
    }

    fn update_counters(&mut self, event: &serde_json::Value) {
        if let Some(total_rows) = event.get("total_rows").and_then(|v| v.as_u64()) {
            self.total_rows = total_rows;
        }
        if let Some(rows) = event.get("rows").and_then(|v| v.as_u64()) {
            self.total_rows = self.total_rows.max(rows);
        }
        if let Some(bytes) = event.get("bytes").and_then(|v| v.as_u64()) {
            self.bytes = bytes;
        }
        if let Some(rows_written) = event.get("rows_written").and_then(|v| v.as_u64()) {
            self.rows_written = rows_written;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env(vars: &[(&str, Option<&str>)], test: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(key, _)| ((*key).to_string(), std::env::var(key).ok()))
            .collect();

        for (key, value) in vars {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }

        test();

        for (key, value) in saved {
            match value {
                Some(value) => std::env::set_var(&key, value),
                None => std::env::remove_var(&key),
            }
        }
    }

    #[test]
    fn resolve_skippr_binary_uses_skipprd_binary_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("skipprd-custom");
        std::fs::write(&bin, b"").expect("write stub binary");
        with_env(
            &[("SKIPPRD_BINARY", Some(bin.to_str().expect("utf8 path")))],
            || {
                assert_eq!(
                    resolve_skippr_binary("skipprd"),
                    bin.to_string_lossy().as_ref()
                );
            },
        );
    }

    #[test]
    fn resolve_skippr_binary_keeps_configured_absolute_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("skipprd-abs");
        std::fs::write(&bin, b"").expect("write stub binary");
        assert_eq!(
            resolve_skippr_binary(bin.to_str().expect("utf8 path")),
            bin.to_string_lossy().as_ref()
        );
    }

    fn postgres_provider() -> SkipprCliProvider {
        SkipprCliProvider {
            binary: "skipprd".to_string(),
            data_dir: PathBuf::from("/tmp/skippr-provider-test"),
            tenant: "tenant".to_string(),
            workspace: "workspace".to_string(),
            el_config: ElToolResolved::default(),
            warehouse: WarehouseResolved {
                kind: WarehouseKind::Postgres,
                container: "skippr_test".to_string(),
                namespace: "public".to_string(),
                extras: serde_json::json!({}),
            },
            storage_mode: Some("local".to_string()),
            storage_bucket: None,
        }
    }

    fn databricks_provider(token: &str) -> SkipprCliProvider {
        SkipprCliProvider {
            binary: "skipprd".to_string(),
            data_dir: PathBuf::from("/tmp/skippr-provider-test"),
            tenant: "tenant".to_string(),
            workspace: "workspace".to_string(),
            el_config: ElToolResolved::default(),
            warehouse: WarehouseResolved {
                kind: WarehouseKind::Databricks,
                container: "main".to_string(),
                namespace: "default".to_string(),
                extras: serde_json::json!({
                    "workspace_url": "https://dbc-example.cloud.databricks.com",
                    "token": token,
                    "warehouse_id": "abc123",
                }),
            },
            storage_mode: Some("local".to_string()),
            storage_bucket: None,
        }
    }

    fn snowflake_provider() -> SkipprCliProvider {
        SkipprCliProvider {
            binary: "skipprd".to_string(),
            data_dir: PathBuf::from("/tmp/skippr-provider-test"),
            tenant: "tenant".to_string(),
            workspace: "workspace".to_string(),
            el_config: ElToolResolved::default(),
            warehouse: WarehouseResolved {
                kind: WarehouseKind::Snowflake,
                container: "ANALYTICS".to_string(),
                namespace: "RAW".to_string(),
                extras: serde_json::json!({
                    "account": "acct",
                    "user": "svc_user",
                    "warehouse": "COMPUTE_WH",
                    "role": "ACCOUNTADMIN",
                    "stage": "@skippr_stage",
                    "staging_uri": "azure://acct.blob.core.windows.net/container/prefix",
                    "staging_storage_integration": "SNOWFLAKE_AZURE_INT",
                }),
            },
            storage_mode: Some("local".to_string()),
            storage_bucket: None,
        }
    }

    #[test]
    fn postgres_warehouse_uses_skipprd_env_vars_for_runtime_config() {
        with_env(
            &[
                ("POSTGRES_HOST", Some("localhost")),
                ("POSTGRES_PORT", Some("15433")),
                ("POSTGRES_USER", Some("postgres")),
                ("POSTGRES_PASSWORD", Some("testpass")),
                ("POSTGRES_DATABASE", Some("skippr_test")),
                ("POSTGRES_SCHEMA", Some("public")),
                ("POSTGRES_SSLMODE", Some("disable")),
            ],
            || {
                let provider = postgres_provider();

                let env = provider.env_vars();
                assert_eq!(
                    env.get("SKIPPR_CONFIG_FILE").map(String::as_str),
                    Some("/tmp/skippr-provider-test/skippr.yml")
                );
                assert_eq!(
                    env.get("POSTGRES_HOST").map(String::as_str),
                    Some("localhost")
                );
                assert_eq!(env.get("POSTGRES_PORT").map(String::as_str), Some("15433"));
                assert_eq!(
                    env.get("POSTGRES_DATABASE").map(String::as_str),
                    Some("skippr_test")
                );

                let cfg = provider.build_output_config(None);
                assert_eq!(cfg.get("host").and_then(|v| v.as_str()), Some("localhost"));
                assert_eq!(cfg.get("port").and_then(|v| v.as_u64()), Some(15433));
                assert_eq!(cfg.get("user").and_then(|v| v.as_str()), Some("postgres"));
                assert_eq!(
                    cfg.get("password").and_then(|v| v.as_str()),
                    Some("testpass")
                );
                assert_eq!(
                    cfg.get("database").and_then(|v| v.as_str()),
                    Some("skippr_test")
                );
                assert_eq!(cfg.get("schema").and_then(|v| v.as_str()), Some("public"));
                assert_eq!(cfg.get("sslmode").and_then(|v| v.as_str()), Some("disable"));
            },
        );
    }

    #[test]
    fn databricks_warehouse_resolves_env_refs_from_config_fields() {
        with_env(&[("WAREHOUSE_TOKEN", Some("dapi-test-token"))], || {
            let provider = databricks_provider("${WAREHOUSE_TOKEN}");

            let env = provider.env_vars();
            assert_eq!(
                env.get("DATA_OUTPUT_PLUGIN_NAME").map(String::as_str),
                Some("Databricks")
            );
            assert!(!env.contains_key("DATABRICKS_WORKSPACE_URL"));
            assert!(!env.contains_key("DATABRICKS_TOKEN"));
            assert!(!env.contains_key("DATABRICKS_WAREHOUSE_ID"));

            let cfg = provider.build_output_config(None);
            assert_eq!(
                cfg.get("workspace_url").and_then(|v| v.as_str()),
                Some("https://dbc-example.cloud.databricks.com"),
            );
            assert_eq!(
                cfg.get("token").and_then(|v| v.as_str()),
                Some("dapi-test-token")
            );
            assert_eq!(
                cfg.get("warehouse_id").and_then(|v| v.as_str()),
                Some("abc123")
            );
            assert_eq!(cfg.get("catalog").and_then(|v| v.as_str()), Some("main"));
            assert_eq!(cfg.get("schema").and_then(|v| v.as_str()), Some("default"));
        });
    }

    #[test]
    fn snowflake_warehouse_maps_cross_cloud_staging_config() {
        with_env(
            &[
                ("AZURE_STORAGE_SAS_TOKEN", Some("sas-token")),
                (
                    "GOOGLE_APPLICATION_CREDENTIALS",
                    Some("/tmp/gcs-service-account.json"),
                ),
            ],
            || {
                let provider = snowflake_provider();

                let env = provider.env_vars();
                assert_eq!(
                    env.get("SNOWFLAKE_STAGE").map(String::as_str),
                    Some("@skippr_stage")
                );
                assert_eq!(
                    env.get("SNOWFLAKE_STAGING_URI").map(String::as_str),
                    Some("azure://acct.blob.core.windows.net/container/prefix")
                );
                assert_eq!(
                    env.get("SNOWFLAKE_STAGING_STORAGE_INTEGRATION")
                        .map(String::as_str),
                    Some("SNOWFLAKE_AZURE_INT")
                );

                let cfg = provider.build_output_config(None);
                assert_eq!(cfg.get("account").and_then(|v| v.as_str()), Some("acct"));
                assert_eq!(cfg.get("user").and_then(|v| v.as_str()), Some("svc_user"));
                assert_eq!(
                    cfg.get("database").and_then(|v| v.as_str()),
                    Some("ANALYTICS")
                );
                assert_eq!(cfg.get("schema").and_then(|v| v.as_str()), Some("RAW"));
                assert_eq!(
                    cfg.get("stage").and_then(|v| v.as_str()),
                    Some("@skippr_stage")
                );
                assert_eq!(
                    cfg.get("staging_uri").and_then(|v| v.as_str()),
                    Some("azure://acct.blob.core.windows.net/container/prefix")
                );
                assert_eq!(
                    cfg.get("staging_storage_integration")
                        .and_then(|v| v.as_str()),
                    Some("SNOWFLAKE_AZURE_INT")
                );
                assert_eq!(
                    cfg.get("staging_azure_sas_token").and_then(|v| v.as_str()),
                    Some("sas-token")
                );
                assert_eq!(
                    cfg.get("staging_gcs_service_account_key_path")
                        .and_then(|v| v.as_str()),
                    Some("/tmp/gcs-service-account.json")
                );
            },
        );
    }

    fn athena_provider() -> SkipprCliProvider {
        SkipprCliProvider {
            binary: "skipprd".to_string(),
            data_dir: PathBuf::from("/tmp/skippr-provider-test"),
            tenant: "tenant".to_string(),
            workspace: "workspace".to_string(),
            el_config: ElToolResolved::default(),
            warehouse: WarehouseResolved {
                kind: WarehouseKind::Athena,
                container: "AwsDataCatalog".to_string(),
                namespace: "bike_hire".to_string(),
                extras: serde_json::json!({
                    "workgroup": "bikehire",
                    "region": "us-east-1",
                    "result_s3": "s3://skippr-e2e-sample-data-output/results-test3",
                }),
            },
            storage_mode: Some("local".to_string()),
            storage_bucket: None,
        }
    }

    #[test]
    fn athena_warehouse_maps_public_config_into_skipprd_sink_config() {
        let provider = athena_provider();

        let env = provider.env_vars();
        assert_eq!(
            env.get("AWS_DEFAULT_REGION").map(String::as_str),
            Some("us-east-1")
        );
        assert_eq!(
            env.get("ATHENA_WORKGROUP").map(String::as_str),
            Some("bikehire")
        );
        assert_eq!(
            env.get("ATHENA_RESULT_S3").map(String::as_str),
            Some("s3://skippr-e2e-sample-data-output/results-test3")
        );

        let cfg = provider.build_output_config(None);
        assert_eq!(
            cfg.get("s3_bucket").and_then(|v| v.as_str()),
            Some("skippr-e2e-sample-data-output")
        );
        assert_eq!(
            cfg.get("s3_prefix").and_then(|v| v.as_str()),
            Some("results-test3")
        );
        assert_eq!(
            cfg.get("athena_workgroup_name").and_then(|v| v.as_str()),
            Some("bikehire")
        );
        assert_eq!(
            cfg.get("athena_results_s3_bucket").and_then(|v| v.as_str()),
            Some("skippr-e2e-sample-data-output")
        );
        assert_eq!(
            cfg.get("glue_database_name").and_then(|v| v.as_str()),
            Some("bike_hire")
        );
    }

    #[test]
    fn skippr_yml_uses_runtime_plugin_names_for_snake_case_sources() {
        let provider = postgres_provider();
        let config = SkipprPipelineConfig {
            pipeline_name: "delta".to_string(),
            skippr_input: serde_json::json!({
                "kind": "delta_lake",
                "table_uri": "s3://bucket/path",
            }),
            output_plugin: react_suite_data_engineer::providers::SkipprOutputConfig {
                kind: "postgres".to_string(),
                ..Default::default()
            },
            schema_sink: None,
        };

        let yml = provider.generate_skippr_yml(&config);
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yml).unwrap();
        let source = parsed
            .get("data_sources")
            .and_then(|v| v.get("source"))
            .and_then(|v| v.as_mapping())
            .unwrap();
        assert!(source.contains_key(&serde_yaml::Value::String("DeltaLake".to_string())));
        assert!(!source.contains_key(&serde_yaml::Value::String("Delta_lake".to_string())));
    }

    #[test]
    fn parses_compact_discover_summary() {
        let summary = parse_discover_summary(
            br#"
{"event":"discover_start","pipeline":"pipe","timestamp":"2026-04-28T00:00:00Z"}
{"event":"discover_complete","ok":true,"pipeline":"pipe","namespaces_discovered":1,"total_fields":2,"elapsed_ms":10,"timestamp":"2026-04-28T00:00:02Z"}
"#,
        )
        .expect("discover summary should parse");

        assert!(summary.ok);
        assert_eq!(summary.namespaces_count, 1);
        assert_eq!(summary.total_fields, 2);
    }

    #[test]
    fn storage_config_is_explicit_without_forcing_local() {
        let provider = SkipprCliProvider {
            storage_mode: Some("s3".to_string()),
            storage_bucket: Some("skippr-prod-metadata".to_string()),
            ..postgres_provider()
        };

        let env = provider.env_vars();
        assert_eq!(
            env.get("SKIPPRD_EL_STORAGE_MODE").map(String::as_str),
            Some("s3")
        );
        assert_eq!(
            env.get("SKIPPR_S3_BUCKET").map(String::as_str),
            Some("skippr-prod-metadata")
        );

        let config = SkipprPipelineConfig {
            pipeline_name: "pipe".to_string(),
            skippr_input: serde_json::json!({
                "kind": "postgres",
                "connection_string": "${POSTGRES_CONNECTION_STRING}",
            }),
            output_plugin: react_suite_data_engineer::providers::SkipprOutputConfig {
                kind: "postgres".to_string(),
                ..Default::default()
            },
            schema_sink: None,
        };
        let yml = provider.generate_skippr_yml(&config);
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yml).unwrap();

        assert_eq!(
            parsed
                .get("skippr")
                .and_then(|v| v.get("skipprd_el_storage_mode"))
                .and_then(|v| v.as_str()),
            Some("s3")
        );
        assert_eq!(
            parsed
                .get("skippr")
                .and_then(|v| v.get("skippr_s3_bucket"))
                .and_then(|v| v.as_str()),
            Some("skippr-prod-metadata")
        );
    }
}

#[async_trait]
impl SkipprProvider for SkipprCliProvider {
    async fn write_pipeline_config(
        &self,
        _scope: &RequestScope,
        config: &SkipprPipelineConfig,
    ) -> Result<String, String> {
        let yml_content = self.generate_skippr_yml(config);
        let yml_path = self.skippr_yml_path();

        std::fs::create_dir_all(self.data_dir_path())
            .map_err(|e| format!("failed to create data dir: {}", e))?;

        std::fs::write(&yml_path, yml_content.as_bytes())
            .map_err(|e| format!("failed to write skippr.yml: {}", e))?;

        tracing::info!(path = %yml_path.display(), "wrote skippr.yml");
        Ok(yml_path.to_string_lossy().to_string())
    }

    async fn discover_pipeline(
        &self,
        _scope: &RequestScope,
        pipeline: &str,
    ) -> Result<SkipprDiscoverResult, String> {
        let output = self
            .run_skippr(&["discover", "--pipeline", pipeline, "--output", "json"])
            .await?;

        let mut errors = Vec::new();
        if !output.status.success() && errors.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            errors.push(format!("skippr discover failed: {}", stderr.trim()));
        }

        let summary = parse_discover_summary(&output.stdout).unwrap_or_default();

        Ok(SkipprDiscoverResult {
            ok: output.status.success(),
            namespaces_count: summary.namespaces_count,
            total_fields: summary.total_fields,
            errors,
        })
    }

    async fn show_pipeline(
        &self,
        _scope: &RequestScope,
        pipeline: &str,
    ) -> Result<SkipprPipelineStatus, String> {
        let sql = format!("SHOW PIPELINE \"{}\"", pipeline);
        let output = self
            .run_skippr(&["query", "--plain", "--sql", &sql])
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("SHOW PIPELINE failed: {}", stderr.trim()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
            .map_err(|e| format!("failed to parse SHOW PIPELINE output: {}", e))?;

        let status = parsed
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let metadata_location = parsed
            .get("metadata_location")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let namespaces = parsed
            .get("namespaces")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|ns| {
                        let namespace = ns
                            .get("name")
                            .or_else(|| ns.get("namespace"))?
                            .as_str()?
                            .to_string();
                        let fields = ns
                            .get("fields")
                            .and_then(|v| v.as_array())
                            .map(|farr| {
                                farr.iter()
                                    .filter_map(|f| {
                                        Some(SkipprFieldSchema {
                                            name: f.get("name")?.as_str()?.to_string(),
                                            field_type: f.get("type")?.as_str()?.to_string(),
                                            nullable: f
                                                .get("nullable")
                                                .and_then(|v| v.as_bool())
                                                .unwrap_or(true),
                                            source_field_name: f
                                                .get("source_field_name")
                                                .and_then(|v| v.as_str())
                                                .map(str::to_string),
                                            out_field_name: f
                                                .get("out_field_name")
                                                .and_then(|v| v.as_str())
                                                .map(str::to_string),
                                            field_id: f
                                                .get("field_id")
                                                .and_then(|v| v.as_i64())
                                                .and_then(|value| i32::try_from(value).ok()),
                                            lineage_id: f
                                                .get("lineage_id")
                                                .and_then(|v| v.as_str())
                                                .map(str::to_string),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        let offset = ns.get("offset").cloned();
                        let cdc_enabled = ns
                            .get("cdc_enabled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let last_checkpoint = ns
                            .get("last_checkpoint")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        Some(SkipprNamespaceStatus {
                            namespace,
                            fields,
                            offset,
                            cdc_enabled,
                            last_checkpoint,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(SkipprPipelineStatus {
            pipeline: pipeline.to_string(),
            status,
            namespaces,
            metadata_location,
        })
    }

    async fn load_schema(
        &self,
        _scope: &RequestScope,
        pipeline: &str,
        schema_json_path: &str,
    ) -> Result<(), String> {
        let sql = format!("LOAD SCHEMA '{}' INTO {}", schema_json_path, pipeline);
        let output = self
            .run_skippr(&["query", "--plain", "--sql", &sql])
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("LOAD SCHEMA failed: {}", stderr.trim()));
        }
        Ok(())
    }

    async fn sync_pipeline(
        &self,
        _scope: &RequestScope,
        pipeline: &str,
    ) -> Result<SkipprSyncResult, String> {
        let idle_timeout = std::time::Duration::from_secs(120);
        let (mut summary, stderr_bytes, success) = self
            .run_skippr_streaming(
                &["sync", "--pipeline", pipeline, "--once", "--output", "json"],
                idle_timeout,
            )
            .await?;

        if !success && summary.errors.is_empty() {
            let stderr = String::from_utf8_lossy(&stderr_bytes);
            summary
                .errors
                .push(format!("skippr sync failed: {}", stderr.trim()));
        }

        Ok(SkipprSyncResult {
            ok: success,
            tables_synced: summary.tables_synced,
            total_rows: summary.total_rows,
            bytes: summary.bytes,
            rows_written: summary.rows_written,
            errors: summary.errors,
        })
    }
}
