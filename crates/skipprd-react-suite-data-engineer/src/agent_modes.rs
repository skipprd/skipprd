use super::*;

/// External-facing interaction modes for the data_engineer suite.
///
/// The suite README lists five agent *types* (ask, review, agent, plan, validate),
/// but plan/validate/author/publish are internal phases driven by the `Agent` mode's
/// phase loop (see `execute_phase`). Only these three represent distinct entry points
/// that callers can select via `dispatch_agent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum AgentMode {
    Ask,
    Review,
    Agent,
}

impl std::str::FromStr for AgentMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ask" => Ok(Self::Ask),
            "review" => Ok(Self::Review),
            "agent" => Ok(Self::Agent),
            _ => Err(format!(
                "invalid agent_type '{}' for suite 'data_engineer' (expected 'ask' | 'review' | 'agent')",
                s
            )),
        }
    }
}

impl AgentMode {
    pub(super) fn parse(raw: &str) -> Result<Self, String> {
        raw.parse()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum AgentToolCapability {
    ReadOnlyFile,
    MutableFile,
    RunSql,
    AskUser,
    AskApproval,
    SearchDbtExamples,
    StagingModel,
    GoldModel,
    DbtValidate,
    PublishDbt,
    SqlRegister,
    CatalogNote,
    Artifacts,
}

struct DataEngineerExecutor<'a> {
    thread_store: ThreadStore,
    sctx: &'a SuiteCtx,
    thread_id: &'a str,
    question: &'a str,
    max_replan_backtracks: usize,
}

#[async_trait::async_trait]
impl<'a> react_core::workflow::PhaseExecutor for DataEngineerExecutor<'a> {
    async fn execute_turn(&self, out_frames: &mut Vec<FlowFrame>) -> PhaseOutcome {
        let metering = crate::metering::global_metering();
        if let Err(e) = metering.budget.check_and_refresh().await {
            tracing::error!(error = %e, "credit budget exhausted at phase boundary");
            return PhaseOutcome::Failed { reason: e };
        }

        let execution_state = match crate::progress_controller::ExecutionState::load_strict(
            &self.thread_store.control_store(),
            self.thread_id,
        )
        .await
        {
            Ok(Some(es)) => es,
            Ok(None) => crate::progress_controller::ExecutionState::new(),
            Err(e) => return PhaseOutcome::Failed { reason: e },
        };

        let phase = execution_state.phase.current_phase;
        let thread_log = self.thread_store.get(self.thread_id).await.ok();
        let thread_state_step_count = thread_log.as_ref().map(|log| log.steps.len()).unwrap_or(0);

        match crate::phase_gate::evaluate_pre_turn_directive(
            &execution_state,
            phase,
            self.max_replan_backtracks,
        ) {
            crate::phase_gate::PreTurnDirective::Proceed => {}
            crate::phase_gate::PreTurnDirective::FailFast { kind, reason } => {
                if let Err(e) = crate::transition_dispatcher::apply_phase_directive(
                    &self.thread_store,
                    self.thread_id,
                    Some(env_util::DEFAULT_AGENT_NAME.to_string()),
                    Some(phase),
                    crate::transition_dispatcher::PhaseDirective::Block {
                        phase,
                        kind,
                        reason: reason.clone(),
                    },
                )
                .await
                {
                    tracing::error!("failed to persist guard block on FailFast: {e}");
                }
                if let Err(e) = crate::state_manager::apply_execution_event(
                    &self.thread_store.control_store(),
                    self.thread_id,
                    crate::progress_controller::DataEngineerEvent::MarkedFailed {
                        reason: reason.clone(),
                    },
                )
                .await
                {
                    tracing::error!("failed to persist mark_failed on FailFast: {e}");
                }
                return PhaseOutcome::Failed { reason };
            }
        }

        let mut repair_ctx = execution_state.repair_context();
        if let Some(log) = thread_log.as_ref() {
            if let Some(track) = TrackKind::from_any_phase(phase) {
                repair_ctx.recent_failed_file_ops =
                    DataEngineerSuite::collect_recent_failed_file_ops(log, track);
            }
        }
        let outcome = DataEngineerSuite::execute_phase(
            &self.thread_store,
            self.thread_id,
            phase,
            self.question,
            self.sctx,
            &execution_state,
            thread_state_step_count,
            &repair_ctx,
            out_frames,
        )
        .await;

        match outcome {
            PhaseOutcome::TransitionCommitted => {
                PhaseOutcome::stayed_with_progress("phase transition committed")
            }
            other => other,
        }
    }

