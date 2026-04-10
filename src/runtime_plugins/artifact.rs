use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::runtime_plugins::manifest::{RuntimePluginArtifact, RuntimePluginManifest};

pub async fn resolve_plugin_executable(
    manifest_path: &Path,
    manifest: &RuntimePluginManifest,
) -> io::Result<PathBuf> {
    if let Some(artifact) = manifest.artifact_for_current_target() {
        let executable = resolve_artifact(manifest_path, manifest, artifact).await?;
        verify_sha256(&executable, artifact.sha256.as_deref()).await?;
        return Ok(executable);
    }

    manifest.resolve_executable(manifest_path)
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
                return Ok(destination);
            }
        }
        download_artifact(url, &destination).await?;
        verify_sha256(&destination, artifact.sha256.as_deref()).await?;
        return Ok(destination);
    }

    manifest.resolve_executable_path(manifest_path, &artifact.executable)
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

    let response = reqwest::get(url)
        .await
        .map_err(|err| io::Error::other(format!("runtime artifact download failed: {}", err)))?;
    if !response.status().is_success() {
        return Err(io::Error::other(format!(
            "runtime artifact download returned status {} for {}",
            response.status(),
            url
        )));
    }

    let bytes = response.bytes().await.map_err(|err| {
        io::Error::other(format!("runtime artifact download read failed: {}", err))
    })?;

    let mut file = fs::File::create(destination).await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    set_executable_permissions(destination).await?;
    Ok(())
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
