use chrono::{DateTime, Utc};
use std::time::SystemTime;

use crate::constants::HANG_CONFIRM_SEC;

// ---------------------------------------------------------------------------
// Core enumerations
// ---------------------------------------------------------------------------

/// Which agent role an attempt belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Executor,
    Reviewer,
}

/// Provider backend that can serve as executor or reviewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Claude,
    Opencode,
    Codex,
}

impl ProviderKind {
    /// Returns the canonical lowercase string name for CLI/log use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Codex => "codex",
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProviderKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(Self::Claude),
            "opencode" => Ok(Self::Opencode),
            "codex" => Ok(Self::Codex),
            other => Err(format!(
                "unknown provider '{other}'; expected claude|opencode|codex"
            )),
        }
    }
}

/// Attempt-level state produced by `fsm::classify()` on each probe cycle.
///
/// Classification priority is strict (highest → lowest):
///   1 Cancelled          — operator signal, preempts everything
///   2 FailedServiceCap   — must stop immediately, before generic failure handling
///   3 FailedProviderAccess
///   4 FailedTransport
///   5 Success            — checked before silent/protocol fallback
///   6 SoftSuccess
///   7 FailedProtocol     — only after final-result boundary
///   8 FailedSilent       — process exited, no output, no fresh provider activity
///   9 HangConfirmed      — work stale AND provider log stale
///  10 Finalizing         — outbox is non-empty, waiting for process exit/forced stop
///  11 Running            — any work signal changed since last probe
///  12 WaitingOutput      — alive, no new signals
///  13 Dispatching / Unknown — fallback
///
/// TerminatingStale, RetryPending, RetryExhausted are **not** classifier outputs;
/// they are assigned by routing logic after the classifier returns HangConfirmed
/// or a retriable terminal failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptState {
    /// Process spawned; first probe not yet completed.
    Dispatching,
    /// At least one work signal changed since the previous probe.
    Running,
    /// Process alive (or exited with fresh provider activity) but no required result yet.
    WaitingOutput,
    /// Process finished and all artifact-contract checks passed.
    Success,
    /// Process finished cleanly with only a partial artifact set (workflow-dependent).
    SoftSuccess,
    /// Process ended with no success markers and no service-cap markers.
    FailedSilent,
    /// Quota / rate / capacity limit matched in provider stdout/stderr.
    FailedServiceCap,
    /// Permission / auth / sandbox denial matched in provider stdout/stderr.
    FailedProviderAccess,
    /// IPC / connection / command-wrapper failure.
    FailedTransport,
    /// Required artifact invalid, malformed, or empty after final-result boundary.
    FailedProtocol,
    /// Work signals stale AND provider log stale ≥ HANG_CONFIRM_SEC.
    HangConfirmed,
    /// Outbox is already non-empty; waiting for process natural exit or forced stop timeout.
    Finalizing,
    /// Assigned by routing after HangConfirmed — not a classifier output.
    TerminatingStale,
    /// Assigned by routing after retriable failure — not a classifier output.
    RetryPending,
    /// Assigned by routing when budget exhausted — not a classifier output.
    RetryExhausted,
    /// Operator cancelled the run.
    Cancelled,
    Unknown,
}

/// Run-level states for the orchestrator FSM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    RunInit,
    ContextPrep,
    SessionBind,
    ExecutorDispatch,
    ExecutorMonitor,
    RoundRetryDecide,
    ExecutorOutputCollect,
    OrchVerify,
    ReviewerDispatch,
    ReviewerMonitor,
    ReviewerOutputCollect,
    QualityGate,
    RoundFeedbackPrep,
    RunDone,
    RunAborted,
    RunFailedInvalidInput,
    RunFailedDirtyWorktree,
    RunFailedSessionBind,
    RunFailedServiceCap,
    RunFailedProviderAccess,
    RunFailedConsecutiveFailureLimit,
    RunFailedExternalBlocker,
    RunFailedIrreconcilableDisagreement,
    RunFailedPoisonedSession,
    RunFailedProtocol,
    RunFailedSessionLocked,
    /// IO, subprocess, or transport failures unrelated to agent protocol.
    RunFailedInternal,
}