    async fn on_budget_exhausted(&self, _out_frames: &mut Vec<FlowFrame>, total_steps: usize) {
        let budget_msg = format!(
            "headless_budget_exhausted: Agent reached the phase-step budget without completing.\n- total_steps={}",
            total_steps,
        );
        if let Some(mut es) = crate::progress_controller::ExecutionState::load(
            &self.thread_store.control_store(),
            self.thread_id,
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!("failed to load execution state at budget exhaustion: {e}");
            None
        }) {
            let detail = format!(
                "{}\n\nExecution state at exhaustion:\n- current_phase={}\n- phase_reason_code={}\n- replan_backtracks={}\n- repair_cycles={}/{}\n- hard_mutation_repair_mode={}",
                budget_msg,
                es.phase.current_phase.as_str(),
                es.phase.transition.as_ref().map(|t| t.as_reason_str().to_string()).unwrap_or_else(|| "null".to_string()),
                es.phase.replan_backtracks,
                es.repair.cycle_count(),
                crate::progress_controller::MAX_REPAIR_CYCLES,
                es.hard_mutation_repair_mode(),
            );
            es.mark_failed(&detail);
            if let Err(e) = es
                .save(&self.thread_store.control_store(), self.thread_id)
                .await
            {
                tracing::error!("failed to persist budget-exhaustion mark_failed: {e}");
            }
        }
    }

    async fn step_count(&self) -> usize {
        self.thread_store
            .get(self.thread_id)
            .await
            .ok()
            .map(|log| log.steps.len())
            .unwrap_or(0)
    }
}

impl DataEngineerSuite {
    /// Collect recent failed file mutations from the thread log, scoped to a
    /// specific track. Requiring `track` at the call site is a compile-time
    /// guarantee that the caller cannot accidentally leak cross-phase context.
    fn collect_recent_failed_file_ops(
        log: &react_core::session::ThreadLog,
        track: TrackKind,
    ) -> Vec<crate::progress_controller::RecentFailedFileOp> {
        use react_core::session::{ThreadStep, ToolStepStatus};

        let mut counts: std::collections::BTreeMap<(String, String), (String, usize)> =
            std::collections::BTreeMap::new();
        let mut insertion_order: Vec<(String, String)> = Vec::new();

        let mut current_track: Option<TrackKind> = None;
        for step in log.steps.iter() {
            if let ThreadStep::Phase { phase, .. } = step {
                current_track =
                    control_flow::Phase::from_str(phase.trim()).and_then(TrackKind::from_any_phase);
                continue;
            }
            if current_track != Some(track) {
                continue;
            }
            let ThreadStep::ToolEnd {
                name,
                args,
                status,
                observation,
                ..
            } = step
            else {
                continue;
            };
            if *status != ToolStepStatus::Failed || name != "file" {
                continue;
            }
            let op = args
                .get("op")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if !matches!(op.as_str(), "patch" | "rm" | "mv" | "write") {
                continue;
            }
            let path = match op.as_str() {
                "mv" => {
                    let from = args
                        .get("from")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    let to = args.get("to").and_then(|v| v.as_str()).unwrap_or("").trim();
                    if from.is_empty() && to.is_empty() {
                        continue;
                    }
                    format!("{from} -> {to}").trim().to_string()
                }
                _ => args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            };
            if path.is_empty() {
                continue;
            }
            let error_brief = observation
                .errors
                .first()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "tool failed without a specific error message".to_string());
            let key = (op.clone(), path.clone());
            let entry = counts.entry(key.clone()).or_insert_with(|| {
                insertion_order.push(key);
                (error_brief.clone(), 0)
            });
            entry.0 = error_brief;
            entry.1 += 1;
        }

