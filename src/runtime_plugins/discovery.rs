use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::header::{CACHE_CONTROL, PRAGMA, USER_AGENT};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::info;

use crate::helpers::configuration::RuntimePluginEntry;
use crate::runtime_plugins::host::ResolvedRuntimePlugin;
use crate::runtime_plugins::protocol::RuntimePluginKind;

const DEFAULT_DISCOVERY_BASE_URL: &str = "https://install.skippr.io/releases/runtime-plugins";
const METADATA_REFRESH_QUERY_PARAM: &str = "skippr_metadata_refresh";

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
    explicit_entry: Option<RuntimePluginEntry>,
    expected_kind: RuntimePluginKind,
    expected_plugin_name: &str,
    configured_plugin_version: Option<&str>,
) -> io::Result<ResolvedRuntimePlugin> {
    let resolved = match explicit_entry {
        Some(entry) => ResolvedRuntimePlugin::load(PathBuf::from(entry.manifest))?,
        None => {
            discover_runtime_plugin(
                expected_kind,
                expected_plugin_name,
                configured_plugin_version,
            )
            .await?
        }
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
        .map_err(|err| io::Error::other(format!("runtime discovery request failed: {err}")))?;

    if !response.status().is_success() {
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
        .map_err(|err| io::Error::other(format!("runtime discovery read failed: {err}")))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serial_test::serial;

    use crate::runtime_plugins::protocol::RuntimePluginKind;

    use super::{
        append_metadata_refresh_query, configured_runtime_plugin_version,
        latest_manifest_index_url, manifest_cache_key, missing_runtime_plugin_message,
        rewrite_manifest_url_version, runtime_plugin_cache_root, RuntimePluginIndex,
        RuntimePluginIndexEntry,
    };

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