impl RunState {
    /// True when no further transitions are possible.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::RunDone
                | Self::RunAborted
                | Self::RunFailedInvalidInput
                | Self::RunFailedDirtyWorktree
                | Self::RunFailedSessionBind
                | Self::RunFailedServiceCap
                | Self::RunFailedProviderAccess
                | Self::RunFailedConsecutiveFailureLimit
                | Self::RunFailedExternalBlocker
                | Self::RunFailedIrreconcilableDisagreement
                | Self::RunFailedPoisonedSession
                | Self::RunFailedProtocol
                | Self::RunFailedSessionLocked
                | Self::RunFailedInternal
        )
    }
}

/// Workflow type selected from CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowType {
    Plan,
    Investigate,
    Implement,
}

impl WorkflowType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Investigate => "investigate",
            Self::Implement => "implement",
        }
    }
}

impl std::fmt::Display for WorkflowType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for WorkflowType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "plan" => Ok(Self::Plan),
            "investigate" => Ok(Self::Investigate),
            "implement" => Ok(Self::Implement),
            other => Err(format!(
                "unknown workflow '{other}'; expected plan|investigate|implement"
            )),
        }
    }
}

/// Notion task access policy for this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotionPolicy {
    Required,
    Optional,
}

impl NotionPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
        }
    }
}

/// Policy for remote network actions against remote target systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteNetworkPolicy {
    Forbidden,
    ReadOnly,
    Operational,
}

impl RemoteNetworkPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forbidden => "forbidden",
            Self::ReadOnly => "read_only",
            Self::Operational => "operational",
        }
    }
}

impl std::fmt::Display for RemoteNetworkPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RemoteNetworkPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "forbidden" => Ok(Self::Forbidden),
            "read_only" => Ok(Self::ReadOnly),
            "operational" => Ok(Self::Operational),
            other => Err(format!(
                "unknown remote network policy '{other}'; expected forbidden|read_only|operational"
            )),
        }
    }
}

impl std::fmt::Display for NotionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for NotionPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "required" => Ok(Self::Required),
            "optional" => Ok(Self::Optional),
            other => Err(format!(
                "unknown notion policy '{other}'; expected required|optional"
            )),
        }
    }
}

/// Provider-level error class derived from stdout/stderr pattern matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorClass {
    ServiceCap,
    Permission,
    Auth,
    Transport,
}

/// Which fixed template to render into inbox.txt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateId {
    ExecutorInitial,
    ReviewerReview,
    ReviewerRepairYaml,
    ExecutorFeedback,
}

// ---------------------------------------------------------------------------
// Attempt state data
// ---------------------------------------------------------------------------

