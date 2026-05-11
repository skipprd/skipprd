//! File ownership: the single source of truth for who-may-author-what in the dbt project tree.
//!
//! The data-engineer suite touches several classes of file under the scoped dbt project. Each
//! file falls into exactly one ownership class. Every site that needs to enforce or describe
//! ownership (the file-tool deny-list, the `dbt_project.yml` sanitizer, the author/repair prompt
//! "stripped-content" section, etc.) consults this module — never duplicates the table.
//!
//! # Ownership classes
//!
//! | Class            | Files                                                                | Edit rights                           | Rationale                                                                                            |
//! | ---------------- | -------------------------------------------------------------------- | ------------------------------------- | ---------------------------------------------------------------------------------------------------- |
//! | `SkipprOwned`    | `profiles.yml`, `packages.yml`                                       | LLM cannot write                      | System-scoped, authoritative, immutable to the agent. Connection identity and dependency governance. |
//! | `Shared`         | `dbt_project.yml`                                                    | LLM can write; specific top-level keys are stripped on every save | Most of the file is structural routing; agent value is limited to per-folder `models:` config.       |
//! | `LlmOwned`       | `models/**/*.sql`, `models/**/*.yml`, `models/schema.yml`, `seeds/*`, `macros/*.sql` | LLM has full write authority          | Modeling and reusable Jinja are the agent's primary surface.                                         |
//! | `BuildArtifact`  | `target/*`, `dbt_packages/*`, `.dbt/*`, `logs/*`                     | LLM cannot write; runtime manages     | Generated; never source-of-truth.                                                                    |
//!
//! # Strip-and-notify
//!
//! The `Shared` class is the interesting one. When the sanitizer rewrites `dbt_project.yml` it
//! iterates [`Ownership::Shared`]'s `skippr_top_level_keys` and removes any matches. Each removal
//! is reported back as a `StrippedArtifact` so the next author turn can re-author the intent in a
//! sanctioned location. The relocation guidance comes straight from this module's
//! `relocation_hint_for_keys` table — adding a new key means adding one entry here and nothing
//! else.
//!
//! # How to add a new file or key
//!
//! 1. If introducing a new file path, extend [`classify_rel_path`] with a branch that returns the
//!    appropriate [`Ownership`] variant. Prefer specific matches before fall-through.
//! 2. If extending the `Shared` policy for `dbt_project.yml`, add the top-level key name to
//!    [`DBT_PROJECT_SKIPPR_KEYS`] and (where a sanctioned alternative exists) add a tuple to
//!    [`DBT_PROJECT_RELOCATION_HINTS`].
//! 3. If introducing a new system-owned file, add it to [`SKIPPR_OWNED_FILES`] with its hint text
//!    and (if applicable) to [`classify_rel_path`].
//!
//! The hint strings in this module are LLM-visible. They MUST NOT reference internal
//! product/module names, Rust types, or implementation mechanisms — only dbt-native concepts and
//! the "system" abstraction. The agent has no awareness of skippr internals.

/// Classification of a file path within the scoped dbt project tree.
///
/// All variants carry the metadata required by callers; consumers should pattern-match on the
/// variant rather than relying on path strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// Fully system-owned. The agent cannot write here under any circumstances. The runtime
    /// regenerates these files (the regeneration mechanism is an implementation detail; the
    /// contract is "agent does not author this file").
    SkipprOwned {
        /// Human-readable rationale, surfaced in the file-tool deny-list error.
        reason: &'static str,
        /// `true` if the runtime regenerates this file every dbt invocation. Implementation hint
        /// only; never affects ownership semantics.
        regenerated_each_run: bool,
    },
    /// Co-owned: the agent may write the file, but specific top-level keys are stripped on every
    /// save and reported back via the strip-notify mechanism.
    Shared {
        /// Human-readable rationale, surfaced in code comments and tests.
        reason: &'static str,
        /// Top-level keys removed by the sanitizer.
        skippr_top_level_keys: &'static [&'static str],
        /// Per-key relocation guidance shown to the LLM after a strip.
        relocation_hint_for_keys: &'static [(&'static str, &'static str)],
    },
    /// Fully agent-owned. The LLM has unrestricted authority to author and patch.
    LlmOwned,
    /// Generated runtime artefact. Never source-of-truth; never authored by the agent.
    BuildArtifact,
}

