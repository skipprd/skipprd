use crate::failure_kind::FailureKind;

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

pub fn normalize_text(s: &str) -> String {
    s.to_ascii_lowercase()
}

pub fn normalize_errors(errors: &[String]) -> String {
    normalize_text(&errors.join("\n"))
}

pub fn is_infra_transient(s: &str) -> bool {
    contains_any(
        s,
        &[
            // Network / HTTP
            "timeout",
            "timed out",
            "temporar",
            "temporarily",
            "http 502",
            "http 503",
            "http 504",
            "bad gateway",
            "gateway timeout",
            "service unavailable",
            "connection reset",
            "connection aborted",
            "connection refused",
            "network error",
            "econnrefused",
            "econnreset",
            "etimedout",
            // AWS / cloud provider
            "service error",
            "internal error",
            "internal server error",
            "internalserverexception",
            "serviceexception",
            "throttlingexception",
            "toomanyrequestsexception",
            "rate exceeded",
            "slow down",
            "request limit",
            "provisioned throughput",
        ],
    )
}

pub fn is_infra_config(s: &str) -> bool {
    contains_any(
        s,
        &[
            // Missing credential/key files
            "no such file or directory",
            "file not found",
            "cannot find the file",
            "cannot find the path",
            // Broken profiles.yml / YAML-level config parse failures
            "did not find expected ',' or '}'",
            "profiles.yml",
            "could not parse",
            // Auth / permission failures (not transient)
            "authentication failed",
            "private_key_path",
            "invalid private key",
            "permission denied",
            "access denied",
            "not authorized",
            "invalid credentials",
            // Missing env vars
            "env_var not found",
        ],
    )
}

pub fn classify_dbt_failure(errors: &[String]) -> FailureKind {
    if errors.is_empty() {
        return FailureKind::Unknown;
    }
    let s = normalize_errors(errors);
    if is_infra_transient(&s) {
        FailureKind::InfraTransient
    } else if is_infra_config(&s) {
        FailureKind::InfraConfig
    } else {
        FailureKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_dbt_failure_non_infra_is_unknown() {
        let errors = vec!["Runtime Error: syntax error at or near FROM".to_string()];
        assert_eq!(classify_dbt_failure(&errors), FailureKind::Unknown);
    }

    #[test]
    fn classify_dbt_failure_missing_source_is_unknown() {
        let errors = vec![
            "Compilation Error: depends on a source named 'x.y' which was not found".to_string(),
        ];
        assert_eq!(classify_dbt_failure(&errors), FailureKind::Unknown);
    }

    #[test]
    fn classify_dbt_failure_schema_error_is_unknown() {
        let errors = vec!["Compilation Error: schema.yml parse failure".to_string()];
        assert_eq!(classify_dbt_failure(&errors), FailureKind::Unknown);
    }

    #[test]
    fn classify_dbt_failure_warehouse_auth_is_config() {
        let errors = vec!["accessdenied: user is not authorized".to_string()];
        assert_eq!(classify_dbt_failure(&errors), FailureKind::InfraConfig);
    }

    #[test]
    fn classify_missing_key_file_is_config() {
        let errors = vec!["[Errno 2] No such file or directory: 'snowflake_key.p8'".to_string()];
        assert_eq!(classify_dbt_failure(&errors), FailureKind::InfraConfig);
    }

    #[test]
    fn classify_profiles_yml_parse_is_config() {
        let errors = vec![
            "dbt failed before compilation/build: invalid YAML/Jinja in profiles.yml".to_string(),
        ];
        assert_eq!(classify_dbt_failure(&errors), FailureKind::InfraConfig);
    }

    #[test]
    fn classify_yaml_flow_mapping_error_is_config() {
        let errors = vec!["did not find expected ',' or '}'".to_string()];
        assert_eq!(classify_dbt_failure(&errors), FailureKind::InfraConfig);
    }

    #[test]
    fn classify_dbt_failure_service_error_is_transient() {
        let errors = vec!["sql validation failed: service error".to_string()];
        assert_eq!(classify_dbt_failure(&errors), FailureKind::InfraTransient);
    }

    #[test]
    fn classify_dbt_failure_throttling_is_transient() {
        let errors = vec!["ThrottlingException: rate exceeded".to_string()];
        assert_eq!(classify_dbt_failure(&errors), FailureKind::InfraTransient);
    }

    #[test]
    fn classify_dbt_failure_internal_server_is_transient() {
        let errors = vec!["InternalServerException: An internal error occurred".to_string()];
        assert_eq!(classify_dbt_failure(&errors), FailureKind::InfraTransient);
    }

    #[test]
    fn is_infra_transient_covers_aws_patterns() {
        assert!(is_infra_transient("service error"));
        assert!(is_infra_transient("internal server error"));
        assert!(is_infra_transient("internalserverexception"));
        assert!(is_infra_transient("serviceexception: something went wrong"));
        assert!(is_infra_transient("throttlingexception: rate exceeded"));
        assert!(is_infra_transient("toomanyrequestsexception"));
        assert!(is_infra_transient("slow down"));
        assert!(is_infra_transient("request limit exceeded"));
        assert!(is_infra_transient("timed out waiting for response"));
        assert!(is_infra_transient("bad gateway"));
        assert!(is_infra_transient("gateway timeout"));
        assert!(is_infra_transient("service unavailable"));
    }

    #[test]
    fn is_infra_transient_does_not_match_sql_errors() {
        assert!(!is_infra_transient("syntax error at or near select"));
        assert!(!is_infra_transient("compilation error in model"));
    }

    #[test]
    fn classify_dbt_failure_empty_is_unknown() {
        assert_eq!(classify_dbt_failure(&[]), FailureKind::Unknown);
    }

    #[test]
    fn is_infra_config_covers_common_patterns() {
        assert!(is_infra_config(
            "no such file or directory: 'snowflake_key.p8'"
        ));
        assert!(is_infra_config("authentication failed for user 'test'"));
        assert!(is_infra_config("invalid private key format"));
        assert!(is_infra_config("dbt failed: profiles.yml syntax error"));
        assert!(is_infra_config("did not find expected ',' or '}'"));
        assert!(is_infra_config("permission denied accessing resource"));
        assert!(is_infra_config("access denied for this operation"));
    }

    #[test]
    fn is_infra_config_does_not_match_sql_errors() {
        assert!(!is_infra_config("syntax error at or near select"));
        assert!(!is_infra_config("compilation error in model"));
        assert!(!is_infra_config("ambiguous column reference"));
    }

    #[test]
    fn old_variants_deserialize_as_unknown() {
        let old = r#""missing_source""#;
        let kind: FailureKind = serde_json::from_str(old).unwrap();
        assert_eq!(kind, FailureKind::Unknown);

        let old = r#""no_failure""#;
        let kind: FailureKind = serde_json::from_str(old).unwrap();
        assert_eq!(kind, FailureKind::Unknown);
    }
}
