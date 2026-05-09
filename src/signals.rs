use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use crate::constants::regex_catalog;
use crate::errors::OrchestratorResult;
use crate::state::{ProbeSignals, ProviderErrorClass, ProviderKind};
use crate::transport::{file_meta, sha256_hex};

// ---------------------------------------------------------------------------
// Probe signals
//
// Executor outbox opacity rule (architectural invariant):
//   The monitor may check outbox.txt EXISTENCE, BYTE SIZE, and MTIME only.
//   The orchestrator may additionally snapshot the raw executor outbox body for
//   diagnostic logging and the final JSON report. It must NOT grep, parse,
//   classify, summarize, semantically inspect, or route based on that body.
//   All semantic use of executor outbox belongs to the reviewer agent. Provider
//   stdout/stderr, provider logs, and reviewer YAML are orchestration inputs and
//   may be inspected.
// ---------------------------------------------------------------------------

/// Collect probe signals for the active attempt.
pub async fn collect_probe_signals(
    repo: impl AsRef<Path>,
    pid: Option<u32>,
    workflow_artifacts: &[&str],
    provider: ProviderKind,
    session_id: &str,
) -> OrchestratorResult<ProbeSignals> {
    let repo = repo.as_ref();

    let process_alive = pid.map(is_process_alive).unwrap_or(false);
    let has_child_processes = pid.map(has_live_child_processes).unwrap_or(false);

    let outbox_meta = file_meta(
        repo.join(crate::constants::TRANSPORT_DIR)
            .join(crate::constants::OUTBOX_FILE),
    )
    .await?;

    let (git_status_hash, git_status_short) = git_status_snapshot(repo).await?;
    let git_head_hash = git_head_hash(repo).await?;

    let artifact_map = probe_artifacts(repo, workflow_artifacts).await?;

    let provider_log_mtime = provider_log_mtime(provider, session_id, repo).await?;

    Ok(ProbeSignals {
        process_alive,
        outbox_meta,
        git_status_hash,
        git_status_short,
        git_head_hash,
        artifact_map,
        provider_log_mtime,
        has_child_processes,
    })
}

/// Return the mtime of the provider-specific session activity signal.
///
/// For Claude and Codex: synchronous filesystem stat of the session JSONL file.
/// For OpenCode: async `opencode session list` call scoped to `session_id` so that
/// activity from unrelated OpenCode sessions does not keep the staleness signal fresh.
///
/// Missing provider logs return `Ok(None)`. Provider signal read failures
/// (permission errors, malformed CLI output, failed signal commands) are fatal and
/// propagate as orchestration errors.
async fn provider_log_mtime(
    provider: ProviderKind,
    session_id: &str,
    repo: &Path,
) -> OrchestratorResult<Option<SystemTime>> {
    match provider {
        ProviderKind::Claude => crate::providers::claude::session_jsonl_mtime(repo, session_id),
        ProviderKind::Codex => crate::providers::codex::session_jsonl_mtime(session_id),
        ProviderKind::Opencode => crate::providers::opencode::session_mtime(session_id, repo).await,
    }
}

/// Returns `true` when the OS process with `pid` is alive.
pub fn is_process_alive(pid: u32) -> bool {
    procfs::process::Process::new(pid as i32).is_ok()
}

