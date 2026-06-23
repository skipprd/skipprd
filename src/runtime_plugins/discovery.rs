use std::fs as std_fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{CACHE_CONTROL, PRAGMA, USER_AGENT};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::runtime_plugins::host::ResolvedRuntimePlugin;
use crate::runtime_plugins::manifest::RuntimePluginManifest;
use crate::runtime_plugins::protocol::RuntimePluginKind;

const DEFAULT_DISCOVERY_BASE_URL: &str = "https://install.skippr.io/releases/runtime-plugins";
const METADATA_REFRESH_QUERY_PARAM: &str = "skippr_metadata_refresh";
const LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR_ENV: &str = "SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR";
const USE_LOCAL_PLUGIN_CODE_ENV: &str = "USE_LOCAL_PLUGIN_CODE";
const METADATA_FETCH_MAX_ATTEMPTS: usize = 4;
const METADATA_FETCH_INITIAL_BACKOFF_MS: u64 = 200;

#[derive(Clone, Debug, Deserialize)]
struct RuntimePluginIndex {
    bundle_version: String,
    manifests: Vec<RuntimePluginIndexEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct RuntimePluginIndexEntry {
    plugin_name: String,
    kind: RuntimePluginKind,
    manifest_filename: String,
    manifest_url: String,
}

fn runtime_plugin_kind_label(kind: RuntimePluginKind) -> &'static str {
    match kind {
        RuntimePluginKind::DataSource => "data source",
        RuntimePluginKind::DataSink => "data sink",
        RuntimePluginKind::SchemaSink => "schema sink",
    }
}

fn format_runtime_plugin_names(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

fn missing_runtime_plugin_message(
    index: &RuntimePluginIndex,
    expected_kind: RuntimePluginKind,
    expected_plugin_name: &str,
    requested_plugin_version: &str,
    index_url: &str,
) -> String {
    let mut available_plugins_for_kind: Vec<String> = index
        .manifests
        .iter()
        .filter(|entry| entry.kind == expected_kind)
        .map(|entry| entry.plugin_name.clone())
        .collect();
    available_plugins_for_kind.sort();
    available_plugins_for_kind.dedup();

    let mut case_insensitive_matches: Vec<String> = available_plugins_for_kind
        .iter()
        .filter(|plugin_name| plugin_name.eq_ignore_ascii_case(expected_plugin_name))
        .cloned()
        .collect();
    case_insensitive_matches.sort();
    case_insensitive_matches.dedup();

    let case_hint = if case_insensitive_matches.is_empty() {
        String::new()
    } else {
        format!(
            "\nHint: a plugin with the same letters but different casing exists: {}.",
            format_runtime_plugin_names(&case_insensitive_matches)
        )
    };

    format!(
        "Skippr could not find a published runtime {} plugin named '{}'. \
This usually means the plugin has not been published yet, \
the published manifest uses a different name or casing, or the requested version is wrong.{}\
\n\nDebug details:\
\n- requested plugin: {}\
\n- requested kind: {:?}\
\n- requested version: {}\
\n- latest index label: {}\
\n- manifest index URL: {}\
\n- case-insensitive matches: {}\
\n- available plugins for this kind: {}",
        runtime_plugin_kind_label(expected_kind),
        expected_plugin_name,
        case_hint,
        expected_plugin_name,
        expected_kind,
        requested_plugin_version,
        index.bundle_version,
        index_url,
        format_runtime_plugin_names(&case_insensitive_matches),
        format_runtime_plugin_names(&available_plugins_for_kind),
    )
}

pub fn configured_runtime_plugin_version(configured_version: Option<&str>) -> Option<String> {
    configured_version
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub fn runtime_plugin_cache_root() -> PathBuf {
    if let Some(root) = std::env::var_os("SKIPPR_RUNTIME_PLUGIN_DIR") {
        return PathBuf::from(root);
    }

    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".skippr").join("runtime_plugins");
    }

    std::env::temp_dir().join("skippr_runtime_plugins")
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn use_local_plugin_code() -> bool {
    env_truthy(USE_LOCAL_PLUGIN_CODE_ENV)
}

fn local_runtime_plugin_manifest_dir() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR_ENV) {
        return Ok(PathBuf::from(path));
    }
    Ok(std::env::current_dir()?
        .join(".skippr")
        .join("local-runtime-plugins")
        .join("manifests"))
}