/// Top-level `dbt_project.yml` keys the sanitizer always strips. Each entry has a matching hint
/// in [`DBT_PROJECT_RELOCATION_HINTS`] (or no hint, meaning "no sanctioned alternative").
const DBT_PROJECT_SKIPPR_KEYS: &[&str] = &[
    "name",
    "profile",
    "model-paths",
    "seed-paths",
    "macro-paths",
    "target-path",
    "on-run-start",
    "on-run-end",
    "dispatch",
    "depends_on",
    "vars",
    "query-comment",
    "require-dbt-version",
    "clean-targets",
];

/// Per-key relocation hints surfaced to the LLM after a strip. Keys MUST appear in
/// [`DBT_PROJECT_SKIPPR_KEYS`]. A key without an entry here is reported as "this key has no
/// sanctioned re-author location" by the prompt template.
const DBT_PROJECT_RELOCATION_HINTS: &[(&str, &str)] = &[
    (
        "name",
        "Project name is system-managed; the agent cannot re-author it.",
    ),
    (
        "profile",
        "Profile name is system-managed. Connection settings are system-managed and not authorable by the agent.",
    ),
    (
        "model-paths",
        "Path layout is system-fixed. Add new SQL under `models/<folder>/`; new seeds under `seeds/`; new macros under `macros/`.",
    ),
    (
        "seed-paths",
        "Path layout is system-fixed. Add new SQL under `models/<folder>/`; new seeds under `seeds/`; new macros under `macros/`.",
    ),
    (
        "macro-paths",
        "Path layout is system-fixed. Add new SQL under `models/<folder>/`; new seeds under `seeds/`; new macros under `macros/`.",
    ),
    (
        "target-path",
        "Path layout is system-fixed. Add new SQL under `models/<folder>/`; new seeds under `seeds/`; new macros under `macros/`.",
    ),
    (
        "on-run-start",
        "Project-wide hooks are system-owned. If you need setup SQL for a specific model, use `{{ config(pre_hook=[...]) }}` in that model's SQL. If you need a model-group hook, use a `_config.yml` block under `models/<folder>/`. Do not re-author at the project root.",
    ),
    (
        "on-run-end",
        "Project-wide hooks are system-owned. If you need teardown SQL for a specific model, use `{{ config(post_hook=[...]) }}` in that model's SQL. If you need a model-group hook, use a `_config.yml` block under `models/<folder>/`. Do not re-author at the project root.",
    ),
    (
        "dispatch",
        "For an adapter-specific macro override, name your macro `<name>__<adapter>.sql` under `macros/` (dbt picks this up via its built-in adapter dispatch). Do not declare a top-level `dispatch:` block.",
    ),
    (
        "depends_on",
        "`depends_on` is not a project-level concept. Express dependencies via `{{ ref('...') }}` inside the SQL, or `{{ config(depends_on=[ref('...')]) }}` in the calling model.",
    ),
    (
        "vars",
        "Pipeline-level vars are system-managed and not authorable by the agent. For per-model vars, use `{{ config(vars={...}) }}` inside the model SQL.",
    ),
    (
        "query-comment",
        "`query-comment` is system-managed. The agent cannot author it.",
    ),
    (
        "require-dbt-version",
        "dbt version is system-managed. Do not pin.",
    ),
    (
        "clean-targets",
        "Clean targets are system-managed. Do not author.",
    ),
];

/// Description of a fully system-owned file: the relative path (or path prefix), the rationale,
/// the hint shown in tool errors when the LLM attempts a write, and whether the runtime
/// regenerates this file each invocation.
struct SkipprOwnedFile {
    /// Either an exact relative path (e.g. `profiles.yml`) or a directory prefix ending in `/`
    /// (e.g. `target/`).
    path_or_prefix: &'static str,
    reason: &'static str,
    regenerated_each_run: bool,
}

const SKIPPR_OWNED_FILES: &[SkipprOwnedFile] = &[
    SkipprOwnedFile {
        path_or_prefix: "profiles.yml",
        reason: "`profiles.yml` is system-scoped, authoritative, and immutable. The agent cannot edit this file.",
        regenerated_each_run: true,
    },
    SkipprOwnedFile {
        path_or_prefix: "packages.yml",
        reason: "Package dependencies are governance-controlled. The agent cannot author this file.",
        regenerated_each_run: false,
    },
];

