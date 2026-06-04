use std::collections::HashSet;

use serde::Deserialize;

pub const NAMESPACE_EVENT_FACT: &str = "hubspot_event_fact";
pub const NAMESPACE_PORTAL_SNAPSHOT: &str = "hubspot_portal_snapshot";
pub const NAMESPACE_PIPELINE_STAGE_DIM: &str = "hubspot_pipeline_stage_dim";
pub const NAMESPACE_DEAL_SNAPSHOT: &str = "hubspot_deal_snapshot";
pub const NAMESPACE_COMPANY_SNAPSHOT: &str = "hubspot_company_snapshot";
pub const NAMESPACE_FORM_DIM: &str = "hubspot_form_dim";
pub const NAMESPACE_LANDING_PAGE_DIM: &str = "hubspot_landing_page_dim";
pub const NAMESPACE_CTA_DIM: &str = "hubspot_cta_dim";
pub const NAMESPACE_SYNC_RUN_DAILY: &str = "hubspot_sync_run_daily";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HubspotStream {
    Crm,
    Marketing,
    Service,
    Onsite,
}

impl HubspotStream {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "crm" => Some(Self::Crm),
            "marketing" => Some(Self::Marketing),
            "service" => Some(Self::Service),
            "onsite" => Some(Self::Onsite),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Crm => "crm",
            Self::Marketing => "marketing",
            Self::Service => "service",
            Self::Onsite => "onsite",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    Minimal,
    #[default]
    #[serde(alias = "console_default")]
    ConsoleDefault,
    Full,
}

const MINIMAL_STREAMS: &[&str] = &["crm"];
const CONSOLE_DEFAULT_STREAMS: &[&str] = &["crm", "marketing", "service"];
const FULL_STREAMS: &[&str] = &["crm", "marketing", "service", "onsite"];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<HubspotStream> {
    let names = match profile {
        StreamProfile::Minimal => MINIMAL_STREAMS,
        StreamProfile::ConsoleDefault => CONSOLE_DEFAULT_STREAMS,
        StreamProfile::Full => FULL_STREAMS,
    };
    names
        .iter()
        .filter_map(|name| HubspotStream::from_name(name))
        .collect()
}

pub fn resolve_streams(
    profile: StreamProfile,
    explicit: Option<Vec<String>>,
) -> Vec<HubspotStream> {
    let selected: HashSet<String> = explicit.unwrap_or_default().into_iter().collect();
    if selected.is_empty() {
        return streams_for_profile(profile);
    }
    selected
        .iter()
        .filter_map(|name| HubspotStream::from_name(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_default_includes_crm_marketing_service() {
        let streams = streams_for_profile(StreamProfile::ConsoleDefault);
        assert!(streams.contains(&HubspotStream::Crm));
        assert!(streams.contains(&HubspotStream::Marketing));
        assert!(streams.contains(&HubspotStream::Service));
        assert!(!streams.contains(&HubspotStream::Onsite));
    }
}
