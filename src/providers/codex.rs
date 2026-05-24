use std::path::Path;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;

use crate::errors::OrchestratorResult;
use crate::providers::{temp_log_path, DispatchedProcess};

/// One row in `~/.codex/session_index.jsonl`.
///
/// Discovery algorithm: read this file, filter by `thread_name == expected`, require
/// exactly one match.  Corrupted lines and multiple matches are both errors —
/// ambiguous session binding must never silently use the wrong session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexSessionIndexRow {
    pub id: String,
    pub thread_name: String,
    /// ISO-8601 timestamp string used as a lexicographically sortable freshness key.
    pub updated_at: String,
}

/// Dispatch Codex in exec-resume mode.
pub async fn dispatch(
    session_id: &str,
    repo: &Path,
    trigger_prompt: &str,
) -> OrchestratorResult<DispatchedProcess> {
    let stdout_path = temp_log_path("codex_stdout");
    let stderr_path = temp_log_path("codex_stderr");

    let stdout_file = std::fs::File::create(&stdout_path)?;
    let stderr_file = std::fs::File::create(&stderr_path)?;

    let mut cmd = tokio::process::Command::new("codex");
    cmd.args([
        "exec",
        "resume",
        "--json",
        "--dangerously-bypass-approvals-and-sandbox",
    ])
    .args([session_id, trigger_prompt])
    .current_dir(repo)
    .stdout(Stdio::from(stdout_file))
    .stderr(Stdio::from(stderr_file));
    super::apply_agent_env(&mut cmd);
    super::apply_child_death_policy(&mut cmd);
    let child = cmd.spawn()?;

    Ok(DispatchedProcess {
        child,
        stdout_path,
        stderr_path,
    })
}

/// Returns the default location of the Codex session index.
fn session_index_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| {
        std::path::PathBuf::from(h)
            .join(".codex")
            .join("session_index.jsonl")
    })
}

/// Discover a Codex session by thread name from `~/.codex/session_index.jsonl`.
///
/// Fails fast on any non-parseable line (structured index; no non-JSON lines expected).
/// Filters by `workspace_root` by reading `session_meta.payload.cwd` from each
/// candidate's rollout JSONL — sessions started in a different directory are excluded.
pub fn discover_by_thread(
    repo: &Path,
    thread_name: &str,
    role: crate::state::AgentRole,
) -> OrchestratorResult<String> {
    let index_path =
        session_index_path().ok_or_else(|| crate::errors::OrchestratorError::InvalidInput {
            field: "HOME".to_owned(),
            reason: "HOME is not set; cannot locate ~/.codex/session_index.jsonl".to_owned(),
        })?;
    discover_by_thread_from_index(&index_path, thread_name, role, repo)
}

/// Discover a Codex session from a given index path.
///
/// Candidates are filtered by `workspace_root`: each candidate's `session_meta.payload.cwd`
/// (from its rollout JSONL) is compared to `workspace_root`. If the rollout file cannot be
/// found or does not contain a cwd, the candidate is treated as dangling and excluded.
pub fn discover_by_thread_from_index(
    index_path: &Path,
    thread_name: &str,
    role: crate::state::AgentRole,
    workspace_root: &Path,
) -> OrchestratorResult<String> {
    discover_by_thread_from_index_with_cwd_lookup(
        index_path,
        thread_name,
        role,
        workspace_root,
        codex_session_cwd,
    )
}

