use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    #[default]
    Target,
    Competitor,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::Competitor => "competitor",
        }
    }
}