/// In-memory state for one active provider attempt.
pub struct AttemptStateData {
    pub state: AttemptState,
    pub role: AgentRole,
    pub dispatch_ts: DateTime<Utc>,
    /// OS PID of the child process, if alive.
    pub pid: Option<u32>,
    /// Exit code recorded after process exit.
    pub exit_code: Option<i32>,
    /// SHA-256 of inbox.txt at dispatch time.
    ///
    /// Must equal the inbox hash when a retry fires.  If inbox changed between
    /// dispatch and retry (concurrent write, partial overwrite) abort the retry
    /// with `RequestChangedAbortRetry` rather than silently reusing a stale request.
    pub request_fingerprint: String,
    pub last_work_signal_ts: Option<DateTime<Utc>>,
    /// Grace deadline waiting for a commit (executor only; never set for reviewer).
    ///
    /// Reviewer must not create dirty project files.  Grace periods exist solely
    /// for executor finalization windows.
    pub grace_until_commit: Option<DateTime<Utc>>,
    /// Grace deadline waiting for outbox after commit appeared (executor only).
    pub grace_until_outbox: Option<DateTime<Utc>>,
    /// Outbox mtime captured immediately before reviewer dispatch.
    ///
    /// Stored in the **reviewer** attempt (not the preceding executor attempt) so
    /// it is readable in `do_reviewer_output_collect`.  If mtime is unchanged
    /// after reviewer exits the reviewer never wrote YAML → FAILED_SILENT.
    pub pre_reviewer_outbox_mtime: Option<SystemTime>,
    /// Timestamp for next probe cycle.
    pub next_probe_at: DateTime<Utc>,
    /// SHA-256 of `git status --short` output captured at dispatch time; baseline for dirty-state detection.
    pub dispatch_git_status_hash: String,
    /// SHA-256 of `git status --short` from the most recent probe; used to detect inter-probe changes.
    pub prev_probe_git_status_hash: String,
    /// Parsed lines from `git status --short` at the most recent probe.
    ///
    /// Used for human-readable `git-delta` logs (`newly_changed` / `resolved`).
    pub prev_probe_git_status_lines: std::collections::BTreeSet<String>,
    /// `git rev-parse HEAD` captured at dispatch time; baseline for new-commit detection.
    ///
    /// Detects the case where the agent committed (worktree clean) but has not yet written
    /// outbox — `git_status_hash` returns to its dispatch value, but HEAD has advanced.
    pub dispatch_git_head_hash: String,
    /// `git rev-parse HEAD` from the most recent probe; used to detect inter-probe commits.
    pub prev_probe_git_head_hash: String,
    /// Outbox metadata observed at the most recent probe; `None` if outbox did not exist.
    pub prev_probe_outbox_meta: Option<FileMeta>,
    /// Provider log file mtime observed at the most recent probe; `None` if the log is absent.
    pub prev_probe_log_mtime: Option<std::time::SystemTime>,
    /// `true` once `provider_log_mtime` has returned `Some` at least once for this attempt.
    ///
    /// Used to distinguish "log not yet created" (normal at startup) from
    /// "log disappeared after being accessible" (anomaly worth warning about).
    pub provider_log_ever_seen: bool,
    /// Timestamp of the last emitted heartbeat log line; `None` before the first heartbeat.
    pub last_heartbeat_ts: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Probe snapshot — classifier input
// ---------------------------------------------------------------------------

/// All signals available to `fsm::classify()` on a single probe cycle.
///
/// Grace deadlines are copied from `AttemptStateData` before the classifier runs.
///
/// ## Signal taxonomy
///
/// **Work signals** — executor may still be actively working:
/// - live provider batch process (`process_alive`);
/// - live child verification/build processes spawned by executor (`has_live_child_processes`
///   checks cargo, rustc, pytest, etc. — their presence suppresses hang detection during
///   build/test phases);
/// - fresh provider log mtime (session JSONL for Claude/Codex, SQLite for OpenCode).
///
/// **Result signals** — active agent produced or is producing output:
/// - non-empty outbox.txt (size/mtime only; orchestrator must NOT read content);
/// - dirty project files;
/// - local commits;
/// - required artifact files (PLAN.md, INVESTIGATION.md).
///
/// **Terminal diagnostics** — explain why an attempt cannot continue normally:
/// - service-cap, permission/auth, transport failure markers in provider stdout/stderr.
///
/// Result signals must NOT be treated as proof of ongoing work.  Dirty files,
/// commits, outbox, and artifacts route to output collection or grace-period
/// handling, not to `Running`.
pub struct ProbeSnapshot {
    pub cancelled: bool,
    pub process_exited: bool,
    pub process_alive: bool,
    /// Non-empty outbox detected by metadata only (no content inspection).
    pub outbox_present: bool,
    pub provider_error: Option<ProviderErrorClass>,
    pub success_contract: bool,
    pub soft_success_contract: bool,
    /// Reviewer YAML malformed/missing — evaluated only after final-result boundary.
    pub protocol_invalid: bool,
    /// Provider-side activity is fresh despite process having exited.
    ///
    /// Keeps the attempt in WaitingOutput when the provider is still computing
    /// server-side after the local batch process exited (e.g. Claude streaming).
    pub provider_activity_after_exit_is_fresh: bool,
    /// At least one work signal changed since the previous probe.
    pub work_signals_changed: bool,
    /// Timestamp of the most recent work signal.
    pub last_work_signal_ts: Option<DateTime<Utc>>,
    /// Provider session log has not been updated within `HANG_CONFIRM_SEC` (condition B).
    ///
    /// Computed inline each cycle as `provider_log_mtime.elapsed() >= HANG_CONFIRM_SEC`.
    /// Used together with work-signal staleness to prevent false positives from
    /// local silence while the provider is still active server-side.
    pub provider_stale: bool,
    /// Active grace deadline copied from `AttemptStateData` for this probe cycle.
    pub grace_deadline: Option<DateTime<Utc>>,
    /// `now − dispatch_ts ≥ HANG_MAX_WITH_WORK_SIGNALS_SEC` (work-signal ceiling exceeded).
    pub hang_max_with_work_signals_exceeded: bool,
}

impl ProbeSnapshot {
    /// True when the provider is alive but both local work signals and provider log are stale.
    pub fn hang_confirmed(&self, now: DateTime<Utc>) -> bool {
        self.process_alive
            && self
                .last_work_signal_ts
                .map(|t| (now - t).num_seconds().max(0) as u64 >= HANG_CONFIRM_SEC)
                .unwrap_or(false)
            && self.provider_stale
    }