        let skip = insertion_order.len().saturating_sub(8);
        insertion_order
            .into_iter()
            .skip(skip)
            .filter_map(|(op, path)| {
                let (error_brief, count) = counts.remove(&(op.clone(), path.clone()))?;
                Some(crate::progress_controller::RecentFailedFileOp {
                    op,
                    path,
                    error_brief,
                    count,
                })
            })
            .collect()
    }

    pub(super) fn agent_capability_profile(
        agent_mode: AgentMode,
        allow_user_interrupt_tools: bool,
    ) -> BTreeSet<AgentToolCapability> {
        let mut caps = BTreeSet::new();
        match agent_mode {
            AgentMode::Review => {
                caps.insert(AgentToolCapability::ReadOnlyFile);
                caps.insert(AgentToolCapability::Artifacts);
            }
            AgentMode::Ask => {
                caps.insert(AgentToolCapability::MutableFile);
                caps.insert(AgentToolCapability::RunSql);
                caps.insert(AgentToolCapability::AskApproval);
                caps.insert(AgentToolCapability::Artifacts);
            }
            AgentMode::Agent => {
                caps.insert(AgentToolCapability::MutableFile);
                caps.insert(AgentToolCapability::RunSql);
                caps.insert(AgentToolCapability::AskApproval);
                caps.insert(AgentToolCapability::SearchDbtExamples);
                caps.insert(AgentToolCapability::StagingModel);
                caps.insert(AgentToolCapability::GoldModel);
                caps.insert(AgentToolCapability::DbtValidate);
                caps.insert(AgentToolCapability::PublishDbt);
                caps.insert(AgentToolCapability::SqlRegister);
                caps.insert(AgentToolCapability::CatalogNote);
                caps.insert(AgentToolCapability::Artifacts);
            }
        }
        if allow_user_interrupt_tools && agent_mode != AgentMode::Review {
            caps.insert(AgentToolCapability::AskUser);
        }
        caps
    }

    pub(super) fn headless_mode_enabled() -> bool {
        env_util::headless_mode_enabled()
    }

    pub(super) fn enforce_non_interactive_contract(
        agent_mode: AgentMode,
        frames: Vec<FlowFrame>,
    ) -> Result<Vec<FlowFrame>, String> {
        let await_user_prompt = frames.iter().find_map(|f| match f {
            FlowFrame::Interrupt { kind, prompt } if kind == "await_user" => Some(prompt.clone()),
            _ => None,
        });
        if let Some(prompt) = await_user_prompt {
            if agent_mode == AgentMode::Agent {
                return Err(format!("agent_mode_await_user_forbidden: {}", prompt));
            }
            if Self::headless_mode_enabled() {
                return Err(format!("await_user_forbidden_in_headless: {}", prompt));
            }
        }
        Ok(frames)
    }

    pub(super) fn build_tools_card(
        header: &str,
        tool_lines: Vec<String>,
        notes: Vec<String>,
        not_available: Option<String>,
    ) -> String {
        let mut lines: Vec<String> = Vec::new();
        lines.push(header.to_string());
        lines.extend(tool_lines);
        if !notes.is_empty() {
            lines.push(String::new());
            lines.extend(notes);
        }
        if let Some(na) = not_available {
            lines.push(String::new());
            lines.push(na);
        }
        lines.join("\n")
    }

    pub(super) async fn check_subjective_retry_budget(
        thread_store: &ThreadStore,
        thread_id: &str,
        kind: crate::progress_controller::SubjectiveRetryKind,
    ) -> Result<crate::retry_budget::SubjectiveRetryOutcome, String> {
        crate::retry_budget::check_subjective_retry_budget(thread_store, thread_id, kind).await
    }

    pub(super) async fn clear_subjective_retries(
        thread_store: &ThreadStore,
        thread_id: &str,
        kinds: Vec<crate::progress_controller::SubjectiveRetryKind>,
    ) -> Result<(), String> {
        crate::retry_budget::clear_subjective_retries(thread_store, thread_id, kinds).await
    }

    fn el_enabled(_sctx: &SuiteCtx) -> bool {
        false
    }

    pub(super) fn validate_agent_type(agent_type: &str) -> Result<AgentMode, String> {
        AgentMode::parse(agent_type)
    }

    pub(super) fn inject_review_question(question: &str) -> String {
        prompts::shared::user_goal_line("Review request:", question)
    }

    pub(super) fn inject_model_question(question: &str) -> String {
        prompts::shared::user_goal_line("Modeling goal:", question)
    }

    pub(super) fn inject_cleanse_question(question: &str) -> String {
        prompts::shared::user_goal_line("Cleansing goal:", question)
    }

    pub(super) async fn run_ask(
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        let sys = prompts::with_time_context(prompts::ask_system_prompt());
        let tools_card = Self::build_tools_card_for_agent_type(AgentMode::Ask);

        let pf = preflight::CatalogPreflightProvider {
            discovery_limits: preflight::discovery::DiscoveryLimits::default(),
        };
        let bundle = pf.run(thread_id, question, "ask", sctx).await.discovery;

        let registry = Self::build_tools(AgentMode::Ask, sctx)?;
        let actx = Self::build_agent_ctx(
            sctx,
            thread_id,
            "ask",
            std::sync::Arc::new(SqlValidatedPolicy {
                dataset_candidates: bundle
                    .datasets
                    .iter()
                    .take(8)
                    .map(|(ds, sc)| DatasetCandidate {
                        dataset_id: ds.clone(),
                        score: *sc,
                    })
                    .collect(),
                ..SqlValidatedPolicy::default()
            }),
            env_util::ASK_MAX_STEPS,
            10,
        );

        Self::run_outcome_to_frames(
            Agent::run_until_block(
                &registry,
                &actx,
                &sys,
                &tools_card,
                question,
                LlmCallOptions {
                    prompt_id: "data_engineer.ask_user_parse",
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    max_output_tokens: Some(env_util::ask_max_tokens()),
                    reasoning_effort: Some(env_util::ask_reasoning_effort()),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| e.to_string()),
        )
    }

    pub(super) async fn run_review(
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        Self::ensure_catalog_bootstrap_semaphored(thread_id, sctx).await?;
        let sys = prompts::with_time_context(prompts::review_system_prompt());
        let tools_card = Self::build_tools_card_for_agent_type(AgentMode::Review);

        let registry = Self::build_tools(AgentMode::Review, sctx)?;
        let actx = Self::build_agent_ctx(
            sctx,
            thread_id,
            "review",
            std::sync::Arc::new(react_core::agent::DefaultPolicy),
            env_util::REVIEW_MAX_STEPS,
            10,
        );

        let prompt = Self::inject_review_question(question);
        Self::run_outcome_to_frames(
            Agent::run_until_block(
                &registry,
                &actx,
                &sys,
                &tools_card,
                &prompt,
                LlmCallOptions {
                    prompt_id: "data_engineer.ask_approval_parse",
                    expected_format: react_core::llm::LlmExpectedFormat::JsonObject,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| e.to_string()),
        )
    }

    pub(super) fn build_agent_ctx(
        sctx: &SuiteCtx,
        thread_id: &str,
        agent_name: &str,
        policy: std::sync::Arc<dyn react_core::agent::AgentPolicy>,
        max_steps: usize,
        per_step_timeout: u64,
    ) -> AgentCtx {
        let thread_store = ThreadStore::new(
            sctx.storage().clone(),
            sctx.scope().clone(),
            sctx.keyspace().clone(),
        );
        let mut actx = react_core::agent::AgentCtxBuilder::new(
            sctx.llm().clone(),
            sctx.storage().clone(),
            sctx.scope().clone(),
            sctx.keyspace().clone(),
            policy,
        )
        .top_k(env_util::DEFAULT_TOP_K)
        .per_step_timeout_secs(per_step_timeout)
        .max_steps(max_steps)
        .thread_id(thread_id)
        .trace_tx(sctx.trace_tx().clone())
        .agent_name(agent_name)
        .vector(sctx.vector().clone())
        .thread_store(thread_store)
        .resolved_config(sctx.resolved_config().clone())
        .build();
        crate::ctx_ext::copy_capabilities_to_actx(sctx, &mut actx);
        actx
    }

    pub(super) fn run_outcome_to_frames(
        outcome: Result<RunOutcome, String>,
    ) -> Result<Vec<FlowFrame>, String> {
        match outcome {
            Ok(RunOutcome::Complete {
                thread_id: _tid,
                result,
            }) => Ok(vec![FlowFrame::Complete {
                kind: FlowKind::new(result.kind.clone()),
                payload: result.payload,
                display: result.display,
            }]),
            Ok(RunOutcome::Interrupt {
                thread_id: _tid,
                kind,
                prompt,
            }) => {
                let kind_str = match kind {
                    react_core::agent::InterruptKind::AwaitUser => "await_user",
                    react_core::agent::InterruptKind::AwaitApproval => "await_approval",
                };
                Ok(vec![FlowFrame::Interrupt {
                    kind: FlowKind::new(kind_str),
                    prompt,
                }])
            }
            Err(e) => Err(e),
        }
    }

    pub(super) fn agent_tool_ctx(thread_id: &str, sctx: &SuiteCtx) -> AgentCtx {
        Self::build_agent_ctx(
            sctx,
            thread_id,
            env_util::DEFAULT_AGENT_NAME,
            std::sync::Arc::new(react_core::agent::DefaultPolicy),
            env_util::APPROVAL_PARSE_MAX_STEPS,
            10,
        )
    }

    pub(super) fn plan_agent_ctx(thread_id: &str, sctx: &SuiteCtx) -> AgentCtx {
        Self::build_agent_ctx(
            sctx,
            thread_id,
            env_util::DEFAULT_AGENT_NAME,
            std::sync::Arc::new(InterruptOnlyPolicy),
            env_util::PLAN_DISCOVERY_MAX_STEPS,
            20,
        )
    }

    pub(super) async fn run_agent(
        thread_id: &str,
        question: &str,
        sctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        use control_flow::Phase;

        let has_el = Self::el_enabled(sctx);

        if has_el {
            let thread_store = ThreadStore::new(
                sctx.storage().clone(),
                sctx.scope().clone(),
                sctx.keyspace().clone(),
            );
            let existing = crate::progress_controller::ExecutionState::load_strict(
                &thread_store.control_store(),
                thread_id,
            )
            .await
            .unwrap_or(None);
            if existing.is_none() {
                let mut es = crate::progress_controller::ExecutionState::new();
                es.phase.current_phase = Phase::ElDiscover;
                if let Err(e) = es.save(&thread_store.control_store(), thread_id).await {
                    tracing::warn!("failed to seed EL initial phase: {e}");
                }
            }
        }

        if !has_el {
            if let Err(e) = Self::ensure_catalog_bootstrap_semaphored(thread_id, sctx).await {
                let thread_store = ThreadStore::new(
                    sctx.storage().clone(),
                    sctx.scope().clone(),
                    sctx.keyspace().clone(),
                );
                let reason = format!(
                    "catalog bootstrap metadata gate failed before planning:\n{}",
                    e.trim()
                );
                tracing::error!(
                    thread_id = %thread_id,
                    error = %e,
                    "data_engineer: refusing to continue after preflight catalog metadata gate failure"
                );
                let _ = apply_guard_block(
                    &thread_store,
                    thread_id,
                    Phase::Preflight,
                    GuardBlockKind::PrecheckFailed,
                    reason.clone(),
                )
                .await;
                return Err(format!("agent_mode_await_user_forbidden: {}", reason));
            }
        }

        let model_name = sctx
            .resolved_config()
            .as_ref()
            .and_then(|c| c.llm.reason_model.clone())
            .unwrap_or_default();
        let max_phase_steps: usize = env_util::max_phase_steps_for_model(&model_name);
        let max_replan_backtracks: usize = control_flow::replan_backtrack_counter_cap();

        let thread_store = ThreadStore::new(
            sctx.storage().clone(),
            sctx.scope().clone(),
            sctx.keyspace().clone(),
        );

        let executor = DataEngineerExecutor {
            thread_store,
            sctx,
            thread_id,
            question,
            max_replan_backtracks,
        };

        let config = react_core::workflow::WorkflowConfig {
            max_phase_steps,
            max_consecutive_waiting_idle: 5,
            max_consecutive_waiting_active: 12,
            ..Default::default()
        };

        react_core::workflow::runner::run(&executor, &config).await
    }

    /// Infallible phase dispatcher. Individual phase executors may return
    /// `Err(String)` internally; this boundary catches every such error,
    /// records a `PhaseExecutionError` guard block + `mark_failed` in
    /// ControlState, and converts the error into `PhaseOutcome::Failed`.
    ///
    /// Because the return type carries no `Result`, the caller (`run_agent`)
    /// cannot use `?` to silently discard errors — the compiler forces an
    /// exhaustive match on the `Failed` variant.
    pub(super) async fn execute_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: crate::control_flow::Phase,
        question: &str,
        sctx: &SuiteCtx,
        execution_state: &crate::progress_controller::ExecutionState,
        thread_state_step_count: usize,
        repair_ctx: &crate::progress_controller::RepairContext,
        out_frames: &mut Vec<FlowFrame>,
    ) -> PhaseOutcome {
        match Self::execute_phase_inner(
            thread_store,
            thread_id,
            phase,
            question,
            sctx,
            execution_state,
            thread_state_step_count,
            repair_ctx,
            out_frames,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                let reason = e.to_string();
                tracing::error!(
                    thread_id = %thread_id,
                    phase = %phase.as_str(),
                    error = %reason,
                    "phase execution error — recording guard block",
                );
                let _ = apply_guard_block(
                    thread_store,
                    thread_id,
                    phase,
                    GuardBlockKind::PhaseExecutionError,
                    &reason,
                )
                .await;
                if let Err(e) = crate::state_manager::apply_execution_event(
                    &thread_store.control_store(),
                    thread_id,
                    crate::progress_controller::DataEngineerEvent::MarkedFailed {
                        reason: reason.clone(),
                    },
                )
                .await
                {
                    tracing::error!("failed to persist mark_failed after phase error: {e}");
                }
                PhaseOutcome::Failed { reason }
            }
        }
    }

    async fn execute_phase_inner(
        thread_store: &ThreadStore,
        thread_id: &str,
        phase: crate::control_flow::Phase,
        question: &str,
        sctx: &SuiteCtx,
        execution_state: &crate::progress_controller::ExecutionState,
        thread_state_step_count: usize,
        repair_ctx: &crate::progress_controller::RepairContext,
        out_frames: &mut Vec<FlowFrame>,
    ) -> Result<PhaseOutcome, PhaseError> {
        use crate::control_flow::Phase;
        match phase {
            Phase::ElDiscover => {
                Self::execute_el_discover_phase(thread_store, thread_id, sctx).await
            }
            Phase::ElSync => Self::execute_el_sync_phase(thread_store, thread_id, sctx).await,
            Phase::ElVerify => Self::execute_el_verify_phase(thread_store, thread_id, sctx).await,
            Phase::Preflight => Self::execute_preflight_phase(thread_store, thread_id, sctx).await,
            Phase::CleansePlan | Phase::ModelPlan => {
                Self::execute_plan_phase(
                    thread_store,
                    thread_id,
                    phase,
                    question,
                    sctx,
                    execution_state,
                    thread_state_step_count,
                    repair_ctx,
                )
                .await
            }
            Phase::CleanseAuthor | Phase::ModelAuthor => {
                if execution_state.hard_mutation_repair_mode() {
                    Self::execute_repair_phase(
                        thread_store,
                        thread_id,
                        sctx,
                        execution_state,
                        repair_ctx,
                    )
                    .await
                } else {
                    Self::execute_author_phase(
                        thread_store,
                        thread_id,
                        phase,
                        question,
                        sctx,
                        execution_state,
                        thread_state_step_count,
                        repair_ctx,
                    )
                    .await
                }
            }
            Phase::CleanseValidate | Phase::ModelValidate => {
                Self::execute_validate_phase(
                    thread_store,
                    thread_id,
                    phase,
                    question,
                    sctx,
                    execution_state,
                    thread_state_step_count,
                )
                .await
            }
            Phase::CleanseReview | Phase::ModelReview => {
                Self::execute_review_phase(
                    thread_store,
                    thread_id,
                    phase,
                    question,
                    sctx,
                    execution_state,
                    thread_state_step_count,
                    out_frames,
                )
                .await
            }
            Phase::PublishAwaitApproval => {
                Self::execute_publish_await_approval_phase(thread_store, thread_id, sctx).await
            }
            Phase::Publish => Self::execute_publish_phase(thread_store, thread_id, sctx).await,
            Phase::Done => Self::execute_done_phase(out_frames),
        }
    }

    async fn execute_repair_phase(
        thread_store: &ThreadStore,
        thread_id: &str,
        sctx: &SuiteCtx,
        execution_state: &crate::progress_controller::ExecutionState,
        _repair_ctx: &crate::progress_controller::RepairContext,
    ) -> Result<PhaseOutcome, PhaseError> {
        let current_phase = execution_state.phase.current_phase;
        let track = crate::track_spec::TrackKind::from_any_phase(current_phase)
            .ok_or_else(|| format!("repair phase has no track: {}", current_phase.as_str()))?;
        let validate_phase = track.validate_phase();

        let actx = Self::agent_tool_ctx(thread_id, sctx);
        let cfg = crate::resolved_config_from_ctx(&actx);
        let dispatch = cfg
            .map(|c| crate::model_dispatch::ModelDispatch::from_resolved(&c.llm))
            .unwrap_or_else(|| crate::model_dispatch::ModelDispatch {
                reason_model: "gpt-4o-mini".into(),
                task_model: "gpt-4o-mini".into(),
            });

        let error_context = execution_state
            .repair
            .failure_context
            .clone()
            .unwrap_or_else(|| crate::progress_controller::ValidationFailureContext {
                brief: String::new(),
                log_excerpts: None,
                compile_ok: false,
                run_ok: false,
            });

        let repair_cycle = execution_state.repair.cycle_count();
        let result = crate::repair_subroutine::run_repair(
            sctx,
            thread_store,
            thread_id,
            &dispatch,
            error_context,
            None,
            repair_cycle,
        )
        .await;

        match result {
            Ok(_) => {
                crate::state_manager::apply_execution_event(
                    &thread_store.control_store(),
                    thread_id,
                    crate::progress_controller::DataEngineerEvent::RepairSucceeded,
                )
                .await
                .map_err(|e| format!("failed to clear repair state after success: {e}"))?;

                crate::phase_contract::commit_metered_decision(
                    thread_store,
                    thread_id,
                    Some(current_phase),
                    crate::phase_contract::PhaseDecision::forward(
                        validate_phase,
                        Some(crate::progress_controller::PhaseTransition::RepairCompleted),
                    ),
                    vec![crate::metering::UsageEvent::RepairCycle {
                        cycle: 1,
                        project_id: thread_id.to_string(),
                    }],
                    crate::metering::global_metering(),
                )
                .await?;
                Ok(PhaseOutcome::TransitionCommitted)
            }
            Err(e) => {
                crate::state_manager::apply_execution_event(
                    &thread_store.control_store(),
                    thread_id,
                    crate::progress_controller::DataEngineerEvent::RepairExhausted,
                )
                .await
                .map_err(|e2| format!("failed to mark repair exhausted: {e2}"))?;

                crate::phase_contract::commit_metered_decision(
                    thread_store,
                    thread_id,
                    Some(current_phase),
                    crate::phase_contract::PhaseDecision::forward(
                        validate_phase,
                        Some(
                            crate::progress_controller::PhaseTransition::RepairExhausted {
                                reason: e.clone(),
                            },
                        ),
                    ),
                    vec![crate::metering::UsageEvent::RepairCycle {
                        cycle: 1,
                        project_id: thread_id.to_string(),
                    }],
                    crate::metering::global_metering(),
                )
                .await?;
                Ok(PhaseOutcome::TransitionCommitted)
            }
        }
    }

    pub(super) async fn dispatch_agent(
        &self,
        thread_id: &str,
        question: &str,
        agent_type: &str,
        ctx: &SuiteCtx,
    ) -> Result<Vec<FlowFrame>, String> {
        let agent_mode = Self::validate_agent_type(agent_type)?;
        let frames = match agent_mode {
            AgentMode::Agent => Self::run_agent(thread_id, question, ctx).await,
            AgentMode::Review => Self::run_review(thread_id, question, ctx).await,
            AgentMode::Ask => Self::run_ask(thread_id, question, ctx).await,
        }?;
        Self::enforce_non_interactive_contract(agent_mode, frames)
    }
}
