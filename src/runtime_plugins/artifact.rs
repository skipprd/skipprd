use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::runtime_plugins::manifest::{RuntimePluginArtifact, RuntimePluginManifest};

const ARTIFACT_DOWNLOAD_MAX_ATTEMPTS: usize = 4;
const ARTIFACT_DOWNLOAD_INITIAL_BACKOFF_MS: u64 = 200;

pub async fn resolve_plugin_executable(
    manifest_path: &Path,
    manifest: &RuntimePluginManifest,
) -> io::Result<PathBuf> {
    if let Some(artifact) = manifest.artifact_for_current_target() {
        let executable = resolve_artifact(manifest_path, manifest, artifact).await?;
        verify_sha256(&executable, artifact.sha256.as_deref()).await?;
        return Ok(executable);
    }

    let executable = manifest.resolve_executable(manifest_path)?;
    info!(
        "Using runtime plugin executable from manifest path: plugin={} kind={:?} version={} target={} path={}",
        manifest.plugin_name,
        manifest.kind,
        manifest.version,
        RuntimePluginManifest::current_target(),
        executable.display(),
    );
    Ok(executable)
}

async fn resolve_artifact(
    manifest_path: &Path,
    manifest: &RuntimePluginManifest,
    artifact: &RuntimePluginArtifact,
) -> io::Result<PathBuf> {
    if let Some(url) = artifact.url.as_deref() {
        let destination = install_destination(manifest_path, manifest, artifact)?;
        if destination.exists() {
            if verify_sha256(&destination, artifact.sha256.as_deref())
                .await
                .is_ok()
                || artifact.sha256.is_none()
            {
                info!(
                    "Using cached runtime plugin binary: plugin={} kind={:?} version={} target={} path={} url={}",
                    manifest.plugin_name,
                    manifest.kind,
                    manifest.version,
                    RuntimePluginManifest::current_target(),
                    destination.display(),
                    url,
                );
                return Ok(destination);
            }
        }
        info!(
            "Downloading runtime plugin binary: plugin={} kind={:?} version={} target={} url={} destination={}",
            manifest.plugin_name,
            manifest.kind,
            manifest.version,
            RuntimePluginManifest::current_target(),
            url,
            destination.display(),
        );
        download_artifact(url, &destination).await?;
        verify_sha256(&destination, artifact.sha256.as_deref()).await?;
        info!(
            "Downloaded runtime plugin binary: plugin={} kind={:?} version={} target={} path={}",
            manifest.plugin_name,
            manifest.kind,
            manifest.version,
            RuntimePluginManifest::current_target(),
            destination.display(),
        );
        return Ok(destination);
    }

    let executable = manifest.resolve_executable_path(manifest_path, &artifact.executable)?;
    info!(
        "Using runtime plugin executable from manifest artifact: plugin={} kind={:?} version={} target={} path={}",
        manifest.plugin_name,
        manifest.kind,
        manifest.version,
        RuntimePluginManifest::current_target(),
        executable.display(),
    );
    Ok(executable)
}

fn install_destination(
    manifest_path: &Path,
    manifest: &RuntimePluginManifest,
    artifact: &RuntimePluginArtifact,
) -> io::Result<PathBuf> {
    let install_root = install_root(manifest_path, manifest)?;
    let artifact_name = Path::new(&artifact.executable)
        .file_name()
        .map(|name| name.to_owned())
        .ok_or_else(|| io::Error::other("runtime plugin artifact executable has no filename"))?;
    Ok(install_root
        .join(&manifest.name)
        .join(&manifest.version)
        .join(RuntimePluginManifest::current_target())
        .join(artifact_name))
}

fn install_root(manifest_path: &Path, manifest: &RuntimePluginManifest) -> io::Result<PathBuf> {
    if let Some(root) = std::env::var_os("SKIPPR_RUNTIME_PLUGIN_DIR") {
        return Ok(PathBuf::from(root));
    }

    if let Some(root) = manifest.install_root.as_deref() {
        return manifest.resolve_executable_path(manifest_path, root);
    }

    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".skippr").join("runtime_plugins"));
    }

    Ok(std::env::temp_dir().join("skippr_runtime_plugins"))
}

async fn download_artifact(url: &str, destination: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).await?;
    }

    let bytes = download_artifact_bytes(url).await?;

    let mut file = fs::File::create(destination).await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    set_executable_permissions(destination).await?;
    Ok(())
}

async fn download_artifact_bytes(url: &str) -> io::Result<bytes::Bytes> {
    let mut last_error: Option<io::Error> = None;
    for attempt in 1..=ARTIFACT_DOWNLOAD_MAX_ATTEMPTS {
        match download_artifact_bytes_once(url).await {
            Ok(bytes) => return Ok(bytes),
            Err(err) if attempt < ARTIFACT_DOWNLOAD_MAX_ATTEMPTS && is_retryable_io_error(&err) => {
                let backoff = artifact_download_backoff(attempt);
                warn!(
                    "runtime artifact download failed transiently attempt={}/{} backoff_ms={} url={} error={}",
                    attempt,
                    ARTIFACT_DOWNLOAD_MAX_ATTEMPTS,
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
        .unwrap_or_else(|| io::Error::other(format!("runtime artifact download failed for {url}"))))
}

async fn download_artifact_bytes_once(url: &str) -> io::Result<bytes::Bytes> {
    let response = reqwest::get(url)
        .await
        .map_err(|err| retryable_error(format!("runtime artifact download failed: {}", err)))?;
    if !response.status().is_success() {
        if is_retryable_status(response.status()) {
            return Err(retryable_error(format!(
                "runtime artifact download returned status {} for {}",
                response.status(),
                url
            )));
        }
        return Err(io::Error::other(format!(
            "runtime artifact download returned status {} for {}",
            response.status(),
            url
        )));
    }

    response
        .bytes()
        .await
        .map_err(|err| retryable_error(format!("runtime artifact download read failed: {}", err)))
}

fn artifact_download_backoff(attempt: usize) -> Duration {
    Duration::from_millis(
        ARTIFACT_DOWNLOAD_INITIAL_BACKOFF_MS * (1_u64 << attempt.saturating_sub(1)),
    )
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

async fn verify_sha256(path: &Path, expected: Option<&str>) -> io::Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };

    let bytes = fs::read(path).await?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected {
        return Err(io::Error::other(format!(
            "runtime artifact checksum mismatch for '{}': expected {} got {}",
            path.display(),
            expected,
            actual
        )));
    }
    Ok(())
}

async fn set_executable_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = fs::metadata(path).await?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).await?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{artifact_download_backoff, is_retryable_status};

    #[test]
    fn transient_artifact_download_statuses_are_retryable() {
        assert!(is_retryable_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(reqwest::StatusCode::REQUEST_TIMEOUT));
        assert!(!is_retryable_status(reqwest::StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(reqwest::StatusCode::FORBIDDEN));
    }

    #[test]
    fn artifact_download_backoff_is_exponential() {
        assert_eq!(
            artifact_download_backoff(1),
            std::time::Duration::from_millis(200)
        );
        assert_eq!(
            artifact_download_backoff(2),
            std::time::Duration::from_millis(400)
        );
        assert_eq!(
            artifact_download_backoff(3),
            std::time::Duration::from_millis(800)
        );
    }
}
