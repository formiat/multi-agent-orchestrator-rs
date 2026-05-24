use serde::{Deserialize, Serialize};

use crate::state::{
    AgentRole, NotionPolicy, ProviderKind, RemoteNetworkPolicy, RunState, WorkflowType,
};

impl ReasonCode {
    /// True when the run completed successfully (quality reached).
    pub fn is_success(self) -> bool {
        matches!(self, Self::DoneQualityReached)
    }
}

// ---------------------------------------------------------------------------
// Canonical reason codes
// ---------------------------------------------------------------------------

/// Terminal run reason codes; serialized as snake_case in the final JSON report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    DoneQualityReached,
    StoppedIrreconcilableDisagreement,
    StoppedServiceCap,
    StoppedProviderAccess,
    StoppedPoisonedSession,
    StoppedSessionLocked,
    StoppedConsecutiveFailureLimit,
    StoppedExternalBlocker,
    StoppedDirtyWorktree,
    FailedSessionBind,
    FailedInvalidInput,
    FailedProtocol,
    CancelledByOperator,
    InternalError,
}

impl std::fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{self:?}"));
        f.write_str(&s)
    }
}

/// Maps a `RunState` to its canonical `ReasonCode`.
pub fn reason_code_for_state(state: RunState) -> ReasonCode {
    match state {
        RunState::RunDone => ReasonCode::DoneQualityReached,
        RunState::RunAborted => ReasonCode::CancelledByOperator,
        RunState::RunFailedInvalidInput => ReasonCode::FailedInvalidInput,
        RunState::RunFailedDirtyWorktree => ReasonCode::StoppedDirtyWorktree,
        RunState::RunFailedSessionBind => ReasonCode::FailedSessionBind,
        RunState::RunFailedServiceCap => ReasonCode::StoppedServiceCap,
        RunState::RunFailedProviderAccess => ReasonCode::StoppedProviderAccess,
        RunState::RunFailedConsecutiveFailureLimit => ReasonCode::StoppedConsecutiveFailureLimit,
        RunState::RunFailedExternalBlocker => ReasonCode::StoppedExternalBlocker,
        RunState::RunFailedIrreconcilableDisagreement => {
            ReasonCode::StoppedIrreconcilableDisagreement
        }
        RunState::RunFailedPoisonedSession => ReasonCode::StoppedPoisonedSession,
        RunState::RunFailedProtocol => ReasonCode::FailedProtocol,
        RunState::RunFailedSessionLocked => ReasonCode::StoppedSessionLocked,
        RunState::RunFailedInternal => ReasonCode::InternalError,
        _ => ReasonCode::InternalError,
    }
}

// ---------------------------------------------------------------------------
// Final report
// ---------------------------------------------------------------------------

/// Machine-readable JSON report emitted to stdout after run completion.
#[derive(Debug, Serialize)]
pub struct FinalReport {
    pub workflow_type: WorkflowType,
    pub state: RunState,
    pub reason_code: ReasonCode,
    pub workspace_root: String,
    pub branch: String,
    pub notion_policy: NotionPolicy,
    pub remote_network_policy: RemoteNetworkPolicy,

    pub executor_provider: Option<ProviderKind>,
    pub executor_session_id: Option<String>,
    pub executor_thread_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor_model: Option<String>,
    pub reviewer_provider: Option<ProviderKind>,
    pub reviewer_session_id: Option<String>,
    pub reviewer_thread_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_model: Option<String>,

    pub consecutive_failure_count: u32,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_decision: Option<String>,
    /// Reviewer's overall rationale for the decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_rationale: Option<String>,
    /// Reviewer-provided findings payload (free-form).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_findings: Option<serde_json::Value>,
    /// Reviewer-provided checks payload (free-form).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_checks: Option<serde_json::Value>,
    /// Reviewer-provided verification commands payload (free-form).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_verification_commands: Option<serde_json::Value>,

    pub artifact_paths: Vec<String>,
    pub commit_hashes: Vec<String>,

    pub warnings: Vec<String>,
    pub failures: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action_required: Option<String>,

    /// Last generated inbox prompt and collected transport outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_inbox: Option<TransportBodyReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor_outbox: Option<TransportBodyReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_outbox: Option<TransportBodyReport>,

    /// Diagnostic extras (provider, blocker, disagreement reasons, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Raw transport file snapshot emitted for maximum observability.
///
/// `body` is an opaque diagnostic blob. The orchestrator may log and report it,
/// but must not make routing decisions by parsing executor outbox content.
#[derive(Debug, Clone, Serialize)]
pub struct TransportBodyReport {
    pub role: AgentRole,
    pub provider: ProviderKind,
    pub session_id: String,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub lines: usize,
    pub utf8_lossy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
    pub body: String,
}

impl FinalReport {
    /// Emit the report as pretty-printed JSON to stdout.
    pub fn print(&self) {
        match serde_json::to_string_pretty(self) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("failed to serialize report: {e}"),
        }
    }
}
