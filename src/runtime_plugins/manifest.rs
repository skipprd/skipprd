use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::runtime_plugins::protocol::{
    HandshakeResponse, RuntimePluginKind, RuntimeSinkCapabilityDescriptor,
    RuntimeSourceCapabilityDescriptor, RUNTIME_PROTOCOL_VERSION,
};
use crate::runtime_plugins::targets::{
    current_target_triple, manifest_key_candidates_for_current_target,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimePluginArtifact {
    pub executable: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimePluginManifest {
    pub name: String,
    pub kind: RuntimePluginKind,
    pub plugin_name: String,
    #[serde(default = "default_plugin_version")]
    pub version: String,
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u32,
    #[serde(default)]
    pub sdk_build_fingerprint: Option<String>,
    #[serde(default = "default_config_schema_version")]
    pub config_schema_version: u32,
    #[serde(default)]
    pub install_root: Option<String>,
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub artifacts: HashMap<String, RuntimePluginArtifact>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub supports_schema: bool,
    #[serde(default)]
    pub source_capability: Option<RuntimeSourceCapabilityDescriptor>,
    #[serde(default)]
    pub sink_capability: Option<RuntimeSinkCapabilityDescriptor>,
}

fn default_protocol_version() -> u32 {
    RUNTIME_PROTOCOL_VERSION
}

fn default_plugin_version() -> String {
    "0.0.0-dev".to_string()
}

fn default_config_schema_version() -> u32 {
    1
}

impl RuntimePluginManifest {
    pub fn load_from_path(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        let manifest: Self =
            serde_json::from_slice(&bytes).map_err(|err| io::Error::other(err.to_string()))?;
        Ok(manifest)
    }

    pub fn current_target() -> String {
        current_target_triple().to_string()
    }

    pub fn artifact_for_current_target(&self) -> Option<&RuntimePluginArtifact> {
        for key in manifest_key_candidates_for_current_target() {
            if let Some(artifact) = self.artifacts.get(&key) {
                return Some(artifact);
            }
        }
        None
    }

    pub fn resolve_executable_path(
        &self,
        manifest_path: &Path,
        executable: &str,
    ) -> io::Result<PathBuf> {
        let binary_dir = std::env::current_exe()?
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| io::Error::other("current executable has no parent directory"))?;

        let resolved = executable.replace("${binary_dir}", &binary_dir.to_string_lossy());
        let candidate = PathBuf::from(resolved);
        if candidate.is_absolute() {
            return Ok(candidate);
        }

        let manifest_dir = manifest_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| io::Error::other("manifest path has no parent directory"))?;
        Ok(manifest_dir.join(candidate))
    }

    pub fn resolve_executable(&self, manifest_path: &Path) -> io::Result<PathBuf> {
        let executable = self
            .executable
            .as_deref()
            .ok_or_else(|| io::Error::other("runtime plugin manifest is missing an executable"))?;
        self.resolve_executable_path(manifest_path, executable)
    }

    pub fn verify_handshake(&self, handshake: &HandshakeResponse) -> io::Result<()> {
        if handshake.protocol_version != self.protocol_version {
            return Err(io::Error::other(format!(
                "runtime plugin '{}' reported protocol version {} but manifest expects {}",
                self.name, handshake.protocol_version, self.protocol_version
            )));
        }

        if handshake.kind != self.kind {
            return Err(io::Error::other(format!(
                "runtime plugin '{}' reported kind {:?} but manifest expects {:?}",
                self.name, handshake.kind, self.kind
            )));
        }

        if handshake.plugin_name != self.plugin_name {
            return Err(io::Error::other(format!(
                "runtime plugin '{}' reported plugin name '{}' but manifest expects '{}'",
                self.name, handshake.plugin_name, self.plugin_name
            )));
        }

        if self.supports_schema && !handshake.supports_schema {
            return Err(io::Error::other(format!(
                "runtime plugin '{}' manifest requires schema support but handshake does not",
                self.name
            )));
        }

        match (&self.source_capability, &handshake.source_capability) {
            (Some(expected), Some(actual)) if expected != actual => {
                return Err(io::Error::other(format!(
                    "runtime plugin '{}' source capability mismatch: manifest={:?} handshake={:?}",
                    self.name, expected, actual
                )));
            }
            (Some(_), None) => {
                return Err(io::Error::other(format!(
                    "runtime plugin '{}' manifest declared a source capability but handshake omitted it",
                    self.name
                )));
            }
            _ => {}
        }

        match (&self.sink_capability, &handshake.sink_capability) {
            (Some(expected), Some(actual)) if expected != actual => {
                return Err(io::Error::other(format!(
                    "runtime plugin '{}' sink capability mismatch: manifest={:?} handshake={:?}",
                    self.name, expected, actual
                )));
            }
            (Some(_), None) => {
                return Err(io::Error::other(format!(
                    "runtime plugin '{}' manifest declared a sink capability but handshake omitted it",
                    self.name
                )));
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{RuntimePluginArtifact, RuntimePluginKind, RuntimePluginManifest};
    use crate::runtime_plugins::protocol::RUNTIME_PROTOCOL_VERSION;

    fn sample_manifest() -> RuntimePluginManifest {
        RuntimePluginManifest {
            name: "athena-runtime-sink".to_string(),
            kind: RuntimePluginKind::DataSink,
            plugin_name: "Athena".to_string(),
            version: "0.1.1".to_string(),
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            sdk_build_fingerprint: None,
            config_schema_version: 1,
            install_root: None,
            executable: Some("skippr-plugin-data-sink-athena".to_string()),
            artifacts: HashMap::from([(
                "darwin-aarch64".to_string(),
                RuntimePluginArtifact {
                    executable: "skippr-plugin-data-sink-athena".to_string(),
                    url: Some("https://example.invalid/athena".to_string()),
                    sha256: None,
                },
            )]),
            args: Vec::new(),
            supports_schema: false,
            source_capability: None,
            sink_capability: None,
        }
    }

    #[test]
    fn current_target_artifact_supports_legacy_aliases() {
        if RuntimePluginManifest::current_target() != "aarch64-apple-darwin" {
            return;
        }

        let manifest = sample_manifest();
        let artifact = manifest
            .artifact_for_current_target()
            .expect("legacy darwin alias should resolve on macOS arm64");
        assert_eq!(artifact.executable, "skippr-plugin-data-sink-athena");
    }

    #[test]
    fn legacy_manifest_without_sdk_build_fingerprint_remains_readable() {
        let manifest: RuntimePluginManifest = serde_json::from_value(serde_json::json!({
            "name": "athena-runtime-sink",
            "kind": "DataSink",
            "plugin_name": "Athena",
            "version": "0.1.9",
            "protocol_version": RUNTIME_PROTOCOL_VERSION
        }))
        .expect("legacy runtime plugin manifest should deserialize");

        assert_eq!(manifest.sdk_build_fingerprint, None);
    }
}