    /// True when the attempt has definitively passed the point where more output can arrive.
    pub fn final_result_boundary_reached(&self, now: DateTime<Utc>) -> bool {
        if let Some(deadline) = self.grace_deadline {
            if now < deadline {
                return false;
            }
        }
        if self.process_exited && !self.provider_activity_after_exit_is_fresh {
            return true;
        }
        if let Some(deadline) = self.grace_deadline {
            if now >= deadline {
                return true;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Signal data structs
// ---------------------------------------------------------------------------

/// Filesystem metadata snapshot used for outbox change detection.
#[derive(Debug, Clone, PartialEq)]
pub struct FileMeta {
    pub size: u64,
    pub mtime: std::time::SystemTime,
}

/// Probe results collected every `PROBE_INTERVAL_SEC`.
pub struct ProbeSignals {
    pub process_alive: bool,
    /// `None` when the outbox file does not exist.
    pub outbox_meta: Option<FileMeta>,
    /// SHA-256 hex of `git status --short` output — changes when worktree changes.
    pub git_status_hash: String,
    /// Raw `git status --short` output from the current probe.
    pub git_status_short: String,
    /// Raw `git rev-parse HEAD` output — changes when a new commit is created.
    pub git_head_hash: String,
    /// key: artifact relative path (e.g. "PLAN.md"), value: exists and non-empty
    pub artifact_map: std::collections::HashMap<String, bool>,
    /// mtime of the provider-specific session log file; `None` when the log is absent.
    pub provider_log_mtime: Option<std::time::SystemTime>,
    /// True when the agent process has live (non-zombie) descendant processes.
    ///
    /// Live descendants (`cargo`, `rustc`, `clippy-driver`, `make`, `pytest`, etc.)
    /// are work signals: their presence suppresses hang detection during build/test phases.
    pub has_child_processes: bool,
}

// ---------------------------------------------------------------------------
// Template values
// ---------------------------------------------------------------------------

/// Placeholder values for fixed prompt template substitution.
pub struct TemplateValues {
    pub workflow_type: WorkflowType,
    pub workspace_root: String,
    pub transport_dir: String,
    pub inbox_path: String,
    pub outbox_path: String,
    pub orchestrator_docs_dir: String,
    pub branch: String,
    pub user_prompt: String,
    pub notion_policy: NotionPolicy,
    pub remote_network_policy: RemoteNetworkPolicy,
    pub workflow_contract: String,
    pub git_facts: String,
    /// True when executor wrote a non-empty outbox (false in soft-success scenarios).
    pub executor_outbox_present: bool,
    /// Full YAML schema block embedded in reviewer prompts.
    pub reviewer_yaml_schema: Option<String>,
    /// Exact parser/validator error from the previous rejected reviewer YAML.
    pub reviewer_yaml_rejection: Option<String>,
    /// Raw YAML from the previous reviewer round (feedback template only).
    pub review_result_yaml: Option<String>,
    /// Numbered feedback items for the executor (feedback template only).
    pub feedback_for_executor: Option<String>,
    /// Optional executor-only local run wrapper instruction.
    pub runlim_rule: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn make_snapshot(
        process_alive: bool,
        last_work_signal_ts: Option<DateTime<Utc>>,
        provider_stale: bool,
    ) -> ProbeSnapshot {
        ProbeSnapshot {
            cancelled: false,
            process_exited: !process_alive,
            process_alive,
            outbox_present: false,
            provider_error: None,
            success_contract: false,
            soft_success_contract: false,
            protocol_invalid: false,
            provider_activity_after_exit_is_fresh: false,
            work_signals_changed: false,
            last_work_signal_ts,
            provider_stale,
            grace_deadline: None,
            hang_max_with_work_signals_exceeded: false,
        }
    }

    #[test]
    fn remote_network_policy_parses_supported_modes_only() {
        assert_eq!(
            "forbidden".parse::<RemoteNetworkPolicy>().unwrap(),
            RemoteNetworkPolicy::Forbidden
        );
        assert_eq!(
            "read_only".parse::<RemoteNetworkPolicy>().unwrap(),
            RemoteNetworkPolicy::ReadOnly
        );
        assert_eq!(
            "operational".parse::<RemoteNetworkPolicy>().unwrap(),
            RemoteNetworkPolicy::Operational
        );
        assert!("allowed".parse::<RemoteNetworkPolicy>().is_err());
    }

    #[test]
    fn hang_confirmed_requires_alive_and_stale() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 1, 0, 0).unwrap(); // +1h
        let stale_ts = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(); // 3600s ago

        let snap = make_snapshot(true, Some(stale_ts), true);
        assert!(snap.hang_confirmed(now));

        // Dead process should not be confirmed as a hang.
        let snap_dead = make_snapshot(false, Some(stale_ts), false);
        assert!(!snap_dead.hang_confirmed(now));
    }

    #[test]
    fn hang_not_confirmed_when_fresh() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 4, 0).unwrap(); // +4 min
        let fresh_ts = Utc.with_ymd_and_hms(2026, 1, 1, 0, 3, 0).unwrap(); // 1 min ago

        let snap = make_snapshot(true, Some(fresh_ts), true);
        assert!(!snap.hang_confirmed(now));
    }