/// Returns `true` when process `pid` has at least one live (non-zombie) descendant.
///
/// Live descendants — `cargo`, `rustc`, `clippy-driver`, `make`, `pytest`, etc. — are
/// work signals: their presence suppresses hang detection during build/test phases.
/// Uses a depth-first walk capped at four levels to cover agent → cargo → rustc → linker
/// without scanning unbounded process trees.
pub fn has_live_child_processes(pid: u32) -> bool {
    const MAX_DEPTH: u8 = 4;
    let mut queue: Vec<(u32, u8)> = vec![(pid, 0)];
    let mut visited = std::collections::HashSet::new();
    visited.insert(pid);

    while let Some((current, depth)) = queue.pop() {
        let Ok(proc) = procfs::process::Process::new(current as i32) else {
            continue;
        };
        let Ok(tasks) = proc.tasks() else { continue };
        for task in tasks.flatten() {
            let Ok(children) = task.children() else {
                continue;
            };
            for child_pid in children {
                if !visited.insert(child_pid) {
                    continue;
                }
                // Non-zombie, non-dead child → active work is happening.
                if let Ok(child_proc) = procfs::process::Process::new(child_pid as i32) {
                    if let Ok(stat) = child_proc.stat() {
                        if stat.state != 'Z' && stat.state != 'X' {
                            return true;
                        }
                    }
                }
                if depth < MAX_DEPTH {
                    queue.push((child_pid, depth + 1));
                }
            }
        }
    }
    false
}

/// Returns `(sha256, raw_output)` for `git status --short`.
async fn git_status_snapshot(repo: &Path) -> OrchestratorResult<(String, String)> {
    let status = crate::sessions::run_git(repo, &["status", "--short"]).await?;
    let hash = sha256_hex(status.as_bytes());
    Ok((hash, status))
}

/// Returns the trimmed output of `git rev-parse HEAD`.
pub(crate) async fn git_head_hash(repo: &Path) -> OrchestratorResult<String> {
    Ok(crate::sessions::run_git(repo, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_owned())
}

/// Check which required artifact files exist and are non-empty.
async fn probe_artifacts(repo: &Path, names: &[&str]) -> OrchestratorResult<HashMap<String, bool>> {
    let mut map = HashMap::new();
    for name in names {
        let path = repo.join(name);
        let present = match tokio::fs::metadata(&path).await {
            Ok(m) => m.len() > 0,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        map.insert((*name).to_owned(), present);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Provider error classification
// ---------------------------------------------------------------------------

/// Detailed provider-error match used for diagnostics and report clarity.
pub struct ProviderErrorMatch {
    pub class: ProviderErrorClass,
    pub pattern: &'static str,
    pub fragment: String,
}

/// Classify provider stdout/stderr and return the matched pattern + text fragment.
pub fn classify_provider_error_with_match(text: &str) -> Option<ProviderErrorMatch> {
    let catalog = regex_catalog();
    if let Some(pattern) = first_matching_pattern(
        text,
        &catalog.service_cap,
        crate::constants::SERVICE_CAP_PATTERNS,
    ) {
        return Some(ProviderErrorMatch {
            class: ProviderErrorClass::ServiceCap,
            fragment: matched_fragment(text, pattern),
            pattern,
        });
    }
    if let Some(pattern) = first_matching_pattern(
        text,
        &catalog.permission,
        crate::constants::PERMISSION_PATTERNS,
    ) {
        return Some(ProviderErrorMatch {
            class: ProviderErrorClass::Permission,
            fragment: matched_fragment(text, pattern),
            pattern,
        });
    }
    if let Some(pattern) =
        first_matching_pattern(text, &catalog.auth, crate::constants::AUTH_PATTERNS)
    {
        return Some(ProviderErrorMatch {
            class: ProviderErrorClass::Auth,
            fragment: matched_fragment(text, pattern),
            pattern,
        });
    }
    if let Some(pattern) = first_matching_pattern(
        text,
        &catalog.transport,
        crate::constants::TRANSPORT_PATTERNS,
    ) {
        return Some(ProviderErrorMatch {
            class: ProviderErrorClass::Transport,
            fragment: matched_fragment(text, pattern),
            pattern,
        });
    }
    None
}

fn first_matching_pattern<'a>(
    text: &str,
    set: &regex::RegexSet,
    patterns: &'a [&'static str],
) -> Option<&'a str> {
    let idx = set.matches(text).iter().next()?;
    patterns.get(idx).copied()
}

fn matched_fragment(text: &str, pattern: &str) -> String {
    let Ok(re) = regex::Regex::new(pattern) else {
        return String::new();
    };
    let Some(m) = re.find(text) else {
        return String::new();
    };
    text[m.start()..m.end()].to_owned()
}

/// Read up to `max_bytes` from the tail of a log file without loading the whole file.
///
/// Opens the file, seeks to `max(0, file_len - max_bytes)`, then reads to EOF.
/// Returns an empty string when the file is empty.
pub async fn read_log_tail(path: impl AsRef<Path>, max_bytes: u64) -> OrchestratorResult<String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path.as_ref()).await?;
    let meta = f.metadata().await?;
    if meta.len() == 0 {
        return Ok(String::new());
    }
    let start = meta.len().saturating_sub(max_bytes);
    f.seek(std::io::SeekFrom::Start(start)).await?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Build git facts string for prompt injection.