pub fn validate_resolved_plugin(
    resolved: ResolvedRuntimePlugin,
    expected_kind: RuntimePluginKind,
    expected_plugin_name: &str,
) -> io::Result<ResolvedRuntimePlugin> {
    if resolved.manifest.kind != expected_kind {
        return Err(io::Error::other(format!(
            "Runtime manifest '{}' is kind {:?}, expected {:?}",
            resolved.manifest.name, resolved.manifest.kind, expected_kind
        )));
    }

    if resolved.manifest.plugin_name != expected_plugin_name {
        return Err(io::Error::other(format!(
            "Runtime manifest '{}' targets plugin '{}' but pipeline config resolved '{}'",
            resolved.manifest.name, resolved.manifest.plugin_name, expected_plugin_name
        )));
    }

    Ok(resolved)
}

pub async fn resolve_runtime_plugin(
    expected_kind: RuntimePluginKind,
    expected_plugin_name: &str,
    configured_plugin_version: Option<&str>,
) -> io::Result<ResolvedRuntimePlugin> {
    let resolved = if use_local_plugin_code() {
        match resolve_local_runtime_plugin(expected_kind, expected_plugin_name)? {
            Some(resolved) => resolved,
            None => {
                discover_runtime_plugin(
                    expected_kind,
                    expected_plugin_name,
                    configured_plugin_version,
                )
                .await?
            }
        }
    } else {
        discover_runtime_plugin(
            expected_kind,
            expected_plugin_name,
            configured_plugin_version,
        )
        .await?
    };

    let resolved = validate_resolved_plugin(resolved, expected_kind, expected_plugin_name)?;
    info!(
        "Resolved runtime {} plugin '{}' version={} manifest={} path={}",
        runtime_plugin_kind_label(expected_kind),
        resolved.manifest.plugin_name,
        resolved.manifest.version,
        resolved.manifest.name,
        resolved.manifest_path.display(),
    );
    Ok(resolved)
}

fn resolve_local_runtime_plugin(
    expected_kind: RuntimePluginKind,
    expected_plugin_name: &str,
) -> io::Result<Option<ResolvedRuntimePlugin>> {
    let manifest_dir = local_runtime_plugin_manifest_dir()?;
    if !manifest_dir.exists() {
        info!(
            "USE_LOCAL_PLUGIN_CODE=1 but local runtime plugin manifest directory does not exist; falling back to published registry path={}",
            manifest_dir.display(),
        );
        return Ok(None);
    }
    if !manifest_dir.is_dir() {
        return Err(io::Error::other(format!(
            "USE_LOCAL_PLUGIN_CODE=1 but local runtime plugin manifest path is not a directory: {}",
            manifest_dir.display()
        )));
    }

    for manifest_path in sorted_json_files(&manifest_dir)? {
        let manifest = RuntimePluginManifest::load_from_path(&manifest_path).map_err(|err| {
            io::Error::other(format!(
                "failed to load local runtime plugin manifest {}: {}",
                manifest_path.display(),
                err
            ))
        })?;
        if manifest.kind != expected_kind || manifest.plugin_name != expected_plugin_name {
            continue;
        }

        let executable = manifest.resolve_executable(&manifest_path).map_err(|err| {
            io::Error::other(format!(
                "local runtime plugin manifest {} has invalid executable path: {}",
                manifest_path.display(),
                err
            ))
        })?;
        if !executable.exists() {
            return Err(io::Error::other(format!(
                "local runtime plugin manifest {} matched {} {:?} but executable does not exist: {}",
                manifest_path.display(),
                expected_plugin_name,
                expected_kind,
                executable.display()
            )));
        }

        info!(
            "Resolved local runtime {} plugin '{}' version={} manifest={} path={} executable={}",
            runtime_plugin_kind_label(expected_kind),
            manifest.plugin_name,
            manifest.version,
            manifest.name,
            manifest_path.display(),
            executable.display(),
        );
        return Ok(Some(ResolvedRuntimePlugin {
            manifest_path,
            manifest,
        }));
    }

    info!(
        "USE_LOCAL_PLUGIN_CODE=1 but no matching local runtime {} plugin '{}' manifest was found under {}; falling back to published registry",
        runtime_plugin_kind_label(expected_kind),
        expected_plugin_name,
        manifest_dir.display(),
    );
    Ok(None)
}

fn sorted_json_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std_fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext == std::ffi::OsStr::new("json"))
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

