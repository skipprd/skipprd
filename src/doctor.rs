//! Engine preflight: config, env refs, sources, optional sinks, WAL.

use serde::Serialize;

use crate::helpers::configuration::{Config, Pipeline};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DoctorSeverity {
    Info,
    Error,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DoctorCheck {
    pub ok: bool,
    pub severity: DoctorSeverity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_fix_command: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DoctorResult {
    pub ok: bool,
    pub config_path: String,
    pub checks: Vec<DoctorCheck>,
}

pub fn run(config: &Config) -> DoctorResult {
    let mut checks = Vec::new();

    if config.pipelines.is_empty() {
        checks.push(fail("no pipelines configured"));
    } else {
        checks.push(pass(&format!(
            "{} pipeline(s) configured",
            config.pipelines.len()
        )));
    }

    if config
        .data_sources
        .as_ref()
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        checks.push(fail("data_sources is empty"));
    } else {
        checks.push(pass("data_sources configured"));
    }

    let sinks = config.data_sinks.as_ref();
    if sinks.map(|s| !s.is_empty()).unwrap_or(false) {
        checks.push(pass("data_sinks configured (optional)"));
    } else {
        checks.push(info("no data_sink; WAL is the dataset"));
    }

    for (name, pipeline) in &config.pipelines {
        match Config::validate_pipeline_registry_refs_for(config, name, pipeline) {
            Ok(()) => {
                let sink = if pipeline.data_sink.is_some() {
                    "source+sink"
                } else {
                    "source, WAL-only"
                };
                checks.push(pass(&format!("pipeline '{name}' ({sink})")));
            }
            Err(err) => checks.push(fail(&err)),
        }
        wal_checks(&mut checks, config, pipeline);
    }

    let ok = checks
        .iter()
        .all(|c| c.ok || c.severity == DoctorSeverity::Info);
    DoctorResult {
        ok,
        config_path: Config::find_config_file(),
        checks,
    }
}

fn wal_checks(checks: &mut Vec<DoctorCheck>, config: &Config, _pipeline: &Pipeline) {
    let mode = config
        .skippr
        .as_ref()
        .and_then(|s| s.skipprd_el_storage_mode.as_deref())
        .unwrap_or("s3");
    checks.push(pass(&format!("skipprd_el_storage_mode={mode}")));
}

fn pass(message: &str) -> DoctorCheck {
    DoctorCheck {
        ok: true,
        severity: DoctorSeverity::Info,
        message: message.to_string(),
        suggested_fix_command: None,
    }
}

fn info(message: &str) -> DoctorCheck {
    DoctorCheck {
        ok: true,
        severity: DoctorSeverity::Info,
        message: message.to_string(),
        suggested_fix_command: None,
    }
}

fn fail(message: &str) -> DoctorCheck {
    DoctorCheck {
        ok: false,
        severity: DoctorSeverity::Error,
        message: message.to_string(),
        suggested_fix_command: None,
    }
}

impl DoctorResult {
    pub fn print_text(&self) {
        for c in &self.checks {
            if c.ok {
                println!("  [ok]   {}", c.message);
            } else {
                println!("  [FAIL] {}", c.message);
            }
        }
        if self.ok {
            println!("\nAll checks passed.");
        } else {
            println!("\nSome checks failed.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::helpers::configuration::Config;
    use serde_json::json;

    #[test]
    fn doctor_accepts_wal_only_pipeline() {
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "quickstart", "skipprd_el_storage_mode": "local" },
            "pipelines": {
                "bikehire": { "data_source": "data_sources.sample" }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        let result = run(&config);
        assert!(result.ok, "{:?}", result.checks);
    }

    #[test]
    fn doctor_fails_without_source() {
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "quickstart" },
            "pipelines": {
                "bikehire": {}
            }
        }))
        .unwrap();
        let result = run(&config);
        assert!(!result.ok);
    }
}
