/// Transport directory relative to workspace root.
pub const TRANSPORT_DIR: &str = ".agent-io";
/// Inbox file within the transport directory.
pub const INBOX_FILE: &str = "inbox.txt";
/// Outbox file within the transport directory.
pub const OUTBOX_FILE: &str = "outbox.txt";

// ---------------------------------------------------------------------------
// Runtime constants — all hardcoded, no runtime config file.
//
// The first implementation must not have a second configuration surface that
// can drift.  Every timing and policy knob is a named constant
// here; changing policy means changing source and recompiling.
// ---------------------------------------------------------------------------

/// How often the monitor loop probes process state (seconds).
///
/// The orchestrator is a local process; cheap polling does not consume model
/// tokens.  Timing is optimised for machine orchestration, not manual
/// supervision.  There is no initial cooldown after dispatch — the first probe
/// runs immediately and establishes the monitoring baseline.
///
/// The original three-tier probe model (local/provider/export) was collapsed to
/// one uniform loop because all probes are local file/process checks with
/// negligible cost — there is no export-tier remote call in this implementation.
pub const PROBE_INTERVAL_SEC: u64 = 1;

/// How often to emit a heartbeat log line (seconds).
///
/// Heartbeats confirm the monitor loop is still alive during long WAITING_OUTPUT
/// stretches without being log-noisy on every 10-second cycle.
pub const HEARTBEAT_INTERVAL_SEC: u64 = 300;

/// Work signals stale AND provider log stale for this many seconds → HANG_CONFIRMED.
///
/// "Work signals" are: live process, changing outbox size/mtime, changing git
/// status, live child processes (cargo, rustc, pytest, …).
pub const HANG_CONFIRM_SEC: u64 = 420;

/// Hard ceiling from dispatch: work signals may change but no result appears.
///
/// If work signals keep changing beyond this limit the orchestrator still runs
/// one provider-log confirmation probe.  If that confirms stale activity,
/// classify as HANG_CONFIRMED to prevent an unbounded live-but-unproductive run.
pub const HANG_MAX_WITH_WORK_SIGNALS_SEC: u64 = 2400;

/// Finalization window waiting for a commit to appear after dirty files first seen.
///
/// Grace periods are finalization windows, not polling throttles.  The monitor
/// loop continues at PROBE_INTERVAL_SEC while a grace deadline is active.
/// Sequence: dirty files appear → start commit_grace → commit appears → start
/// outbox_grace → outbox appears → EXECUTOR_OUTPUT_COLLECT.
pub const COMMIT_GRACE_SEC: u64 = 180;

/// Finalization window waiting for outbox after a commit appeared.
pub const OUTBOX_GRACE_SEC: u64 = 180;

/// Maximum time to keep a still-alive agent process after non-empty outbox write.
///
/// Once outbox is non-empty the attempt enters `Finalizing`. If the process is still
/// alive `OUTBOX_FINALIZING_KILL_SEC` after outbox mtime, orchestrator force-stops it.
pub const OUTBOX_FINALIZING_KILL_SEC: u64 = 60;

/// Maximum consecutive orchestration/provider failures before stopping the run.
///
/// Only infra failures count: crashes, silent exits, confirmed hangs, selected
/// transport failures.  Reviewer `decision: revise` is a successful semantic
/// round and does NOT increment this counter.  The revise→executor→reviewer
/// loop is intentionally unbounded; the reviewer is responsible for returning
/// accept/blocked/poisoned_session when revisions stop being productive.
/// The counter resets to 0 after any complete successful semantic round.
pub const CONSECUTIVE_FAILURE_LIMIT: u32 = 5;

/// Minimum reviewer quality score (0–10) required to accept the round.
pub const MIN_ACCEPT_SCORE: f64 = 8.0;

/// Mandatory unconditional pause after every completed phase transition (seconds).
///
/// Applied after EXECUTOR_OUTPUT_COLLECT, REVIEWER_OUTPUT_COLLECT,
/// ROUND_FEEDBACK_PREP, and SESSION_BIND before the next phase starts.
/// Eliminates false FAILED_SILENT caused by sub-second mtime identity when
/// the reviewer rewrites outbox.txt immediately after dispatch.  Cheap probes
/// within a phase are not affected.
pub const PHASE_SEPARATOR_WAIT_SEC: u64 = 5;

/// Seconds to wait for a child to exit after SIGTERM before escalating to SIGKILL.
pub const CANCEL_CHILD_WAIT_SEC: u64 = 5;