async fn discover_runtime_plugin(
    expected_kind: RuntimePluginKind,
    expected_plugin_name: &str,
    configured_plugin_version: Option<&str>,
) -> io::Result<ResolvedRuntimePlugin> {
    let client = reqwest::Client::new();
    let requested_plugin_version = configured_runtime_plugin_version(configured_plugin_version);
    let index_url = latest_manifest_index_url();
    info!(
        "Resolving runtime {} plugin '{}' from published registry requested_version='{}' index_url={}",
        runtime_plugin_kind_label(expected_kind),
        expected_plugin_name,
        requested_plugin_version.as_deref().unwrap_or("latest"),
        index_url,
    );
    let index = fetch_json::<RuntimePluginIndex>(&client, &index_url).await?;
    let latest_entry = index
        .manifests
        .iter()
        .find(|entry| entry.kind == expected_kind && entry.plugin_name == expected_plugin_name)
        .ok_or_else(|| {
            io::Error::other(missing_runtime_plugin_message(
                &index,
                expected_kind,
                expected_plugin_name,
                requested_plugin_version.as_deref().unwrap_or("latest"),
                &index_url,
            ))
        })?;
    let manifest_url = resolved_manifest_url(latest_entry, requested_plugin_version.as_deref())?;
    let cache_key = manifest_cache_key(&manifest_url, requested_plugin_version.as_deref());
    info!(
        "Resolved published runtime {} plugin '{}' via requested_version='{}' index_label='{}' manifest={} manifest_url={}",
        runtime_plugin_kind_label(expected_kind),
        expected_plugin_name,
        requested_plugin_version.as_deref().unwrap_or("latest"),
        index.bundle_version,
        latest_entry.manifest_filename,
        manifest_url,
    );
    let manifest_path = cache_manifest(&client, &cache_key, latest_entry, &manifest_url).await?;
    ResolvedRuntimePlugin::load(manifest_path)
}

fn latest_manifest_index_url() -> String {
    format!("{DEFAULT_DISCOVERY_BASE_URL}/latest/manifest-index.json")
}

fn rewrite_manifest_url_version(manifest_url: &str, version: &str) -> io::Result<String> {
    let (prefix, suffix) = manifest_url.split_once("/versions/").ok_or_else(|| {
        io::Error::other(format!(
            "runtime manifest URL is not versioned and cannot be pinned: {manifest_url}"
        ))
    })?;
    let (_current_version, remainder) = suffix.split_once('/').ok_or_else(|| {
        io::Error::other(format!(
            "runtime manifest URL is missing a version path segment: {manifest_url}"
        ))
    })?;
    Ok(format!("{prefix}/versions/{version}/{remainder}"))
}

fn resolved_manifest_url(
    latest_entry: &RuntimePluginIndexEntry,
    requested_plugin_version: Option<&str>,
) -> io::Result<String> {
    match requested_plugin_version {
        Some(version) => rewrite_manifest_url_version(&latest_entry.manifest_url, version),
        None => Ok(latest_entry.manifest_url.clone()),
    }
}

fn sanitize_cache_key(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}

fn manifest_cache_key(manifest_url: &str, requested_plugin_version: Option<&str>) -> String {
    if let Some(version) = requested_plugin_version {
        return sanitize_cache_key(version);
    }

    manifest_url
        .split("/versions/")
        .nth(1)
        .and_then(|suffix| suffix.split('/').next())
        .map(sanitize_cache_key)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "latest".to_string())
}

async fn cache_manifest(
    client: &reqwest::Client,
    cache_key: &str,
    entry: &RuntimePluginIndexEntry,
    manifest_url: &str,
) -> io::Result<PathBuf> {
    let manifest_dir = runtime_plugin_cache_root()
        .join("discovery")
        .join("manifests")
        .join(cache_key);
    let manifest_path = manifest_dir.join(&entry.manifest_filename);
    if manifest_path.exists() {
        info!(
            "Refreshing cached runtime plugin manifest metadata: plugin={} kind={:?} cache_key={} path={} url={}",
            entry.plugin_name,
            entry.kind,
            cache_key,
            manifest_path.display(),
            manifest_url,
        );
    }

    info!(
        "Downloading runtime plugin manifest: plugin={} kind={:?} cache_key={} url={} destination={}",
        entry.plugin_name,
        entry.kind,
        cache_key,
        manifest_url,
        manifest_path.display(),
    );
    let bytes = fetch_metadata_bytes(client, manifest_url).await?;
    fs::create_dir_all(&manifest_dir).await?;
    let mut file = fs::File::create(&manifest_path).await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    info!(
        "Downloaded runtime plugin manifest: plugin={} kind={:?} cache_key={} path={}",
        entry.plugin_name,
        entry.kind,
        cache_key,
        manifest_path.display(),
    );
    Ok(manifest_path)
}

