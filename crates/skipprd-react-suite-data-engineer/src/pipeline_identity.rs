use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PipelineName(String);

impl PipelineName {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, String> {
        let trimmed = value.as_ref().trim();
        if trimmed.is_empty() {
            return Err("pipeline name cannot be empty".to_string());
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for PipelineName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::ops::Deref for PipelineName {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for PipelineName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<&str> for PipelineName {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl FromStr for PipelineName {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[cfg(test)]
mod tests {
    use super::PipelineName;

    #[test]
    fn rejects_empty_pipeline_names() {
        assert!(PipelineName::parse("").is_err());
        assert!(PipelineName::parse("   ").is_err());
    }

    #[test]
    fn trims_valid_pipeline_names() {
        let name = PipelineName::parse("  bike_hire  ").expect("valid pipeline");
        assert_eq!(name.as_str(), "bike_hire");
    }
}
