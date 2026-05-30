use serde_json::{json, Map, Value};

use crate::config::BacklinkJob;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Primary,
    Competitor,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Competitor => "competitor",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncEntity {
    pub site: String,
    pub target: String,
    pub entity_kind: EntityKind,
    pub competitor_name: Option<String>,
    pub backlink_jobs: Vec<BacklinkJob>,
}

impl SyncEntity {
    pub fn checkpoint_key(&self) -> String {
        match &self.competitor_name {
            Some(name) => format!("competitor:{name}:{}", self.target),
            None => format!("primary:{}", self.target),
        }
    }

    pub fn parse_context<'a>(&'a self, run_date: &'a str) -> EntityParseContext<'a> {
        EntityParseContext {
            site: &self.site,
            target: &self.target,
            entity_kind: self.entity_kind.as_str(),
            competitor_name: self.competitor_name.as_deref(),
            run_date,
        }
    }
}

pub struct EntityParseContext<'a> {
    pub site: &'a str,
    pub target: &'a str,
    pub entity_kind: &'a str,
    pub competitor_name: Option<&'a str>,
    pub run_date: &'a str,
}

impl EntityParseContext<'_> {
    pub fn apply_envelope(&self, row: &mut Map<String, Value>) {
        row.insert("site".into(), json!(self.site));
        row.insert("run_date".into(), json!(self.run_date));
        row.insert("target".into(), json!(self.target));
        row.insert("entity_kind".into(), json!(self.entity_kind));
        row.insert(
            "competitor_name".into(),
            self.competitor_name
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
        );
    }
}
