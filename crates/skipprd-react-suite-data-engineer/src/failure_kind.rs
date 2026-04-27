use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    InfraTransient,
    InfraConfig,
    #[serde(other)]
    Unknown,
}

impl Default for FailureKind {
    fn default() -> Self {
        Self::Unknown
    }
}

impl FailureKind {
    pub fn is_transient(self) -> bool {
        matches!(self, Self::InfraTransient)
    }

    /// Environment/configuration errors the LLM cannot repair by rewriting models.
    pub fn is_config(self) -> bool {
        matches!(self, Self::InfraConfig)
    }

    /// True only for errors that could plausibly be fixed by re-authoring dbt models.
    pub fn is_repairable(self) -> bool {
        matches!(self, Self::Unknown)
    }
}
