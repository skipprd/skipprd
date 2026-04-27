use react_core::session::analysis::{Issue, IssueKind};
use react_core::session::{ThreadLog, ThreadStep, ToolStepStatus};
use react_core::suite::DebugProvider;

pub struct DataEngineerDebugProvider;

impl DebugProvider for DataEngineerDebugProvider {
    fn suite_id(&self) -> &'static str {
        "data_engineer"
    }

    fn detect_domain_issues(&self, log: &ThreadLog) -> Vec<Issue> {
        let mut issues = Vec::new();
        detect_repair_cycles(log, &mut issues);
        detect_sql_validation_loops(log, &mut issues);
        detect_phase_regression_cleanse_model(log, &mut issues);
        issues
    }

    fn domain_context(&self) -> &'static str {
        "\
The Data Engineer suite builds data pipelines through a multi-phase workflow:

## Phase Pipeline
Preflight -> (EL phases) -> CleansePlan -> CleanseAuthor -> CleanseValidate -> CleanseReview -> \
ModelPlan -> ModelAuthor -> ModelValidate -> ModelReview -> PublishAwaitApproval -> Publish -> Done

EL (Extract-Load) phases: ElDiscover -> ElSync -> ElVerify

## Key Tools
- sql_run: Executes SQL against the target warehouse (Postgres, BigQuery, Snowflake, etc.)
- files: Reads and writes SQL/YAML model files
- ask_user: Requests user input when ambiguous

## Agent Modes
The suite uses different agent modes (cleanse, model) that run the same phase pattern \
(plan -> author -> validate -> review) but with different prompts and constraints.

## Common Failure Patterns
- Repair subroutine cycles: The author phase may enter a repair loop when SQL validation \
  fails repeatedly. Look for alternating ToolStart(sql_run) and ToolEnd(sql_run, failed) steps.
- SQL validation loops: Multiple sql_run calls with similar queries that keep failing, \
  often due to schema misunderstanding.
- Phase regression: Going back from ModelAuthor to CleansePlan suggests a fundamental \
  misunderstanding of requirements.
- Provider connection failures: Repeated errors from sql_run with connection-related messages."
    }
}

fn detect_repair_cycles(log: &ThreadLog, issues: &mut Vec<Issue>) {
    let mut consecutive_fails = 0;
    let mut run_start = 0;

    for (idx, step) in log.steps.iter().enumerate() {
        if let ThreadStep::ToolEnd { name, status, .. } = step {
            if name == "sql_run" {
                match status {
                    ToolStepStatus::Failed => {
                        if consecutive_fails == 0 {
                            run_start = idx;
                        }
                        consecutive_fails += 1;
                    }
                    ToolStepStatus::Ok => {
                        consecutive_fails = 0;
                    }
                    _ => {}
                }
                if consecutive_fails >= 3 {
                    issues.push(Issue {
                        kind: IssueKind::Loop,
                        description: format!(
                            "Repair subroutine: {} consecutive sql_run failures",
                            consecutive_fails
                        ),
                        step_range: (run_start, idx),
                    });
                    consecutive_fails = 0;
                }
            }
        }
    }
}

fn detect_sql_validation_loops(log: &ThreadLog, issues: &mut Vec<Issue>) {
    let window = 15;
    let threshold = 5;

    for start in 0..log
        .steps
        .len()
        .saturating_sub(window)
        .max(1)
        .min(log.steps.len())
    {
        let end = (start + window).min(log.steps.len());
        let sql_run_count = log.steps[start..end]
            .iter()
            .filter(|s| matches!(s, ThreadStep::ToolEnd { name, .. } if name == "sql_run"))
            .count();

        if sql_run_count >= threshold {
            let already = issues.iter().any(|i| {
                i.kind == IssueKind::Loop
                    && i.description.contains("sql_run validation loop")
                    && i.step_range.1 >= start
            });
            if !already {
                issues.push(Issue {
                    kind: IssueKind::Loop,
                    description: format!(
                        "sql_run validation loop: {} calls in {} steps",
                        sql_run_count, window
                    ),
                    step_range: (start, end),
                });
            }
        }
    }
}

fn detect_phase_regression_cleanse_model(log: &ThreadLog, issues: &mut Vec<Issue>) {
    let mut seen_model = false;
    for (idx, step) in log.steps.iter().enumerate() {
        if let ThreadStep::Phase { phase, .. } = step {
            if phase.starts_with("model_") {
                seen_model = true;
            }
            if seen_model && phase.starts_with("cleanse_") {
                issues.push(Issue {
                    kind: IssueKind::PhaseRegression,
                    description: format!("Regressed from model phases back to '{}'", phase),
                    step_range: (idx, idx),
                });
            }
        }
    }
}
