use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::process::Stdio;

use crate::errors::OrchestratorResult;
use crate::providers::{temp_log_path, DispatchedProcess};

/// OpenCode session as returned by `opencode session list --format json`.
///
/// Discovery algorithm: run `opencode session list --format json`, filter by
/// `directory == workspace_root` AND `title == thread_name`, require exactly one match.
/// Zero or multiple matches are errors — the binding must be unambiguous.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenCodeSessionRow {
    pub id: String,
    pub title: String,
    pub directory: String,
    /// Session last-updated timestamp.
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub updated: DateTime<Utc>,
}

/// Dispatch OpenCode in session-resume mode.
pub async fn dispatch(session_id: &str, repo: &Path) -> OrchestratorResult<DispatchedProcess> {
    let stdout_path = temp_log_path("opencode_stdout");
    let stderr_path = temp_log_path("opencode_stderr");

    let stdout_file = std::fs::File::create(&stdout_path)?;
    let stderr_file = std::fs::File::create(&stderr_path)?;

    let mut cmd = tokio::process::Command::new("opencode");
    cmd.args(["run", "-s", session_id, crate::constants::TRIGGER_PROMPT])
        .current_dir(repo)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    super::apply_child_death_policy(&mut cmd);
    let child = cmd.spawn()?;

    Ok(DispatchedProcess {
        child,
        stdout_path,
        stderr_path,
    })
}