/// Discover a Codex session from a given index path and a session cwd lookup.
///
/// The lookup returns `session_meta.payload.cwd` for a session id, or `None` when
/// the session rollout metadata is missing/unreadable. `None` candidates are treated
/// as dangling and excluded from selection.
fn discover_by_thread_from_index_with_cwd_lookup<F>(
    index_path: &Path,
    thread_name: &str,
    role: crate::state::AgentRole,
    workspace_root: &Path,
    cwd_lookup: F,
) -> OrchestratorResult<String>
where
    F: Fn(&str) -> Option<String>,
{
    let content = std::fs::read_to_string(index_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            crate::errors::OrchestratorError::SessionBindFailed {
                role,
                provider: crate::state::ProviderKind::Codex,
                reason: "~/.codex/session_index.jsonl not found; create a Codex session first"
                    .to_owned(),
            }
        } else {
            crate::errors::OrchestratorError::SessionBindFailed {
                role,
                provider: crate::state::ProviderKind::Codex,
                reason: format!("failed to read ~/.codex/session_index.jsonl: {e}"),
            }
        }
    })?;
    let workspace_root = workspace_root.to_string_lossy();
    let mut latest_by_id: HashMap<String, CodexSessionIndexRow> = HashMap::new();

    for (line_no, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        // Fail fast on any non-parseable line: the index is a structured file with no free-text lines.
        let row: CodexSessionIndexRow = serde_json::from_str(line).map_err(|e| {
            crate::errors::OrchestratorError::SessionBindFailed {
                role,
                provider: crate::state::ProviderKind::Codex,
                reason: format!(
                    "failed to parse codex session_index.jsonl line {}: {e}",
                    line_no + 1
                ),
            }
        })?;
        // session_index.jsonl is append-only; keep only the latest row for each
        // session id so historical thread names do not affect current binding.
        match latest_by_id.get(&row.id) {
            Some(prev) if prev.updated_at >= row.updated_at => {}
            _ => {
                latest_by_id.insert(row.id.clone(), row);
            }
        }
    }

    let mut rows: Vec<CodexSessionIndexRow> = Vec::new();
    for row in latest_by_id.into_values() {
        if row.thread_name != thread_name {
            continue;
        }
        // Filter by workspace directory and exclude dangling index rows with no
        // resolvable rollout/cwd data.
        let Some(cwd) = cwd_lookup(&row.id) else {
            continue;
        };
        if cwd != workspace_root.as_ref() {
            continue;
        }
        rows.push(row);
    }

    match rows.len() {
        0 => Err(crate::errors::OrchestratorError::SessionBindFailed {
            role,
            provider: crate::state::ProviderKind::Codex,
            reason: format!("no session with thread name '{thread_name}' found"),
        }),
        1 => Ok(rows.into_iter().next().unwrap().id),
        n => Err(crate::errors::OrchestratorError::SessionBindFailed {
            role,
            provider: crate::state::ProviderKind::Codex,
            reason: build_codex_ambiguity_reason(&rows, n, thread_name),
        }),
    }
}

/// Read `session_meta.payload.cwd` from the rollout JSONL for `session_id`.
///
/// The rollout file is at `~/.codex/sessions/<year>/<month>/<day>/rollout-*-<session_id>.jsonl`.
/// The first line is always `session_meta`; we read only that line.
fn codex_session_cwd(session_id: &str) -> Option<String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let sessions_dir = home.join(".codex").join("sessions");
    for year_entry in std::fs::read_dir(&sessions_dir).ok()?.flatten() {
        for month_entry in std::fs::read_dir(year_entry.path()).ok()?.flatten() {
            for day_entry in std::fs::read_dir(month_entry.path()).ok()?.flatten() {
                for file_entry in std::fs::read_dir(day_entry.path()).ok()?.flatten() {
                    let path = file_entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let stem = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default();
                    if !stem.ends_with(session_id) {
                        continue;
                    }
                    let content = std::fs::read_to_string(&path).ok()?;
                    let first = content.lines().next()?;
                    let obj = serde_json::from_str::<serde_json::Value>(first).ok()?;
                    if obj.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
                        return obj
                            .get("payload")
                            .and_then(|p| p.get("cwd"))
                            .and_then(|v| v.as_str())
                            .map(str::to_owned);
                    }
                }
            }
        }
    }
    None
}