    #[test]
    fn hang_confirmed_requires_both_conditions() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 1, 0, 0).unwrap();
        let stale_ts = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

        // Work signals stale but provider log fresh → not confirmed
        let snap_provider_fresh = make_snapshot(true, Some(stale_ts), false);
        assert!(!snap_provider_fresh.hang_confirmed(now));

        let snap_confirmed = make_snapshot(true, Some(stale_ts), true);
        assert!(snap_confirmed.hang_confirmed(now));
    }

    #[test]
    fn final_result_boundary_on_exited_process() {
        let now = Utc::now();
        let snap = ProbeSnapshot {
            cancelled: false,
            process_exited: true,
            process_alive: false,
            outbox_present: false,
            provider_error: None,
            success_contract: false,
            soft_success_contract: false,
            protocol_invalid: false,
            provider_activity_after_exit_is_fresh: false,
            work_signals_changed: false,
            last_work_signal_ts: None,
            provider_stale: false,
            grace_deadline: None,
            hang_max_with_work_signals_exceeded: false,
        };
        assert!(snap.final_result_boundary_reached(now));
    }

    #[test]
    fn final_result_boundary_not_reached_during_active_grace() {
        let now = Utc::now();
        let snap = ProbeSnapshot {
            cancelled: false,
            process_exited: true,
            process_alive: false,
            outbox_present: false,
            provider_error: None,
            success_contract: false,
            soft_success_contract: false,
            protocol_invalid: false,
            provider_activity_after_exit_is_fresh: false,
            work_signals_changed: false,
            last_work_signal_ts: None,
            provider_stale: false,
            grace_deadline: Some(now + chrono::Duration::seconds(30)),
            hang_max_with_work_signals_exceeded: false,
        };
        assert!(!snap.final_result_boundary_reached(now));
    }

    #[test]
    fn final_result_boundary_not_reached_while_running() {
        let now = Utc::now();
        let snap = ProbeSnapshot {
            cancelled: false,
            process_exited: false,
            process_alive: true,
            outbox_present: false,
            provider_error: None,
            success_contract: false,
            soft_success_contract: false,
            protocol_invalid: false,
            provider_activity_after_exit_is_fresh: false,
            work_signals_changed: true,
            last_work_signal_ts: Some(now),
            provider_stale: false,
            grace_deadline: None,
            hang_max_with_work_signals_exceeded: false,
        };
        assert!(!snap.final_result_boundary_reached(now));
    }
}
