use crate::progress_controller::ValidationFailureContext;
use serde::{Deserialize, Serialize};

/// Accumulated history of a repair session, ensuring every iteration builds on all prior context.
///
/// `format_for_prompt()` serialises the full history so that prompts are never identical across
/// iterations — the LLM always sees prior attempts and their outcomes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepairSessionLog {
    pub error_context: ValidationFailureContext,
    pub iterations: Vec<RepairIteration>,
}

/// A single file read during the gather phase.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatheredFile {
    pub path: String,
    pub content: String,
}

/// One iteration of the repair loop. Every field is mandatory — no `Default`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepairIteration {
    pub index: usize,
    pub gathered_files: Vec<GatheredFile>,
    pub diagnosis: String,
    pub planned_fixes: Vec<PlannedFix>,
    pub apply_results: Vec<ApplyResult>,
    pub validate_outcome: Option<ValidateOutcome>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlannedFix {
    pub path: String,
    pub op: FileOp,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileOp {
    Patch,
    Write,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApplyResult {
    pub path: String,
    pub op: FileOp,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidateOutcome {
    pub passed: bool,
    pub error_summary: String,
}

impl RepairSessionLog {
    pub fn new(error_context: ValidationFailureContext) -> Self {
        Self {
            error_context,
            iterations: Vec::new(),
        }
    }

    pub fn record(
        &mut self,
        index: usize,
        gathered_files: Vec<GatheredFile>,
        diagnosis: String,
        planned_fixes: Vec<PlannedFix>,
        apply_results: Vec<ApplyResult>,
    ) {
        self.iterations.push(RepairIteration {
            index,
            gathered_files,
            diagnosis,
            planned_fixes,
            apply_results,
            validate_outcome: None,
        });
    }

    pub fn record_validate(&mut self, outcome: ValidateOutcome) {
        if let Some(last) = self.iterations.last_mut() {
            last.validate_outcome = Some(outcome);
        }
    }

    pub fn last(&self) -> Option<&RepairIteration> {
        self.iterations.last()
    }

    /// Serialises the full session history into a prompt-ready string.
    /// Each iteration is described completely so the LLM can learn from prior attempts.
    pub fn format_for_prompt(&self) -> String {
        let mut out = String::new();

        out.push_str("## Error Context\n\n");
        out.push_str(&self.error_context.brief);
        out.push('\n');

        if let Some(ref excerpts) = self.error_context.log_excerpts {
            if !excerpts.trim().is_empty() {
                out.push_str("\n## Relevant Log Lines\n\n");
                out.push_str(excerpts);
                out.push('\n');
            }
        }

        if !self.iterations.is_empty() {
            out.push_str("\n## Prior Repair Attempts\n\n");
            for iter in &self.iterations {
                out.push_str(&format!("### Iteration {}\n\n", iter.index + 1));

                if !iter.gathered_files.is_empty() {
                    out.push_str("**Files read:**\n");
                    for gf in &iter.gathered_files {
                        out.push_str(&format!("- `{}`\n", gf.path));
                    }
                }

                if !iter.diagnosis.is_empty() {
                    out.push_str(&format!("\n**Diagnosis:** {}\n", iter.diagnosis));
                }

                if !iter.planned_fixes.is_empty() {
                    out.push_str("\n**Planned fixes:**\n");
                    for fix in &iter.planned_fixes {
                        out.push_str(&format!(
                            "- `{}` ({:?}): wrote {} chars\n",
                            fix.path,
                            fix.op,
                            fix.content.len()
                        ));
                    }
                }

                if !iter.apply_results.is_empty() {
                    out.push_str("\n**Apply results:**\n");
                    for r in &iter.apply_results {
                        let status = if r.success { "OK" } else { "FAILED" };
                        let err = r
                            .error
                            .as_deref()
                            .map(|e| format!(" — {}", e))
                            .unwrap_or_default();
                        out.push_str(&format!("- `{}` ({:?}): {}{}\n", r.path, r.op, status, err));
                    }
                }

                if let Some(v) = &iter.validate_outcome {
                    let label = if v.passed { "PASSED" } else { "FAILED" };
                    out.push_str(&format!(
                        "\n**Validation:** {} — {}\n",
                        label, v.error_summary
                    ));
                }
                out.push('\n');
            }
        }

        out
    }
}

impl RepairIteration {
    pub fn has_mutations(&self) -> bool {
        self.apply_results.iter().any(|r| r.success)
    }

    pub fn error_brief(&self) -> String {
        self.validate_outcome
            .as_ref()
            .map(|v| v.error_summary.clone())
            .unwrap_or_default()
    }

    pub fn files_changed(&self) -> Vec<String> {
        self.apply_results
            .iter()
            .filter(|r| r.success)
            .map(|r| r.path.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_for_prompt_includes_iterations() {
        let mut log = RepairSessionLog::new(ValidationFailureContext {
            brief: "dbt test failed: 2 failures".into(),
            log_excerpts: None,
            compile_ok: false,
            run_ok: false,
        });

        log.record(
            0,
            vec![GatheredFile {
                path: "models/orders.sql".into(),
                content: "SELECT ...".into(),
            }],
            "null ids in orders table".into(),
            vec![PlannedFix {
                path: "models/orders.sql".into(),
                op: FileOp::Patch,
                content: "...".into(),
            }],
            vec![ApplyResult {
                path: "models/orders.sql".into(),
                op: FileOp::Patch,
                success: false,
                error: Some("patch_hunk_context_miss".into()),
            }],
        );

        let prompt = log.format_for_prompt();
        assert!(prompt.contains("Iteration 1"));
        assert!(prompt.contains("patch_hunk_context_miss"));
        assert!(prompt.contains("dbt test failed"));
    }

    #[test]
    fn empty_session_produces_context_only() {
        let log = RepairSessionLog::new(ValidationFailureContext {
            brief: "compile error".into(),
            log_excerpts: None,
            compile_ok: false,
            run_ok: false,
        });
        let prompt = log.format_for_prompt();
        assert!(prompt.contains("compile error"));
        assert!(!prompt.contains("Prior Repair Attempts"));
    }
}