/// Build the minimal trigger prompt sent to provider CLI.
///
/// The full task is in `inbox_path`; keep these paths absolute to prevent agents
/// from resolving `.agent-io` relative to the orchestrator repository.
pub fn trigger_prompt(inbox_path: &std::path::Path, outbox_path: &std::path::Path) -> String {
    format!(
        "Read the inbox at absolute path `{}` right now, even if you think you already know its contents, and follow it exactly. \
         Write the required result to the outbox at absolute path `{}`.",
        inbox_path.display(),
        outbox_path.display()
    )
}

// ---------------------------------------------------------------------------
// Provider error signature catalog
// ---------------------------------------------------------------------------

/// Service cap / quota patterns applied to provider stdout/stderr.
pub const SERVICE_CAP_PATTERNS: &[&str] = &[
    r"(?i)usage limit (reached|exceeded|hit)",
    r"(?i)reached your usage limit",
    r"(?i)hit your usage limit",
    r"(?i)you'?ve hit your limit",
    r"(?i)rate limit exceeded",
    r"(?i)quota exceeded",
    r"(?i)\bhttp\s*429\b",
    r"(?i)\b429\b.{0,40}(rate limit|too many requests|quota)",
    r"(?i)(selected model is at capacity|server[_ ]overloaded)",
];

/// Permission / sandbox patterns.
pub const PERMISSION_PATTERNS: &[&str] = &[
    r"(?i)permission denied",
    r"(?i)sandbox.{0,40}(denied|forbidden|blocked|violation)",
    r"(?i)policy violation",
];

/// Network / connection transport patterns.
pub const TRANSPORT_PATTERNS: &[&str] = &[
    r"(?i)(connect|read|request)timeout",
    r"(?i)(connect|connection|read|request|operation).{0,24}timed out",
    r"(?i)connection reset",
    r"(?i)broken pipe",
    r"(?i)connection refused",
    r"(?i)connectionrefused",
    r"(?i)network unreachable",
    r"(?i)\b(http\s*)?50[234]\b.{0,32}(bad gateway|service unavailable|gateway timeout)?",
    // Claude api_error cause codes surfacing in stderr
    r"(?i)failed to open socket",
    r"(?i)failedtoopensocket",
];

/// Auth / credential patterns.
pub const AUTH_PATTERNS: &[&str] = &[
    r"(?i)\b401\b.{0,40}(unauthorized|invalid|auth|token|key|credential)",
    r"(?i)\bunauthorized\b.{0,40}(api|token|key|credential|request|access|auth)|\b(auth|api).{0,40}unauthorized\b",
    r"(?i)\b403\b.{0,40}(forbidden|access denied|auth|token|api)|\baccess denied\b",
    r"(?i)\bforbidden\b.{0,40}(api|token|credential|auth|request|access)|\b(api|auth|token|credential).{0,40}\bforbidden\b",
    r#"(?i)apierror.{0,160}statuscode\"?\s*:?\s*401"#,
    r"(?i)invalid.{0,20}(api[-_ ]?key|token|credential|credentials)",
    r"(?i)authentication failed",
];

// ---------------------------------------------------------------------------
// Compiled regex catalog (lazy-initialized)
// ---------------------------------------------------------------------------

use regex::RegexSet;
use std::sync::OnceLock;

/// Lazily compiled regex sets for provider error classification.
pub struct RegexCatalog {
    pub service_cap: RegexSet,
    pub permission: RegexSet,
    pub transport: RegexSet,
    pub auth: RegexSet,
}

/// Returns a reference to the global `RegexCatalog`, initializing on first call.
pub fn regex_catalog() -> &'static RegexCatalog {
    static CATALOG: OnceLock<RegexCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| RegexCatalog {
        service_cap: RegexSet::new(SERVICE_CAP_PATTERNS).expect("service_cap regex"),
        permission: RegexSet::new(PERMISSION_PATTERNS).expect("permission regex"),
        transport: RegexSet::new(TRANSPORT_PATTERNS).expect("transport regex"),
        auth: RegexSet::new(AUTH_PATTERNS).expect("auth regex"),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn trigger_prompt_uses_absolute_transport_paths() {
        let prompt = super::trigger_prompt(
            Path::new("/repo/.agent-io/inbox.txt"),
            Path::new("/repo/.agent-io/outbox.txt"),
        );

        assert!(prompt.contains("/repo/.agent-io/inbox.txt"));
        assert!(prompt.contains("/repo/.agent-io/outbox.txt"));
        assert!(prompt.contains("right now"));
        assert!(prompt.contains("even if you think you already know its contents"));
        assert!(!prompt.contains("./.agent-io"));
    }
}