/// Return the mtime of the Codex rollout JSONL file for the given session.
///
/// Session files are stored as `~/.codex/sessions/**/*-<session_id>.jsonl`.
/// Walks the entire year/month/day hierarchy under `~/.codex/sessions/`.
pub fn session_jsonl_mtime(session_id: &str) -> OrchestratorResult<Option<std::time::SystemTime>> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| crate::errors::OrchestratorError::InvalidInput {
            field: "HOME".to_owned(),
            reason: "HOME is not set; cannot locate Codex session logs".to_owned(),
        })?;
    let sessions_dir = home.join(".codex").join("sessions");

    // Walk the year/month/day hierarchy looking for a matching filename suffix.
    // Depth is bounded by the calendar-day structure; no unbounded traversal.
    let year_entries = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    for year_entry in year_entries {
        let year_entry = year_entry?;
        for month_entry in std::fs::read_dir(year_entry.path())? {
            let month_entry = month_entry?;
            for day_entry in std::fs::read_dir(month_entry.path())? {
                let day_entry = day_entry?;
                for file_entry in std::fs::read_dir(day_entry.path())? {
                    let file_entry = file_entry?;
                    let path = file_entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default();
                    if name.ends_with(session_id) {
                        return Ok(Some(std::fs::metadata(&path)?.modified()?));
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Find Codex JSONL path and metadata for one session id.
///
/// value: `(log_path, mtime, file_size_bytes)`
fn find_session_jsonl_info(
    session_id: &str,
) -> OrchestratorResult<(
    Option<std::path::PathBuf>,
    Option<std::time::SystemTime>,
    Option<u64>,
)> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| crate::errors::OrchestratorError::InvalidInput {
            field: "HOME".to_owned(),
            reason: "HOME is not set; cannot locate Codex session logs".to_owned(),
        })?;
    let sessions_dir = home.join(".codex").join("sessions");
    let year_entries = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, None, None)),
        Err(e) => return Err(e.into()),
    };
    for year_entry in year_entries {
        let year_entry = year_entry?;
        for month_entry in std::fs::read_dir(year_entry.path())? {
            let month_entry = month_entry?;
            for day_entry in std::fs::read_dir(month_entry.path())? {
                let day_entry = day_entry?;
                for file_entry in std::fs::read_dir(day_entry.path())? {
                    let file_entry = file_entry?;
                    let path = file_entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default();
                    if name.ends_with(session_id) {
                        let meta = std::fs::metadata(&path)?;
                        return Ok((Some(path), Some(meta.modified()?), Some(meta.len())));
                    }
                }
            }
        }
    }
    Ok((None, None, None))
}