async fn fetch_json<T>(client: &reqwest::Client, url: &str) -> io::Result<T>
where
    T: DeserializeOwned,
{
    let bytes = fetch_metadata_bytes(client, url).await?;
    serde_json::from_slice(&bytes).map_err(|err| io::Error::other(err.to_string()))
}

fn append_metadata_refresh_query(url: &str, refresh_token: &str) -> io::Result<String> {
    let mut parsed = reqwest::Url::parse(url)
        .map_err(|err| io::Error::other(format!("runtime discovery URL parse failed: {err}")))?;
    parsed
        .query_pairs_mut()
        .append_pair(METADATA_REFRESH_QUERY_PARAM, refresh_token);
    Ok(parsed.to_string())
}

async fn fetch_metadata_bytes(client: &reqwest::Client, url: &str) -> io::Result<Vec<u8>> {
    let mut last_error: Option<io::Error> = None;
    for attempt in 1..=METADATA_FETCH_MAX_ATTEMPTS {
        match fetch_metadata_bytes_once(client, url).await {
            Ok(bytes) => return Ok(bytes),
            Err(err) if attempt < METADATA_FETCH_MAX_ATTEMPTS && is_retryable_io_error(&err) => {
                let backoff = metadata_fetch_backoff(attempt);
                warn!(
                    "runtime discovery request failed transiently attempt={}/{} backoff_ms={} url={} error={}",
                    attempt,
                    METADATA_FETCH_MAX_ATTEMPTS,
                    backoff.as_millis(),
                    url,
                    err
                );
                last_error = Some(err);
                tokio::time::sleep(backoff).await;
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::other(format!("runtime discovery request failed for {url}"))))
}

fn metadata_fetch_backoff(attempt: usize) -> Duration {
    Duration::from_millis(METADATA_FETCH_INITIAL_BACKOFF_MS * (1_u64 << attempt.saturating_sub(1)))
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn retryable_error(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, message)
}

fn is_retryable_io_error(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted
}

async fn fetch_metadata_bytes_once(client: &reqwest::Client, url: &str) -> io::Result<Vec<u8>> {
    let refresh_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string();
    let refreshed_url = append_metadata_refresh_query(url, &refresh_token)?;
    let response = client
        .get(refreshed_url)
        .header(USER_AGENT, "skipprd")
        .header(CACHE_CONTROL, "no-cache, no-store, max-age=0")
        .header(PRAGMA, "no-cache")
        .send()
        .await
        .map_err(|err| retryable_error(format!("runtime discovery request failed: {err}")))?;

    if !response.status().is_success() {
        if is_retryable_status(response.status()) {
            return Err(retryable_error(format!(
                "runtime discovery request returned status {} for {}",
                response.status(),
                url
            )));
        }
        return Err(io::Error::other(format!(
            "runtime discovery request returned status {} for {}",
            response.status(),
            url
        )));
    }

    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|err| retryable_error(format!("runtime discovery read failed: {err}")))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;
    use serial_test::serial;
    use tempfile::tempdir;

    use crate::runtime_plugins::protocol::RuntimePluginKind;

    use super::{
        append_metadata_refresh_query, configured_runtime_plugin_version, is_retryable_status,
        latest_manifest_index_url, manifest_cache_key, metadata_fetch_backoff,
        missing_runtime_plugin_message, resolve_local_runtime_plugin, rewrite_manifest_url_version,
        runtime_plugin_cache_root, use_local_plugin_code, RuntimePluginIndex,
        RuntimePluginIndexEntry,
    };

    fn local_manifest_json(plugin_name: &str, kind: RuntimePluginKind, executable: &str) -> String {
        serde_json::to_string_pretty(&json!({
            "name": format!("{}-local", plugin_name.to_ascii_lowercase()),
            "kind": kind,
            "plugin_name": plugin_name,
            "version": "0.0.0-dev",
            "protocol_version": 1,
            "config_schema_version": 1,
            "executable": executable,
            "artifacts": {},
            "args": [],
            "supports_schema": false
        }))
        .unwrap()
    }

    #[test]
    fn manifest_index_url_uses_latest_prefix() {
        assert_eq!(
            latest_manifest_index_url(),
            "https://install.skippr.io/releases/runtime-plugins/latest/manifest-index.json"
        );
    }

    #[test]
    fn versioned_manifest_url_uses_requested_version() {
        assert_eq!(
            rewrite_manifest_url_version(
                "https://install.skippr.io/releases/runtime-plugins/plugins/s3-source/versions/8.1.0/s3-source.json",
                "9.9.9",
            )
            .unwrap(),
            "https://install.skippr.io/releases/runtime-plugins/plugins/s3-source/versions/9.9.9/s3-source.json"
        );
    }

    #[test]
    fn configured_plugin_version_uses_plugin_config_when_present() {
        assert_eq!(
            configured_runtime_plugin_version(Some("9.9.9")).as_deref(),
            Some("9.9.9")
        );
    }

    #[test]
    fn configured_plugin_version_treats_empty_config_as_unset() {
        assert_eq!(configured_runtime_plugin_version(Some("  ")), None);
    }

    #[test]
    fn configured_plugin_version_defaults_to_latest_when_unset() {
        assert_eq!(configured_runtime_plugin_version(None), None);
    }

    #[test]
    fn manifest_cache_key_prefers_requested_version() {
        assert_eq!(
            manifest_cache_key(
                "https://install.skippr.io/releases/runtime-plugins/plugins/s3-source/versions/8.1.0/s3-source.json",
                Some("9.9.9"),
            ),
            "9.9.9"
        );
    }

    #[test]
    fn metadata_refresh_query_is_appended_to_manifest_urls() {
        let refreshed = append_metadata_refresh_query(
            "https://install.skippr.io/releases/runtime-plugins/latest/manifest-index.json",
            "123",
        )
        .unwrap();
        assert!(refreshed.contains("skippr_metadata_refresh=123"));
    }

    #[test]
    fn transient_registry_statuses_are_retryable() {
        assert!(is_retryable_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(reqwest::StatusCode::REQUEST_TIMEOUT));
        assert!(!is_retryable_status(reqwest::StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn metadata_fetch_backoff_is_bounded_and_exponential() {
        assert_eq!(
            metadata_fetch_backoff(1),
            std::time::Duration::from_millis(200)
        );
        assert_eq!(
            metadata_fetch_backoff(2),
            std::time::Duration::from_millis(400)
        );
        assert_eq!(
            metadata_fetch_backoff(3),
            std::time::Duration::from_millis(800)
        );
    }

    #[test]
    fn missing_plugin_message_is_human_friendly_and_debuggable() {
        let index = RuntimePluginIndex {
            bundle_version: "8.1.0".to_string(),
            manifests: vec![
                RuntimePluginIndexEntry {
                    plugin_name: "S3".to_string(),
                    kind: RuntimePluginKind::DataSource,
                    manifest_filename: "s3.json".to_string(),
                    manifest_url: "https://install.skippr.io/s3.json".to_string(),
                },
                RuntimePluginIndexEntry {
                    plugin_name: "File".to_string(),
                    kind: RuntimePluginKind::DataSource,
                    manifest_filename: "file.json".to_string(),
                    manifest_url: "https://install.skippr.io/file.json".to_string(),
                },
                RuntimePluginIndexEntry {
                    plugin_name: "Athena".to_string(),
                    kind: RuntimePluginKind::DataSink,
                    manifest_filename: "athena.json".to_string(),
                    manifest_url: "https://install.skippr.io/athena.json".to_string(),
                },
            ],
        };

        let message = missing_runtime_plugin_message(
            &index,
            RuntimePluginKind::DataSource,
            "s3",
            "latest",
            "https://install.skippr.io/releases/runtime-plugins/latest/manifest-index.json",
        );

        assert!(message
            .contains("Skippr could not find a published runtime data source plugin named 's3'."));
        assert!(message
            .contains("Hint: a plugin with the same letters but different casing exists: S3."));
        assert!(message.contains("- requested version: latest"));
        assert!(message.contains("- latest index label: 8.1.0"));
        assert!(message.contains("- case-insensitive matches: S3"));
        assert!(message.contains("- available plugins for this kind: File, S3"));
    }

    #[test]
    #[serial]
    fn local_plugin_code_is_opt_in() {
        std::env::remove_var("USE_LOCAL_PLUGIN_CODE");
        assert!(!use_local_plugin_code());
        std::env::set_var("USE_LOCAL_PLUGIN_CODE", "1");
        assert!(use_local_plugin_code());
        std::env::remove_var("USE_LOCAL_PLUGIN_CODE");
    }

    #[test]
    #[serial]
    fn local_manifest_resolves_matching_plugin() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("manifests");
        fs::create_dir_all(&manifest_dir).unwrap();
        let executable = temp.path().join("skippr-plugin-data-source-mssql");
        fs::write(&executable, b"local plugin").unwrap();
        fs::write(
            manifest_dir.join("mssql-source.json"),
            local_manifest_json(
                "Mssql",
                RuntimePluginKind::DataSource,
                executable.to_str().unwrap(),
            ),
        )
        .unwrap();

        std::env::set_var("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR", &manifest_dir);
        let resolved =
            resolve_local_runtime_plugin(RuntimePluginKind::DataSource, "Mssql").unwrap();
        std::env::remove_var("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR");

        let resolved = resolved.expect("matching local manifest should resolve");
        assert_eq!(resolved.manifest.plugin_name, "Mssql");
        assert_eq!(resolved.manifest.kind, RuntimePluginKind::DataSource);
    }

    #[test]
    #[serial]
    fn local_manifest_absence_falls_back_to_published_discovery() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("missing");
        std::env::set_var("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR", &manifest_dir);
        let resolved =
            resolve_local_runtime_plugin(RuntimePluginKind::DataSource, "Mssql").unwrap();
        std::env::remove_var("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR");

        assert!(resolved.is_none());
    }

    #[test]
    #[serial]
    fn local_manifest_miss_falls_back_to_published_discovery() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("manifests");
        fs::create_dir_all(&manifest_dir).unwrap();
        let executable = temp.path().join("skippr-plugin-data-source-postgres");
        fs::write(&executable, b"local plugin").unwrap();
        fs::write(
            manifest_dir.join("postgres-source.json"),
            local_manifest_json(
                "Postgres",
                RuntimePluginKind::DataSource,
                executable.to_str().unwrap(),
            ),
        )
        .unwrap();

        std::env::set_var("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR", &manifest_dir);
        let resolved =
            resolve_local_runtime_plugin(RuntimePluginKind::DataSource, "Mssql").unwrap();
        std::env::remove_var("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR");

        assert!(resolved.is_none());
    }

    #[test]
    #[serial]
    fn malformed_local_manifest_fails_loudly() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("manifests");
        fs::create_dir_all(&manifest_dir).unwrap();
        fs::write(manifest_dir.join("mssql-source.json"), b"{not-json").unwrap();

        std::env::set_var("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR", &manifest_dir);
        let err = resolve_local_runtime_plugin(RuntimePluginKind::DataSource, "Mssql").unwrap_err();
        std::env::remove_var("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR");

        assert!(err
            .to_string()
            .contains("failed to load local runtime plugin manifest"));
    }

    #[test]
    #[serial]
    fn matching_local_manifest_with_missing_executable_fails_loudly() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("manifests");
        fs::create_dir_all(&manifest_dir).unwrap();
        let executable = temp.path().join("missing-plugin");
        fs::write(
            manifest_dir.join("mssql-source.json"),
            local_manifest_json(
                "Mssql",
                RuntimePluginKind::DataSource,
                executable.to_str().unwrap(),
            ),
        )
        .unwrap();

        std::env::set_var("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR", &manifest_dir);
        let err = resolve_local_runtime_plugin(RuntimePluginKind::DataSource, "Mssql").unwrap_err();
        std::env::remove_var("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR");

        assert!(err.to_string().contains("executable does not exist"));
    }

    #[test]
    #[serial]
    fn cache_root_prefers_runtime_plugin_dir_override() {
        let expected = if cfg!(windows) {
            PathBuf::from(r"C:\skippr-runtime")
        } else {
            PathBuf::from("/tmp/skippr-runtime")
        };
        std::env::set_var("SKIPPR_RUNTIME_PLUGIN_DIR", &expected);
        assert_eq!(runtime_plugin_cache_root(), expected);
        std::env::remove_var("SKIPPR_RUNTIME_PLUGIN_DIR");
    }
}
