use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};

pub const NAMESPACE_CHECK_DAILY: &str = "site_security.check_daily";
pub const NAMESPACE_PAGE_SCAN_DAILY: &str = "site_security.page_scan_daily";
pub const NAMESPACE_STORAGE_ENTRY: &str = "site_security.storage_entry";
pub const NAMESPACE_COOKIE_ENTRY: &str = "site_security.cookie_entry";
pub const NAMESPACE_THIRD_PARTY_SCRIPT: &str = "site_security.third_party_script";
pub const NAMESPACE_SITE_RUN_DAILY: &str = "site_security.site_run_daily";
pub const NAMESPACE_TLS_DAILY: &str = "site_security.tls_daily";

pub const ALL_NAMESPACES: &[&str] = &[
    NAMESPACE_SITE_RUN_DAILY,
    NAMESPACE_TLS_DAILY,
    NAMESPACE_PAGE_SCAN_DAILY,
    NAMESPACE_STORAGE_ENTRY,
    NAMESPACE_COOKIE_ENTRY,
    NAMESPACE_THIRD_PARTY_SCRIPT,
    NAMESPACE_CHECK_DAILY,
];

pub fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
    match namespace {
        NAMESPACE_SITE_RUN_DAILY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![FieldPath::single("site"), FieldPath::single("run_date")],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Site Security daily run summary".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_TLS_DAILY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![FieldPath::single("site"), FieldPath::single("run_date")],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Site Security origin TLS and security.txt probe".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_PAGE_SCAN_DAILY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("site"),
                FieldPath::single("page_url"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Site Security per-page scan summary".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_STORAGE_ENTRY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("site"),
                FieldPath::single("page_url"),
                FieldPath::single("storage_kind"),
                FieldPath::single("entry_name"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Cookie and web storage keys with PII heuristics (values not stored)"
                .into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_COOKIE_ENTRY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("site"),
                FieldPath::single("page_url"),
                FieldPath::single("entry_name"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Cookie attributes from Playwright jar (values not stored)".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_THIRD_PARTY_SCRIPT => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("site"),
                FieldPath::single("page_url"),
                FieldPath::single("script_url"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Third-party and first-party script inventory".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_CHECK_DAILY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("site"),
                FieldPath::single("page_url"),
                FieldPath::single("issue_code"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Site Security scorecard checks".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        other => panic!("unknown site_security namespace: {other}"),
    }
}
