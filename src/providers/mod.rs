// ---------------------------------------------------------------------------
// Provider adapter boundary
//
// Orchestration logic must not depend on provider-internal wording beyond the
// hardcoded versioned regex sets in constants.rs.  Each provider module exposes:
//   dispatch(session_id, repo, trigger_prompt, model) -> DispatchedProcess
//   session_*_mtime(...)       -> OrchestratorResult<Option<SystemTime>>
//   discover_by_thread(...)    -> OrchestratorResult<String>
//
// Raw-text inspection boundary:
//   ALLOWED  — provider stdout/stderr, provider logs, provider exports, reviewer YAML.
//   FORBIDDEN — executor outbox content.
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

/// Disable proptest regression-file persistence for all agent-spawned commands.
///
/// Agents inherit this env var, so their `cargo test` invocations do not create
/// `*.proptest-regressions` files in the target workspace as an orchestration side effect.
pub const PROPTEST_DISABLE_FAILURE_PERSISTENCE_ENV: (&str, &str) =
    ("PROPTEST_DISABLE_FAILURE_PERSISTENCE", "1");

pub fn apply_agent_env(cmd: &mut tokio::process::Command) {
    let (key, value) = PROPTEST_DISABLE_FAILURE_PERSISTENCE_ENV;
    cmd.env(key, value);
    if let Some(path) = enriched_path_for_agents() {
        cmd.env("PATH", path);
    }
}

fn enriched_path_for_agents() -> Option<String> {
    let current = std::env::var("PATH").ok().unwrap_or_default();
    let home = std::env::var("HOME").ok();
    let mut entries = Vec::new();
    if let Some(home) = home {
        entries.push(format!("{home}/.local/bin"));
    }
    entries.push("/usr/local/bin".to_owned());
    if !current.is_empty() {
        entries.push(current);
    }
    if entries.is_empty() {
        None
    } else {
        Some(entries.join(":"))
    }
}

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
    trigger_prompt: &str,
    model: Option<&str>,
) -> OrchestratorResult<DispatchedProcess> {
    match provider {
        ProviderKind::Claude => claude::dispatch(session_id, repo, trigger_prompt).await,
        ProviderKind::Opencode => opencode::dispatch(session_id, repo, trigger_prompt, model).await,
        ProviderKind::Codex => codex::dispatch(session_id, repo, trigger_prompt).await,
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
