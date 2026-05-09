// ---------------------------------------------------------------------------
// Provider adapter boundary
//
// Orchestration logic must not depend on provider-internal wording beyond the
// hardcoded versioned regex sets in constants.rs.  Each provider module exposes:
//   dispatch(session_id, repo) -> DispatchedProcess
//   session_*_mtime(...)       -> OrchestratorResult<Option<SystemTime>>
//   discover_by_thread(...)    -> OrchestratorResult<String>
//
// Raw-text inspection boundary:
//   ALLOWED  — provider stdout/stderr, provider logs, provider exports, reviewer YAML.
//   FORBIDDEN — executor ./.agent-io/outbox.txt content.
//
// Creating a new provider session/chat is strictly forbidden for all providers.
// If deterministic existing-session bind fails, stop and report RUN_FAILED_SESSION_BIND.
// ---------------------------------------------------------------------------

pub mod claude;
pub mod codex;
pub mod opencode;

use std::path::PathBuf;

use tokio::process::Child;

use crate::errors::OrchestratorResult;
use crate::state::ProviderKind;

/// Apply child lifetime policy to a `tokio::process::Command` before spawning.
///
/// Two mechanisms are combined:
/// - `kill_on_drop(true)`: Tokio sends SIGKILL when the `Child` handle is dropped
///   (covers panics, unhandled errors, and normal script exit).
/// - `PR_SET_PDEATHSIG(SIGTERM)` in `pre_exec`: the kernel sends SIGTERM to the
///   child when the parent exits for *any* reason, including SIGKILL and OOM.
///
/// Both are needed: `kill_on_drop` fires via Rust Drop; `pdeathsig` fires via the
/// kernel regardless of how the parent dies.
pub fn apply_child_death_policy(cmd: &mut tokio::process::Command) {
    cmd.kill_on_drop(true);
    // SAFETY: prctl(2) is documented as async-signal-safe; no allocations or
    // locks are taken. The closure runs after fork(), before exec().
    unsafe {
        cmd.pre_exec(|| {
            nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM)
                .map_err(std::io::Error::from)?;
            Ok(())
        });
    }
}

/// Result of a provider dispatch — the spawned child process and its capture log paths.
pub struct DispatchedProcess {
    pub child: Child,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

/// Build a temporary log file path under the system temp directory.
///
/// value: `(path, suffix)` where suffix identifies the role (e.g. `"stdout"`)
pub fn temp_log_path(prefix: &str) -> PathBuf {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let pid = std::process::id();
    std::env::temp_dir().join(format!("orchestrate_{prefix}_{ts}_{pid}.log"))
}

/// Dispatch the correct provider binary based on `provider`.
pub async fn dispatch(
    provider: ProviderKind,
    session_id: &str,
    repo: &std::path::Path,
) -> OrchestratorResult<DispatchedProcess> {
    match provider {
        ProviderKind::Claude => claude::dispatch(session_id, repo).await,
        ProviderKind::Opencode => opencode::dispatch(session_id, repo).await,
        ProviderKind::Codex => codex::dispatch(session_id, repo).await,
    }
}

/// Read provider stdout and stderr tails.
///
/// Classification is based strictly on provider process diagnostics, not on
/// assistant text (`-o` last-message capture), to avoid false positives from
/// instruction content.
pub async fn read_diagnostics(proc: &DispatchedProcess) -> OrchestratorResult<(String, String)> {
    let stdout = crate::signals::read_log_tail(&proc.stdout_path, 8192).await?;
    let stderr = crate::signals::read_log_tail(&proc.stderr_path, 8192).await?;
    Ok((stdout, stderr))
}