const BUILD_ARTIFACT_PREFIXES: &[&str] = &["target/", "dbt_packages/", ".dbt/", "logs/"];

const BUILD_ARTIFACT_REASON: &str =
    "Build artifact. Generated by dbt at runtime; never source-of-truth.";

/// Classify a relative project path into one of the four ownership classes.
///
/// Matching is path-string based; the first specific match wins and fall-through goes to
/// [`Ownership::LlmOwned`] (the dbt project tree is otherwise the agent's surface).
pub fn classify_rel_path(rel: &str) -> Ownership {
    let rel = rel.trim_start_matches('/');

    for file in SKIPPR_OWNED_FILES {
        if rel == file.path_or_prefix {
            return Ownership::SkipprOwned {
                reason: file.reason,
                regenerated_each_run: file.regenerated_each_run,
            };
        }
    }

    for prefix in BUILD_ARTIFACT_PREFIXES {
        if rel.starts_with(prefix) {
            return Ownership::BuildArtifact;
        }
    }

    if rel == "dbt_project.yml" {
        return Ownership::Shared {
            reason:
                "Most of dbt_project.yml is structural routing governed by the system; agent value is limited to per-folder `models:` config.",
            skippr_top_level_keys: DBT_PROJECT_SKIPPR_KEYS,
            relocation_hint_for_keys: DBT_PROJECT_RELOCATION_HINTS,
        };
    }

    Ownership::LlmOwned
}

/// `true` when the agent is allowed to write (`patch`/`write`/`mv`/`rm`) at this path. The
/// file-tool deny-list calls this to gate every mutating op.
///
/// `Shared` paths return `true` here (writes are allowed; the sanitizer applies on save). Only
/// `SkipprOwned` and `BuildArtifact` deny writes outright.
pub fn is_writable_by_llm(rel: &str) -> bool {
    match classify_rel_path(rel) {
        Ownership::SkipprOwned { .. } | Ownership::BuildArtifact => false,
        Ownership::Shared { .. } | Ownership::LlmOwned => true,
    }
}

/// Return the hint text shown to the LLM after a top-level key is stripped from a `Shared` file,
/// or `None` if no sanctioned alternative location exists.
pub fn relocation_hint(rel: &str, top_level_key: &str) -> Option<&'static str> {
    match classify_rel_path(rel) {
        Ownership::Shared {
            relocation_hint_for_keys,
            ..
        } => relocation_hint_for_keys
            .iter()
            .find(|(k, _)| *k == top_level_key)
            .map(|(_, v)| *v),
        _ => None,
    }
}

/// Hint text shown when the LLM attempts a write against a non-writable path.
///
/// For `SkipprOwned` paths this is the configured `reason`. For `BuildArtifact` paths a generic
/// rationale is returned. Returns `None` for paths the LLM is allowed to write.
pub fn deny_list_hint(rel: &str) -> Option<&'static str> {
    match classify_rel_path(rel) {
        Ownership::SkipprOwned { reason, .. } => Some(reason),
        Ownership::BuildArtifact => Some(BUILD_ARTIFACT_REASON),
        _ => None,
    }
}

