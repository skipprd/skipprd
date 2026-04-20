use once_cell::sync::Lazy;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct RuntimePluginTarget {
    pub triple: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub publish_artifact_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RuntimePluginTargetMatrix {
    targets: Vec<RuntimePluginTarget>,
}

static TARGETS: Lazy<Vec<RuntimePluginTarget>> = Lazy::new(|| {
    let payload = include_str!("../../runtime_plugins/targets.json");
    let matrix: RuntimePluginTargetMatrix =
        serde_json::from_str(payload).expect("runtime plugin target matrix must be valid JSON");
    matrix.targets
});

pub fn current_target_triple() -> &'static str {
    env!("SKIPPR_BUILD_TARGET_TRIPLE")
}

pub fn target_for_triple(triple: &str) -> Option<&'static RuntimePluginTarget> {
    TARGETS.iter().find(|target| target.triple == triple)
}

pub fn manifest_key_candidates_for_triple(triple: &str) -> Vec<String> {
    match target_for_triple(triple) {
        Some(target) => std::iter::once(target.triple.clone())
            .chain(target.aliases.iter().cloned())
            .collect(),
        None => vec![triple.to_string()],
    }
}

pub fn manifest_key_candidates_for_current_target() -> Vec<String> {
    manifest_key_candidates_for_triple(current_target_triple())
}

#[cfg(test)]
mod tests {
    use super::{
        current_target_triple, manifest_key_candidates_for_current_target,
        manifest_key_candidates_for_triple,
    };

    #[test]
    fn target_matrix_maps_macos_to_legacy_darwin_alias() {
        let keys = manifest_key_candidates_for_triple("aarch64-apple-darwin");
        assert_eq!(keys[0], "aarch64-apple-darwin");
        assert!(keys.iter().any(|key| key == "darwin-aarch64"));
    }

    #[test]
    fn current_target_candidates_include_build_target_first() {
        let keys = manifest_key_candidates_for_current_target();
        assert_eq!(keys[0], current_target_triple());
    }
}