/// Discover an OpenCode session by directory + thread name.
///
/// Calls `opencode session list --format json`, filters by `directory == repo`
/// and `title == thread_name`, then requires exactly one match.
pub async fn discover_by_thread(
    repo: &Path,
    thread_name: &str,
    role: crate::state::AgentRole,
) -> OrchestratorResult<String> {
    let output = tokio::process::Command::new("opencode")
        .args(["session", "list", "--format", "json"])
        .current_dir(repo)
        .output()
        .await?;

    if !output.status.success() {
        return Err(crate::errors::OrchestratorError::SessionBindFailed {
            role,
            provider: crate::state::ProviderKind::Opencode,
            reason: format!(
                "opencode session list failed (exit {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }

    let rows = parse_session_rows_for_bind(&output.stdout, role)?;

    let repo = repo.to_string_lossy();
    if rows.is_empty() {
        return Err(crate::errors::OrchestratorError::SessionBindFailed {
            role,
            provider: crate::state::ProviderKind::Opencode,
            reason: "no OpenCode sessions found for workspace (empty session list)".to_owned(),
        });
    }

    let candidates: Vec<OpenCodeSessionRow> = rows
        .into_iter()
        .filter(|r| r.directory == repo.as_ref() && r.title == thread_name)
        .collect();

    match candidates.len() {
        0 => Err(crate::errors::OrchestratorError::SessionBindFailed {
            role,
            provider: crate::state::ProviderKind::Opencode,
            reason: format!("no session with thread name '{thread_name}' found"),
        }),
        1 => Ok(candidates.into_iter().next().unwrap().id),
        n => Err(crate::errors::OrchestratorError::SessionBindFailed {
            role,
            provider: crate::state::ProviderKind::Opencode,
            reason: build_opencode_ambiguity_reason(&candidates, n, thread_name),
        }),
    }
}

fn build_opencode_ambiguity_reason(
    candidates: &[OpenCodeSessionRow],
    n: usize,
    thread_name: &str,
) -> String {
    let mut lines = candidates
        .iter()
        .map(|r| {
            format!(
                "  session_id={} updated_at={} directory={} title={} title_len={}",
                r.id,
                r.updated.to_rfc3339(),
                r.directory,
                r.title,
                r.title.chars().count()
            )
        })
        .collect::<Vec<_>>();
    lines.sort();
    tracing::warn!(
        "ambiguous OpenCode session binding: {n} sessions share thread name '{thread_name}' — matching sessions:\n{}",
        lines.join("\n")
    );
    format!(
        "{n} sessions share thread name '{thread_name}' — rename or delete all but one (see stderr for session list)"
    )
}

/// Return the last-updated timestamp of the specific OpenCode session.
///
/// Calls `opencode session list --format json` and extracts the `updated` field for
/// `session_id`. Returns `Ok(None)` when the session is absent from the list.
/// CLI failures and JSON parse failures are fatal orchestration errors.
///
/// This is per-session rather than per-file so that activity from unrelated OpenCode
/// sessions on the same machine does not keep the staleness signal fresh.
pub(crate) async fn session_mtime(
    session_id: &str,
    repo: &Path,
) -> OrchestratorResult<Option<std::time::SystemTime>> {
    let output = tokio::process::Command::new("opencode")
        .args(["session", "list", "--format", "json"])
        .current_dir(repo)
        .output()
        .await?;
    if !output.status.success() {
        return Err(crate::errors::OrchestratorError::CommandFailed {
            program: "opencode session list --format json".to_owned(),
            status: output.status,
        });
    }
    parse_session_updated_bytes(&output.stdout, session_id)
}

/// Extract the `updated` timestamp for `session_id` from `opencode session list` JSON output.
///
/// Pure function — extracted for unit testing without spawning a subprocess.
fn parse_session_updated_bytes(
    json: &[u8],
    session_id: &str,
) -> OrchestratorResult<Option<std::time::SystemTime>> {
    let rows = parse_session_rows(json)?;
    let Some(row) = rows.into_iter().find(|r| r.id == session_id) else {
        return Ok(None);
    };
    let secs = row.updated.timestamp();
    if secs < 0 {
        return Ok(None);
    }
    let nanos = row.updated.timestamp_subsec_nanos();
    Ok(Some(
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::new(secs as u64, nanos),
    ))
}

fn parse_session_rows(json: &[u8]) -> OrchestratorResult<Vec<OpenCodeSessionRow>> {
    if json.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_slice(json)?)
}

fn parse_session_rows_for_bind(
    json: &[u8],
    role: crate::state::AgentRole,
) -> OrchestratorResult<Vec<OpenCodeSessionRow>> {
    parse_session_rows(json).map_err(|e| crate::errors::OrchestratorError::SessionBindFailed {
        role,
        provider: crate::state::ProviderKind::Opencode,
        reason: format!("failed to parse opencode session list JSON: {e}"),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_updated_matches_by_id() {
        let json = br#"[
            {"id":"aaa","title":"t","directory":"/x","updated":1700000000000},
            {"id":"bbb","title":"t","directory":"/x","updated":1700001000000}
        ]"#;
        let t = parse_session_updated_bytes(json, "bbb").unwrap().unwrap();
        let expected =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1700001000);
        assert_eq!(t, expected);
    }

    #[test]
    fn parse_session_updated_returns_none_for_missing_id() {
        let json = br#"[{"id":"aaa","title":"t","directory":"/x","updated":1700000000000}]"#;
        assert!(parse_session_updated_bytes(json, "zzz").unwrap().is_none());
    }

    #[test]
    fn parse_session_updated_returns_none_on_invalid_json() {
        assert!(parse_session_updated_bytes(b"not json", "aaa").is_err());
    }

    #[test]
    fn parse_session_updated_returns_none_on_empty_array() {
        assert!(parse_session_updated_bytes(b"[]", "aaa").unwrap().is_none());
    }

    #[test]
    fn parse_session_updated_returns_none_on_empty_stdout() {
        assert!(parse_session_updated_bytes(b"", "aaa").unwrap().is_none());
    }

    #[test]
    fn ambiguity_reason_contains_session_fields() {
        let rows = vec![
            OpenCodeSessionRow {
                id: "s1".to_owned(),
                title: "TASK".to_owned(),
                directory: "/repo1".to_owned(),
                updated: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            },
            OpenCodeSessionRow {
                id: "s2".to_owned(),
                title: "TASK".to_owned(),
                directory: "/repo2".to_owned(),
                updated: DateTime::from_timestamp(1_700_000_100, 0).unwrap(),
            },
        ];
        let reason = build_opencode_ambiguity_reason(&rows, rows.len(), "TASK");
        assert!(reason.contains("TASK"));
        assert!(reason.contains("see stderr"));
    }
}