fn build_codex_ambiguity_reason(
    rows: &[CodexSessionIndexRow],
    n: usize,
    thread_name: &str,
) -> String {
    let mut lines = Vec::new();
    for row in rows {
        let (log_path, log_mtime, file_size_bytes) =
            find_session_jsonl_info(&row.id).unwrap_or((None, None, None));
        let log_path_str = log_path
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let log_mtime_str = log_mtime
            .map(|ts| chrono::DateTime::<chrono::Utc>::from(ts).to_rfc3339())
            .unwrap_or_else(|| "unknown".to_owned());
        let file_size_bytes_str = file_size_bytes
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        lines.push(format!(
            "  session_id={} updated_at={} log_mtime={} file_size_bytes={} log_path={}",
            row.id, row.updated_at, log_mtime_str, file_size_bytes_str, log_path_str
        ));
    }
    lines.sort();
    tracing::warn!(
        "ambiguous Codex session binding: {n} sessions share thread name '{thread_name}' — matching sessions:\n{}",
        lines.join("\n")
    );
    format!(
        "{n} sessions share thread name '{thread_name}' — rename or delete all but one (see stderr for session list)"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AgentRole;
    use std::io::Write;

    fn fake_cwd_lookup(_: &str) -> Option<String> {
        Some("/".to_owned())
    }

    #[test]
    fn discover_by_thread_from_index_single_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_index.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"id":"abc123","thread_name":"TASK-1","updated_at":"2026-01-01T10:00:00Z"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"id":"xyz999","thread_name":"OTHER","updated_at":"2026-01-01T11:00:00Z"}}"#
        )
        .unwrap();

        let result = discover_by_thread_from_index_with_cwd_lookup(
            &path,
            "TASK-1",
            AgentRole::Executor,
            std::path::Path::new("/"),
            fake_cwd_lookup,
        )
        .unwrap();
        assert_eq!(result, "abc123");
    }

    #[test]
    fn discover_by_thread_from_index_multiple_match_is_error() {
        // Two sessions with the same thread name — different timestamps — must be rejected.
        // The user must resolve the ambiguity before running the orchestrator.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_index.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"id":"old","thread_name":"TASK","updated_at":"2026-01-01T09:00:00Z"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"id":"new","thread_name":"TASK","updated_at":"2026-01-02T12:00:00Z"}}"#
        )
        .unwrap();

        let result = discover_by_thread_from_index_with_cwd_lookup(
            &path,
            "TASK",
            AgentRole::Executor,
            std::path::Path::new("/"),
            fake_cwd_lookup,
        );
        match result {
            Err(crate::errors::OrchestratorError::SessionBindFailed { reason, .. }) => {
                assert!(reason.contains("TASK"));
                assert!(reason.contains("see stderr"));
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn discover_by_thread_fails_on_invalid_json_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_index.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{{not valid json}}").unwrap();

        let result = discover_by_thread_from_index_with_cwd_lookup(
            &path,
            "TASK",
            AgentRole::Executor,
            std::path::Path::new("/"),
            fake_cwd_lookup,
        );
        assert!(matches!(
            result,
            Err(crate::errors::OrchestratorError::SessionBindFailed { .. })
        ));
    }

    #[test]
    fn discover_by_thread_tie_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_index.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"id":"a","thread_name":"T","updated_at":"2026-01-01T10:00:00Z"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"id":"b","thread_name":"T","updated_at":"2026-01-01T10:00:00Z"}}"#
        )
        .unwrap();

        let result = discover_by_thread_from_index_with_cwd_lookup(
            &path,
            "T",
            AgentRole::Executor,
            std::path::Path::new("/"),
            fake_cwd_lookup,
        );
        assert!(matches!(
            result,
            Err(crate::errors::OrchestratorError::SessionBindFailed { .. })
        ));
    }

    #[test]
    fn discover_by_thread_dedups_same_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_index.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"id":"same","thread_name":"T","updated_at":"2026-01-01T10:00:00Z"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"id":"same","thread_name":"T","updated_at":"2026-01-03T10:00:00Z"}}"#
        )
        .unwrap();

        let result = discover_by_thread_from_index_with_cwd_lookup(
            &path,
            "T",
            AgentRole::Executor,
            std::path::Path::new("/"),
            fake_cwd_lookup,
        )
        .unwrap();
        assert_eq!(result, "same");
    }

    #[test]
    fn discover_by_thread_uses_latest_thread_name_for_same_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_index.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"id":"same","thread_name":"old-name","updated_at":"2026-01-01T10:00:00Z"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"id":"same","thread_name":"new-name","updated_at":"2026-01-02T10:00:00Z"}}"#
        )
        .unwrap();

        let old = discover_by_thread_from_index_with_cwd_lookup(
            &path,
            "old-name",
            AgentRole::Executor,
            std::path::Path::new("/"),
            fake_cwd_lookup,
        );
        assert!(matches!(
            old,
            Err(crate::errors::OrchestratorError::SessionBindFailed { .. })
        ));

        let new = discover_by_thread_from_index_with_cwd_lookup(
            &path,
            "new-name",
            AgentRole::Executor,
            std::path::Path::new("/"),
            fake_cwd_lookup,
        )
        .unwrap();
        assert_eq!(new, "same");
    }

    #[test]
    fn discover_by_thread_excludes_dangling_rows_without_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_index.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"id":"with-cwd","thread_name":"T","updated_at":"2026-01-01T10:00:00Z"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"id":"dangling","thread_name":"T","updated_at":"2026-01-02T10:00:00Z"}}"#
        )
        .unwrap();

        let result = discover_by_thread_from_index_with_cwd_lookup(
            &path,
            "T",
            AgentRole::Executor,
            std::path::Path::new("/repo"),
            |id| {
                if id == "with-cwd" {
                    Some("/repo".to_owned())
                } else {
                    None
                }
            },
        )
        .unwrap();
        assert_eq!(result, "with-cwd");
    }

    #[test]
    fn discover_by_thread_missing_index_is_session_bind_failed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent_index.jsonl");

        let result = discover_by_thread_from_index(
            &path,
            "TASK",
            AgentRole::Executor,
            std::path::Path::new("/"),
        );
        assert!(matches!(
            result,
            Err(crate::errors::OrchestratorError::SessionBindFailed { .. })
        ));
    }
}