///
/// When `initial_head` is provided, appends `commits_since_dispatch` and `new_commits`
/// computed from the `initial_head..HEAD` range.
pub async fn format_git_facts(
    repo: impl AsRef<Path>,
    initial_head: Option<&str>,
) -> OrchestratorResult<String> {
    let repo = repo.as_ref();
    let status = crate::sessions::run_git(repo, &["status", "--short"]).await?;
    let status = if status.trim().is_empty() {
        "(clean)"
    } else {
        status.trim()
    };
    let head = crate::sessions::run_git(repo, &["log", "-1", "--oneline"]).await?;

    let mut out = format!("git_status: {status}\ngit_head: {head}", head = head.trim(),);

    if let Some(base) = initial_head {
        let range = format!("{base}..HEAD");
        let log = crate::sessions::run_git(repo, &["log", "--oneline", &range]).await?;
        let commits: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
        out.push_str(&format!("\ncommits_since_dispatch: {}", commits.len()));
        if commits.is_empty() {
            out.push_str("\nnew_commits: []");
        } else {
            out.push_str("\nnew_commits:");
            for c in &commits {
                out.push_str(&format!("\n  - {c}"));
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_service_cap() {
        // Claude: "usage limit reached"
        assert_eq!(
            classify_provider_error_with_match("Error: Usage limit reached for your account")
                .map(|m| m.class),
            Some(ProviderErrorClass::ServiceCap)
        );
        // Codex: "You've hit your usage limit" (no "reached")
        assert_eq!(
            classify_provider_error_with_match("You've hit your usage limit. Upgrade to Pro.")
                .map(|m| m.class),
            Some(ProviderErrorClass::ServiceCap)
        );
        // Claude synthetic API error: "You've hit your limit" (without "usage")
        assert_eq!(
            classify_provider_error_with_match("You've hit your limit · resets 2:10pm")
                .map(|m| m.class),
            Some(ProviderErrorClass::ServiceCap)
        );
        assert_eq!(
            classify_provider_error_with_match("HTTP 429: rate limit exceeded").map(|m| m.class),
            Some(ProviderErrorClass::ServiceCap)
        );
        // Codex server_overloaded: "Selected model is at capacity"
        assert_eq!(
            classify_provider_error_with_match(
                "Selected model is at capacity. Please try a different model."
            )
            .map(|m| m.class),
            Some(ProviderErrorClass::ServiceCap)
        );
    }

    #[test]
    fn classify_auth() {
        assert_eq!(
            classify_provider_error_with_match("401 Unauthorized: invalid API key")
                .map(|m| m.class),
            Some(ProviderErrorClass::Auth)
        );
        // OpenCode DB/API payload style
        assert_eq!(
            classify_provider_error_with_match(
                r#"{"name":"APIError","data":{"message":"Invalid API key.","statusCode":401}}"#
            )
            .map(|m| m.class),
            Some(ProviderErrorClass::Auth)
        );
    }

    #[test]
    fn classify_transport() {
        assert_eq!(
            classify_provider_error_with_match("connection reset by peer").map(|m| m.class),
            Some(ProviderErrorClass::Transport)
        );
        // HTTP 502 gateway error
        assert_eq!(
            classify_provider_error_with_match("502 Bad Gateway").map(|m| m.class),
            Some(ProviderErrorClass::Transport)
        );
        // Claude api_error cause code in stderr
        assert_eq!(
            classify_provider_error_with_match("Failed to open socket to api.anthropic.com")
                .map(|m| m.class),
            Some(ProviderErrorClass::Transport)
        );
        // Claude JSONL api_error cause code style
        assert_eq!(
            classify_provider_error_with_match(
                r#""subtype":"api_error","cause":{"code":"FailedToOpenSocket"}"#
            )
            .map(|m| m.class),
            Some(ProviderErrorClass::Transport)
        );
        assert_eq!(
            classify_provider_error_with_match(
                r#""subtype":"api_error","cause":{"code":"ConnectionRefused"}"#
            )
            .map(|m| m.class),
            Some(ProviderErrorClass::Transport)
        );
    }

    #[test]
    fn classify_none_for_clean_output() {
        assert_eq!(
            classify_provider_error_with_match("Task completed successfully.").map(|m| m.class),
            None
        );
    }

    #[test]
    fn does_not_classify_sandbox_metadata_as_permission_error() {
        assert_eq!(
            classify_provider_error_with_match("sandbox_mode=danger-full-access").map(|m| m.class),
            None
        );
    }

    #[test]
    fn does_not_classify_timeout_word_in_command_as_transport_error() {
        assert_eq!(
            classify_provider_error_with_match("timeout 25s notion task view 8088")
                .map(|m| m.class),
            None
        );
    }

    #[test]
    fn classify_python_request_timeout_as_transport() {
        assert_eq!(
            classify_provider_error_with_match(
                "notion_client.errors.RequestTimeoutError: Request to Notion API has timed out"
            )
            .map(|m| m.class),
            Some(ProviderErrorClass::Transport)
        );
    }

    #[test]
    fn does_not_classify_plain_forbidden_word_without_auth_context() {
        assert_eq!(
            classify_provider_error_with_match("The spec says this pattern is forbidden.")
                .map(|m| m.class),
            None
        );
    }

    #[test]
    fn no_child_processes_for_nonexistent_pid() {
        // PID u32::MAX will never exist; must return false without panicking.
        assert!(!has_live_child_processes(u32::MAX));
    }

    #[test]
    fn detects_live_child_process() {
        // DFS from sh's PID, not from std::process::id().  Parallel tests
        // also spawn children of the test process, which would cause a false
        // positive if we used the test-process PID as root.
        //
        // `sleep 30 & wait` prevents dash/sh from exec-replacing itself with
        // sleep (which would make sh_pid point to sleep with no children).
        // With `& wait`, sh stays alive as the parent of the sleep child.
        //
        // Poll up to 500 ms: there is an inherent race between spawn() returning
        // and sh forking its sleep child (shell startup + fork takes a few ms).
        let mut sh = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .spawn()
            .expect("sh must be available");
        let sh_pid = sh.id();
        let found = (0..10_u8).any(|i| {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            has_live_child_processes(sh_pid)
        });
        sh.kill().ok();
        let _ = sh.wait();
        assert!(
            found,
            "live sleep child of sh must be detected within 500 ms"
        );
    }

    #[test]
    fn no_child_after_wait() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep must be available");
        child.kill().ok();
        let _ = child.wait(); // reap so it leaves the process table
                              // Give the kernel a moment to update /proc
        std::thread::sleep(std::time::Duration::from_millis(50));
        // There may be other children from parallel test runners, so we only check
        // that our specific child PID is no longer reported as a live descendant.
        let child_pid = child.id();
        let child_alive = procfs::process::Process::new(child_pid as i32)
            .ok()
            .and_then(|p| p.stat().ok())
            .map(|s| s.state != 'Z' && s.state != 'X')
            .unwrap_or(false);
        assert!(!child_alive, "reaped child must not appear alive in /proc");
    }
}
