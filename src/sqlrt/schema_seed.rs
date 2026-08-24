use std::collections::HashMap;

use crate::discover::{Metadata, OutputMetadata, PipelineMetadata, SkipprDataType};

const OTEL_COLUMNS: &str = include_str!("../../examples/otel/otel_columns.txt");

pub fn otel_columns_text() -> &'static str {
    OTEL_COLUMNS
}

pub fn parse_otel_columns(text: &str) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current = Some(name.to_string());
            out.entry(name.to_string()).or_default();
            continue;
        }
        if let Some(section) = &current {
            out.entry(section.clone())
                .or_default()
                .push(line.to_string());
        }
    }
    out
}

fn field_type(name: &str) -> SkipprDataType {
    match name {
        "start_time_unix_nano"
        | "end_time_unix_nano"
        | "duration_nano"
        | "time_unix_nano"
        | "observed_time_unix_nano"
        | "dropped_attributes_count"
        | "dropped_events_count"
        | "dropped_links_count"
        | "severity_number"
        | "count"
        | "zero_count" => SkipprDataType::Long,
        "hour" | "scale" | "positive_offset" | "negative_offset" => SkipprDataType::Integer,
        "value" | "sum" => SkipprDataType::Double,
        "is_monotonic" => SkipprDataType::Boolean,
        "bucket_counts"
        | "explicit_bounds"
        | "positive_bucket_counts"
        | "negative_bucket_counts" => SkipprDataType::Array,
        _ => SkipprDataType::String,
    }
}

fn field_nullable(name: &str) -> bool {
    matches!(
        name,
        "parent_span_id"
            | "deployment_environment"
            | "http_route"
            | "trace_id"
            | "span_id"
            | "exemplar_trace_id"
            | "observed_time_unix_nano"
            | "severity_text"
            | "severity_number"
            | "scope_name"
            | "scope_version"
    )
}

pub fn metadata_for_columns(columns: &[String]) -> Metadata {
    let mut root = Metadata::new_with_type(SkipprDataType::Record, "");
    for name in columns {
        let mut field = Metadata::new_with_type(field_type(name), name);
        field.nullable = field_nullable(name);
        if name == "bucket_counts"
            || name == "positive_bucket_counts"
            || name == "negative_bucket_counts"
        {
            field.determined_type_values = Some(SkipprDataType::Long);
        }
        if name == "explicit_bounds" {
            field.determined_type_values = Some(SkipprDataType::Double);
        }
        root.set_field(name, field);
    }
    root
}

pub fn output_metadata_for_columns(columns: &[String]) -> OutputMetadata {
    OutputMetadata::from_metadata(&metadata_for_columns(columns))
}

pub fn otel_namespace_metadata() -> HashMap<String, Metadata> {
    parse_otel_columns(OTEL_COLUMNS)
        .into_iter()
        .map(|(ns, cols)| (ns, metadata_for_columns(&cols)))
        .collect()
}

pub fn namespaces_for_signals(signals: &[&str]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for signal in signals {
        match *signal {
            "traces" => out.extend(["spans", "span_events", "span_links"]),
            "logs" => out.push("log_records"),
            "metrics" => out.extend(["gauge", "sum", "histogram", "exponential_histogram"]),
            _ => {}
        }
    }
    out
}

/// Seed pipeline metadata when the source plugin is Otlp and namespaces are missing.
pub fn seed_otel_pipeline_metadata(
    existing: &PipelineMetadata,
    source_plugin: &str,
    signals: &[&str],
) -> Option<PipelineMetadata> {
    if !source_plugin.eq_ignore_ascii_case("Otlp") {
        return None;
    }
    let needed = namespaces_for_signals(signals);
    if needed.iter().all(|ns| existing.metadata.contains_key(*ns)) {
        return None;
    }
    let seeded = otel_namespace_metadata();
    let mut updated = existing.clone();
    for ns in needed {
        if !updated.metadata.contains_key(ns) {
            if let Some(meta) = seeded.get(ns) {
                updated.metadata.insert(ns.to_string(), meta.clone());
            }
        }
    }
    updated.enabled = true;
    Some(updated)
}

pub fn column_names(namespace: &str) -> Vec<String> {
    parse_otel_columns(OTEL_COLUMNS)
        .get(namespace)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_field_names_match_otel_columns_txt() {
        let parsed = parse_otel_columns(OTEL_COLUMNS);
        let seeded = otel_namespace_metadata();
        for (ns, cols) in parsed {
            let meta = seeded.get(&ns).expect(ns.as_str());
            for col in cols {
                assert!(meta.fields.contains_key(&col), "missing {ns}.{col}");
            }
        }
    }

    #[test]
    fn traces_namespaces() {
        assert_eq!(
            namespaces_for_signals(&["traces"]),
            ["spans", "span_events", "span_links"]
        );
    }

    #[test]
    fn metrics_include_exponential_histogram() {
        assert!(namespaces_for_signals(&["metrics"]).contains(&"exponential_histogram"));
    }

    #[test]
    fn examples_otel_skippr_yml_parses() {
        let text = include_str!("../../examples/otel/skippr.yml");
        let cfg: crate::helpers::configuration::Config =
            serde_yaml::from_str(text).expect("examples/otel/skippr.yml");
        assert!(cfg.pipelines.contains_key("otel-traces"));
        assert!(cfg.pipelines.contains_key("otel-logs"));
        assert!(cfg.pipelines.contains_key("otel-metrics"));
        assert!(cfg.pipelines.contains_key("otel-metrics-1m"));
        assert!(cfg.pipelines.contains_key("otel-metrics-5m"));
    }

    #[test]
    fn host_cargo_does_not_depend_on_otlp_plugin() {
        let cargo = include_str!("../../Cargo.toml");
        assert!(
            !cargo.contains("skippr-plugin-data-source-otlp"),
            "host must not depend on the Otlp plugin crate"
        );
    }

    #[test]
    fn bronze_seed_names_include_exemplar_and_hour() {
        let spans = column_names("spans");
        for col in ["hour", "tenant_id", "http_route"] {
            assert!(spans.iter().any(|c| c == col), "missing spans.{col}");
        }
        let sum = column_names("sum");
        assert!(sum.iter().any(|c| c == "exemplar_trace_id"));
        let exp = column_names("exponential_histogram");
        assert!(exp.iter().any(|c| c == "scale"));
    }
}