/// Iterator helper: returns the top-level keys the sanitizer must strip from a `Shared` file, or
/// an empty slice for other ownership classes.
pub fn shared_skippr_top_level_keys(rel: &str) -> &'static [&'static str] {
    match classify_rel_path(rel) {
        Ownership::Shared {
            skippr_top_level_keys,
            ..
        } => skippr_top_level_keys,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_ownership_classifies_known_paths() {
        assert!(matches!(
            classify_rel_path("profiles.yml"),
            Ownership::SkipprOwned { .. }
        ));
        assert!(matches!(
            classify_rel_path("packages.yml"),
            Ownership::SkipprOwned { .. }
        ));
        assert!(matches!(
            classify_rel_path("dbt_project.yml"),
            Ownership::Shared { .. }
        ));
        assert!(matches!(
            classify_rel_path("models/staging/stg_orders.sql"),
            Ownership::LlmOwned
        ));
        assert!(matches!(
            classify_rel_path("models/schema.yml"),
            Ownership::LlmOwned
        ));
        assert!(matches!(
            classify_rel_path("macros/utility.sql"),
            Ownership::LlmOwned
        ));
        assert!(matches!(
            classify_rel_path("target/compiled/x.sql"),
            Ownership::BuildArtifact
        ));
        assert!(matches!(
            classify_rel_path("dbt_packages/dbt_utils/macros/x.sql"),
            Ownership::BuildArtifact
        ));
        assert!(matches!(
            classify_rel_path(".dbt/profile_cache"),
            Ownership::BuildArtifact
        ));
        assert!(matches!(
            classify_rel_path("logs/dbt.log"),
            Ownership::BuildArtifact
        ));
    }

    #[test]
    fn file_ownership_strips_leading_slash() {
        assert!(matches!(
            classify_rel_path("/profiles.yml"),
            Ownership::SkipprOwned { .. }
        ));
        assert!(matches!(
            classify_rel_path("/dbt_project.yml"),
            Ownership::Shared { .. }
        ));
    }

    #[test]
    fn is_writable_by_llm_matches_ownership_class() {
        assert!(!is_writable_by_llm("profiles.yml"));
        assert!(!is_writable_by_llm("packages.yml"));
        assert!(!is_writable_by_llm("target/compiled/x.sql"));
        assert!(is_writable_by_llm("dbt_project.yml"));
        assert!(is_writable_by_llm("models/staging/stg_orders.sql"));
        assert!(is_writable_by_llm("models/schema.yml"));
        assert!(is_writable_by_llm("macros/utility.sql"));
    }

    #[test]
    fn relocation_hint_returns_text_for_known_shared_keys() {
        let hint = relocation_hint("dbt_project.yml", "on-run-start").expect("hint");
        assert!(hint.contains("Project-wide hooks are system-owned"));
        assert!(hint.contains("pre_hook"));
        assert!(!hint.to_ascii_lowercase().contains("skippr"));
    }

    #[test]
    fn relocation_hint_is_none_for_non_shared_paths() {
        assert!(relocation_hint("profiles.yml", "anything").is_none());
        assert!(relocation_hint("models/x.sql", "anything").is_none());
    }

    #[test]
    fn deny_list_hint_returns_text_for_skippr_owned_paths() {
        let hint = deny_list_hint("profiles.yml").expect("hint");
        assert!(hint.contains("system-scoped"));
        assert!(hint.contains("authoritative"));
        assert!(hint.contains("immutable"));
    }

    #[test]
    fn deny_list_hint_returns_build_artifact_text() {
        let hint = deny_list_hint("target/compiled/x.sql").expect("hint");
        assert!(hint.to_ascii_lowercase().contains("build artifact"));
    }

    #[test]
    fn deny_list_hint_is_none_for_writable_paths() {
        assert!(deny_list_hint("dbt_project.yml").is_none());
        assert!(deny_list_hint("models/x.sql").is_none());
    }

    #[test]
    fn shared_skippr_top_level_keys_includes_known_hooks() {
        let keys = shared_skippr_top_level_keys("dbt_project.yml");
        assert!(keys.contains(&"on-run-start"));
        assert!(keys.contains(&"on-run-end"));
        assert!(keys.contains(&"dispatch"));
        assert!(keys.contains(&"vars"));
    }

    #[test]
    fn every_skippr_key_has_a_relocation_hint() {
        for key in DBT_PROJECT_SKIPPR_KEYS {
            let hint = relocation_hint("dbt_project.yml", key);
            assert!(
                hint.is_some(),
                "missing relocation hint for stripped key '{}'",
                key
            );
        }
    }

    #[test]
    fn hint_text_uses_system_framing_not_product_name() {
        for (_, hint) in DBT_PROJECT_RELOCATION_HINTS {
            assert!(
                !hint.to_ascii_lowercase().contains("skippr"),
                "hint must not reference internal product name: {}",
                hint
            );
        }
        for file in SKIPPR_OWNED_FILES {
            assert!(
                !file.reason.to_ascii_lowercase().contains("skippr"),
                "reason must not reference internal product name: {}",
                file.reason
            );
        }
    }
}
