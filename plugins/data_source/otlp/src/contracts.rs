use skippr_runtime_sdk::plugins::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};

use crate::arrow_batch::{
    NS_EXP_HISTOGRAM, NS_GAUGE, NS_HISTOGRAM, NS_LOG_RECORDS, NS_SPANS, NS_SPAN_EVENTS,
    NS_SPAN_LINKS, NS_SUM,
};
use crate::config::{OtlpConfig, OtlpSignal};

fn partition_key() -> Vec<FieldPath> {
    vec![
        FieldPath::single("hour"),
        FieldPath::single("service_name"),
        FieldPath::single("tenant_id"),
    ]
}

fn append_contract(
    namespace: &str,
    primary_key: Vec<FieldPath>,
    description: &str,
) -> SourceNamespaceContract {
    SourceNamespaceContract {
        namespace: namespace.to_string(),
        primary_key,
        cursor: None,
        partition_key: partition_key(),
        write_policy: WritePolicy::Append,
        refresh_window: None,
        description: description.to_string(),
        semantics: Some(SourceSemantics::EventStream),
    }
}

pub fn namespace_contracts(config: &OtlpConfig) -> Vec<SourceNamespaceContract> {
    let mut out = Vec::new();
    for signal in &config.signals {
        match signal {
            OtlpSignal::Traces => {
                out.push(append_contract(
                    NS_SPANS,
                    vec![FieldPath::single("trace_id"), FieldPath::single("span_id")],
                    "OTLP spans",
                ));
                out.push(append_contract(
                    NS_SPAN_EVENTS,
                    vec![
                        FieldPath::single("trace_id"),
                        FieldPath::single("span_id"),
                        FieldPath::single("time_unix_nano"),
                        FieldPath::single("name"),
                    ],
                    "OTLP span events",
                ));
                out.push(append_contract(
                    NS_SPAN_LINKS,
                    vec![
                        FieldPath::single("trace_id"),
                        FieldPath::single("span_id"),
                        FieldPath::single("linked_trace_id"),
                        FieldPath::single("linked_span_id"),
                    ],
                    "OTLP span links",
                ));
            }
            OtlpSignal::Logs => {
                out.push(append_contract(
                    NS_LOG_RECORDS,
                    vec![
                        FieldPath::single("time_unix_nano"),
                        FieldPath::single("service_name"),
                        FieldPath::single("body"),
                    ],
                    "OTLP log records",
                ));
            }
            OtlpSignal::Metrics => {
                out.push(append_contract(
                    NS_GAUGE,
                    vec![
                        FieldPath::single("metric_name"),
                        FieldPath::single("time_unix_nano"),
                        FieldPath::single("service_name"),
                    ],
                    "OTLP gauge",
                ));
                out.push(append_contract(
                    NS_SUM,
                    vec![
                        FieldPath::single("metric_name"),
                        FieldPath::single("time_unix_nano"),
                        FieldPath::single("service_name"),
                    ],
                    "OTLP sum",
                ));
                out.push(append_contract(
                    NS_HISTOGRAM,
                    vec![
                        FieldPath::single("metric_name"),
                        FieldPath::single("time_unix_nano"),
                        FieldPath::single("service_name"),
                    ],
                    "OTLP histogram",
                ));
                out.push(append_contract(
                    NS_EXP_HISTOGRAM,
                    vec![
                        FieldPath::single("metric_name"),
                        FieldPath::single("time_unix_nano"),
                        FieldPath::single("service_name"),
                    ],
                    "OTLP exponential histogram",
                ));
            }
        }
    }
    for contract in &out {
        contract
            .validate()
            .expect("invalid OTLP namespace contract configuration");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traces_emits_three_namespaces() {
        let cfg: crate::config::OtlpConfig =
            serde_json::from_value(serde_json::json!({ "signals": ["traces"] })).unwrap();
        let contracts = namespace_contracts(&cfg);
        let names: Vec<_> = contracts.iter().map(|c| c.namespace.as_str()).collect();
        assert_eq!(names, ["spans", "span_events", "span_links"]);
        for c in &contracts {
            c.validate().unwrap();
            assert_eq!(c.write_policy, WritePolicy::Append);
        }
    }

    #[test]
    fn logs_only_does_not_emit_spans() {
        let cfg: crate::config::OtlpConfig =
            serde_json::from_value(serde_json::json!({ "signals": ["logs"] })).unwrap();
        let contracts = namespace_contracts(&cfg);
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].namespace, "log_records");
    }
}
