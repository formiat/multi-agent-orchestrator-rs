use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::errors::{OrchestratorError, OrchestratorResult};
use crate::providers::DispatchedProcess;
use crate::report::{reason_code_for_state, FinalReport, TransportBodyReport};
use crate::state::{
    AgentRole, AttemptStateData, NotionPolicy, ProviderKind, RemoteNetworkPolicy, RunState,
    WorkflowType,
};
use crate::yaml_check::ReviewerYaml;

mod monitor;
mod phases;

/// Global atomic cancel flag set by the signal handler.
pub static CANCEL_FLAG: AtomicBool = AtomicBool::new(false);

/// Install SIGINT / SIGTERM handlers that set `CANCEL_FLAG`.
pub fn install_cancel_handler() {
    tokio::spawn(async {
        use tokio::signal;
        let mut sigterm = match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to install SIGTERM handler: {e}");
                return;
            }
        };
        tokio::select! {
            _ = signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        CANCEL_FLAG.store(true, Ordering::SeqCst);
        tracing::info!("cancellation signal received");
    });
}

fn is_cancelled() -> bool {
    CANCEL_FLAG.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// CLI args (owned by orchestrator module; parsed in main.rs)
// ---------------------------------------------------------------------------

/// Validated CLI arguments passed to the orchestrator.
pub struct CliArgs {
    pub workflow_type: WorkflowType,
    pub notion_policy: NotionPolicy,
    pub remote_network_policy: RemoteNetworkPolicy,
    pub workspace_root: PathBuf,
    pub executor_thread_name: String,
    pub reviewer_thread_name: String,
    pub prompt: String,
    pub executor_provider: ProviderKind,
    pub reviewer_provider: ProviderKind,
    pub executor_model: Option<String>,
    pub reviewer_model: Option<String>,
}

// ---------------------------------------------------------------------------
// Orchestrator context
// ---------------------------------------------------------------------------

/// Full runtime context threaded through the orchestration loop.
pub struct OrchestratorCtx {
    // --- Inputs ---
    pub args: CliArgs,

    // --- Run state ---
    pub run_state: RunState,
    pub consecutive_failure_count: u32,
    pub current_role: Option<AgentRole>,

    // --- Context bundle (set during CONTEXT_PREP) ---
    pub branch: String,
    /// Git status captured immediately before reviewer dispatch; used as baseline in
    /// `check_reviewer_git_state` to detect reviewer-introduced mutations.
    pub pre_reviewer_git_status: Option<String>,
    /// SHA-256 of `git diff HEAD` captured immediately before reviewer dispatch.
    ///
    /// Detects content changes to files that were already dirty before reviewer ran —
    /// those files have identical `git status --short` lines before and after, so
    /// status-line comparison alone misses the mutation.
    pub pre_reviewer_git_diff_hash: Option<String>,
    /// SHA-256 of all untracked-file contents (excluding outbox) captured immediately before
    /// reviewer dispatch. Detects content changes to untracked files, which `git diff HEAD`
    /// and status-line comparison both miss (status stays `??` before and after).
    pub pre_reviewer_untracked_hash: Option<String>,
    /// Commit SHA captured at CONTEXT_PREP; used to build `initial_head..HEAD` ranges.
    pub initial_git_head: Option<String>,
    pub claude_project_key: Option<String>,

    // --- Session bindings (set during SESSION_BIND) ---
    pub executor_session_id: Option<String>,
    pub reviewer_session_id: Option<String>,

    /// Lock files held open for the duration of the run (drop to release).
    pub(super) executor_lock: Option<std::fs::File>,
    pub(super) reviewer_lock: Option<std::fs::File>,

    // --- Active attempt ---
    pub attempt: Option<AttemptStateData>,
    pub active_process: Option<DispatchedProcess>,

    // --- Collected output facts ---
    pub artifact_map: HashMap<String, bool>,
    pub outbox_present: bool,

    // --- Review result ---
    pub review_result: Option<ReviewerYaml>,
    pub review_result_yaml_raw: Option<String>,
    pub reviewer_yaml_rejection: Option<String>,

    // --- Transport diagnostics ---
    pub last_inbox_snapshot: Option<TransportBodyReport>,
    pub executor_outbox_snapshot: Option<TransportBodyReport>,
    pub reviewer_outbox_snapshot: Option<TransportBodyReport>,

    // --- Report extras ---
    pub commit_hashes: Vec<String>,
    pub artifact_paths: Vec<String>,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
    /// key: repository path from `git status --short`; value: number of probe deltas that touched it.
    pub file_touch_counts: HashMap<String, u32>,
    /// Last coarse phase hint derived from deterministic probe signals.
    pub last_phase_hint: Option<String>,
    /// Number of currently changed files from the latest probe.
    pub last_changed_files_count: usize,
    /// Last observed provider-side command (from stdout/stderr diagnostics tail).
    pub last_provider_action: Option<String>,
    /// Timestamp when `last_provider_action` was first observed.
    pub last_provider_action_ts: Option<chrono::DateTime<chrono::Utc>>,
    pub detail: Option<serde_json::Value>,
    pub next_action_required: Option<String>,
}

impl OrchestratorCtx {
    pub fn new(args: CliArgs) -> Self {
        Self {
            args,
            run_state: RunState::RunInit,
            consecutive_failure_count: 0,
            current_role: None,
            branch: String::new(),
            pre_reviewer_git_status: None,
            pre_reviewer_git_diff_hash: None,
            pre_reviewer_untracked_hash: None,
            initial_git_head: None,
            claude_project_key: None,
            executor_session_id: None,
            reviewer_session_id: None,
            executor_lock: None,
            reviewer_lock: None,
            attempt: None,
            active_process: None,
            artifact_map: HashMap::new(),
            outbox_present: false,
            review_result: None,
            review_result_yaml_raw: None,
            reviewer_yaml_rejection: None,
            last_inbox_snapshot: None,
            executor_outbox_snapshot: None,
            reviewer_outbox_snapshot: None,
            commit_hashes: Vec::new(),
            artifact_paths: Vec::new(),
            warnings: Vec::new(),
            failures: Vec::new(),
            file_touch_counts: HashMap::new(),
            last_phase_hint: None,
            last_changed_files_count: 0,
            last_provider_action: None,
            last_provider_action_ts: None,
            detail: None,
            next_action_required: None,
        }
    }

    /// Return the workspace root path.
    pub fn repo(&self) -> &std::path::Path {
        &self.args.workspace_root
    }

    /// Return the active session id for the given role.
    pub fn session_id(&self, role: AgentRole) -> Option<&str> {
        match role {
            AgentRole::Executor => self.executor_session_id.as_deref(),
            AgentRole::Reviewer => self.reviewer_session_id.as_deref(),
        }
    }

    /// Build the final JSON report.
    pub fn build_report(&self) -> FinalReport {
        let review = self.review_result.as_ref();

        FinalReport {
            workflow_type: self.args.workflow_type,
            state: self.run_state,
            reason_code: reason_code_for_state(self.run_state),
            workspace_root: self.repo().display().to_string(),
            branch: self.branch.clone(),
            notion_policy: self.args.notion_policy,
            remote_network_policy: self.args.remote_network_policy,
            executor_provider: Some(self.args.executor_provider),
            executor_session_id: self.executor_session_id.clone(),
            executor_thread_name: self.args.executor_thread_name.clone(),
            executor_model: self.args.executor_model.clone(),
            reviewer_provider: Some(self.args.reviewer_provider),
            reviewer_session_id: self.reviewer_session_id.clone(),
            reviewer_thread_name: self.args.reviewer_thread_name.clone(),
            reviewer_model: self.args.reviewer_model.clone(),
            consecutive_failure_count: self.consecutive_failure_count,
            quality_score: review.map(|r| r.quality_score),
            reviewer_decision: review.map(|r| r.decision.as_str().to_owned()),
            reviewer_rationale: review.map(|r| r.rationale.clone()),
            reviewer_findings: review.and_then(|r| {
                r.findings
                    .as_ref()
                    .and_then(|v| serde_json::to_value(v).ok())
            }),
            reviewer_checks: review.and_then(|r| {
                r.checks_performed
                    .as_ref()
                    .and_then(|v| serde_json::to_value(v).ok())
            }),
            reviewer_verification_commands: review.and_then(|r| {
                r.verification_commands
                    .as_ref()
                    .and_then(|v| serde_json::to_value(v).ok())
            }),
            artifact_paths: self.artifact_paths.clone(),
            commit_hashes: self.commit_hashes.clone(),
            warnings: self.warnings.clone(),
            failures: self.failures.clone(),
            next_action_required: self.next_action_required.clone(),
            last_inbox: self.last_inbox_snapshot.clone(),
            executor_outbox: self.executor_outbox_snapshot.clone(),
            reviewer_outbox: self.reviewer_outbox_snapshot.clone(),
            detail: self.detail.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Main orchestration entry point
// ---------------------------------------------------------------------------

/// Run the full orchestration workflow.
pub async fn run(args: CliArgs) -> FinalReport {
    install_cancel_handler();
    let mut ctx = OrchestratorCtx::new(args);

    // RUN_INIT → validate and enter CONTEXT_PREP
    if let Err(e) = phases::validate_inputs(&mut ctx) {
        ctx.run_state = RunState::RunFailedInvalidInput;
        ctx.failures.push(e.to_string());
        return ctx.build_report();
    }

    if let Err(e) = phases::validate_provider_models(&ctx).await {
        ctx.run_state = RunState::RunFailedInvalidInput;
        ctx.failures.push(e.to_string());
        return ctx.build_report();
    }
    ctx.run_state = RunState::ContextPrep;

    loop {
        if is_cancelled() && !ctx.run_state.is_terminal() {
            ctx.run_state = RunState::RunAborted;
        }

        if ctx.run_state.is_terminal() {
            break;
        }

        let result = step(&mut ctx).await;

        if let Err(e) = result {
            tracing::error!("orchestrator error: {e}");
            ctx.failures.push(e.to_string());
            if !ctx.run_state.is_terminal() {
                ctx.run_state = error_to_run_state(&e);
            }
        }
    }

    ctx.build_report()
}

/// Convert an `OrchestratorError` to the appropriate terminal `RunState`.
fn error_to_run_state(err: &OrchestratorError) -> RunState {
    match err {
        OrchestratorError::InvalidInput { .. } => RunState::RunFailedInvalidInput,
        OrchestratorError::DirtyWorktree { .. } => RunState::RunFailedDirtyWorktree,
        OrchestratorError::SessionBindFailed { .. } => RunState::RunFailedSessionBind,
        OrchestratorError::SessionLocked { .. } => RunState::RunFailedSessionLocked,
        OrchestratorError::ArtifactContract { .. } => RunState::RunFailedProtocol,
        OrchestratorError::ReviewerProtocolViolation { .. } => RunState::RunFailedProtocol,
        // Io, CommandFailed, TransportResetFailed, RequestChangedAbortRetry are infrastructure
        // failures unrelated to agent protocol.
        _ => RunState::RunFailedInternal,
    }
}

/// Execute one FSM step.
async fn step(ctx: &mut OrchestratorCtx) -> OrchestratorResult<()> {
    match ctx.run_state {
        RunState::ContextPrep => phases::do_context_prep(ctx).await,
        RunState::SessionBind => phases::do_session_bind(ctx).await,
        RunState::ExecutorDispatch => phases::do_executor_dispatch(ctx).await,
        RunState::ExecutorMonitor => monitor::do_monitor(ctx, AgentRole::Executor).await,
        RunState::RoundRetryDecide => phases::do_round_retry_decide(ctx),
        RunState::ExecutorOutputCollect => phases::do_executor_output_collect(ctx).await,
        RunState::OrchVerify => phases::do_orch_verify(ctx).await,
        RunState::ReviewerDispatch => phases::do_reviewer_dispatch(ctx).await,
        RunState::ReviewerMonitor => monitor::do_monitor(ctx, AgentRole::Reviewer).await,
        RunState::ReviewerOutputCollect => phases::do_reviewer_output_collect(ctx).await,
        RunState::QualityGate => phases::do_quality_gate(ctx),
        RunState::RoundFeedbackPrep => phases::do_round_feedback_prep(ctx).await,
        state if state.is_terminal() => Ok(()),
        _ => Ok(()), // unreachable in correct FSM
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::OrchestratorError;
    use crate::state::{AgentRole, ProviderKind, RunState};

    #[test]
    fn error_to_run_state_invalid_input() {
        let e = OrchestratorError::InvalidInput {
            field: "x".to_owned(),
            reason: "bad".to_owned(),
        };
        assert_eq!(error_to_run_state(&e), RunState::RunFailedInvalidInput);
    }

    #[test]
    fn error_to_run_state_dirty_worktree() {
        let e = OrchestratorError::DirtyWorktree {
            status: "M file.rs".to_owned(),
        };
        assert_eq!(error_to_run_state(&e), RunState::RunFailedDirtyWorktree);
    }

    #[test]
    fn error_to_run_state_session_bind_failed() {
        let e = OrchestratorError::SessionBindFailed {
            role: AgentRole::Executor,
            provider: ProviderKind::Claude,
            reason: "not found".to_owned(),
        };
        assert_eq!(error_to_run_state(&e), RunState::RunFailedSessionBind);
    }

    #[test]
    fn error_to_run_state_session_locked() {
        let e = OrchestratorError::SessionLocked {
            provider: ProviderKind::Codex,
            session_id: "abc".to_owned(),
        };
        assert_eq!(error_to_run_state(&e), RunState::RunFailedSessionLocked);
    }

    #[test]
    fn error_to_run_state_artifact_contract() {
        let e = OrchestratorError::ArtifactContract {
            contract: "PLAN.md missing".to_owned(),
        };
        assert_eq!(error_to_run_state(&e), RunState::RunFailedProtocol);
    }

    #[test]
    fn error_to_run_state_reviewer_protocol_violation() {
        let e = OrchestratorError::ReviewerProtocolViolation {
            detail: "mutated repo".to_owned(),
        };
        assert_eq!(error_to_run_state(&e), RunState::RunFailedProtocol);
    }

    #[test]
    fn error_to_run_state_infra_errors_map_to_internal() {
        // Io, RequestChangedAbortRetry, TransportResetFailed are infrastructure
        // failures unrelated to agent protocol — they map to RunFailedInternal.
        let io_err = OrchestratorError::Io {
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        assert_eq!(error_to_run_state(&io_err), RunState::RunFailedInternal);

        let retry_err = OrchestratorError::RequestChangedAbortRetry;
        assert_eq!(error_to_run_state(&retry_err), RunState::RunFailedInternal);

        let transport_err = OrchestratorError::TransportResetFailed {
            path: std::path::PathBuf::from("/tmp/x"),
        };
        assert_eq!(
            error_to_run_state(&transport_err),
            RunState::RunFailedInternal
        );
    }
}
