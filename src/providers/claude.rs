use chrono::{DateTime, Utc};
use std::path::Path;
use std::time::SystemTime;

use std::process::Stdio;

use crate::errors::OrchestratorResult;
use crate::providers::{temp_log_path, DispatchedProcess};
use crate::sessions::claude_project_key;

/// value: `(session_id, log_path, mtime, file_size_bytes)`
fn build_claude_ambiguity_reason(
    entries: &[(String, std::path::PathBuf, Option<SystemTime>, Option<u64>)],
    n: usize,
    thread_name: &str,
) -> String {
    let mut lines = entries
        .iter()
        .map(|(session_id, log_path, mtime, file_size_bytes)| {
            let updated_at = mtime
                .map(|ts| chrono::DateTime::<chrono::Utc>::from(ts).to_rfc3339())
                .unwrap_or_else(|| "unknown".to_owned());
            let file_size_bytes = file_size_bytes
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_owned());
            format!(
                "  session_id={} updated_at={} file_size_bytes={} log_path={}",
                session_id,
                updated_at,
                file_size_bytes,
                log_path.display()
            )
        })
        .collect::<Vec<_>>();
    lines.sort();
    tracing::warn!(
        "ambiguous Claude session binding: {n} sessions share thread name '{thread_name}' — matching sessions:\n{}",
        lines.join("\n")
    );
    format!(
        "{n} sessions share thread name '{thread_name}' — rename or delete all but one (see stderr for session list)"
    )
}

/// Dispatch Claude in print mode (`-p`) resuming an existing session.
pub async fn dispatch(
    session_id: &str,
    repo: &Path,
    trigger_prompt: &str,
) -> OrchestratorResult<DispatchedProcess> {
    let stdout_path = temp_log_path("claude_stdout");
    let stderr_path = temp_log_path("claude_stderr");

    let stdout_file = std::fs::File::create(&stdout_path)?;
    let stderr_file = std::fs::File::create(&stderr_path)?;

    let mut cmd = tokio::process::Command::new("claude");
    cmd.args([
        "-p",
        "--resume",
        session_id,
        "--permission-mode",
        "bypassPermissions",
        trigger_prompt,
    ])
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

/// Provider metadata signal: mtime of the Claude session JSONL file.
///
/// Returns `None` when the log file cannot be found.
pub fn session_jsonl_mtime(
    repo: &Path,
    session_id: &str,
) -> OrchestratorResult<Option<SystemTime>> {
    let key = claude_project_key(repo);
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| crate::errors::OrchestratorError::InvalidInput {
            field: "HOME".to_owned(),
            reason: "HOME environment variable is not set".to_owned(),
        })?;
    let log_path = home
        .join(".claude")
        .join("projects")
        .join(&key)
        .join(format!("{session_id}.jsonl"));

    match std::fs::metadata(log_path) {
        Ok(meta) => Ok(Some(meta.modified()?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Maximum number of bytes read from the tail of the JSONL when scanning for stop signals.
const MAX_JSONL_TAIL_BYTES: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeJsonlServiceCapSignal {
    pub pattern: &'static str,
    pub fragment: String,
}

/// Returns a service-cap signal from Claude JSONL entries written after this dispatch.
///
/// This is safe to run while the Claude process is alive: incomplete JSONL records are ignored
/// until a later probe sees the completed line.
pub async fn session_jsonl_service_cap_since(
    repo: &Path,
    session_id: &str,
    since: DateTime<Utc>,
) -> OrchestratorResult<Option<ClaudeJsonlServiceCapSignal>> {
    let home = match std::env::var_os("HOME").map(std::path::PathBuf::from) {
        Some(h) => h,
        None => {
            return Err(crate::errors::OrchestratorError::InvalidInput {
                field: "HOME".to_owned(),
                reason: "HOME environment variable is not set".to_owned(),
            });
        }
    };
    let key = claude_project_key(repo);
    let log_path = home
        .join(".claude")
        .join("projects")
        .join(&key)
        .join(format!("{session_id}.jsonl"));
    let tail = crate::signals::read_log_tail(&log_path, MAX_JSONL_TAIL_BYTES as u64).await?;
    Ok(tail_service_cap_since(&tail, since))
}

/// Returns `true` when `tail` contains a `max_tokens` stop-reason field.
///
/// The Anthropic API always emits `"stop_reason": "max_tokens"` (space after colon).
/// The compact form is checked as a safety net.
#[cfg(test)]
fn tail_has_max_tokens(tail: &str) -> bool {
    tail.contains(r#""stop_reason": "max_tokens""#)
        || tail.contains(r#""stop_reason":"max_tokens""#)
}

/// Returns `true` when `tail` contains a rate-limit signal.
#[cfg(test)]
fn tail_has_rate_limit(tail: &str) -> bool {
    tail.contains(r#""error":"rate_limit""#)
        || tail.contains(r#""error": "rate_limit""#)
        || tail.contains(r#""apiErrorStatus":429"#)
        || tail.contains(r#""apiErrorStatus": 429"#)
        || tail.contains("You've hit your limit")
}

fn tail_service_cap_since(tail: &str, since: DateTime<Utc>) -> Option<ClaudeJsonlServiceCapSignal> {
    for line in tail.lines().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(timestamp) = value
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
            .map(|ts| ts.with_timezone(&Utc))
        else {
            continue;
        };
        if timestamp < since {
            continue;
        }

        if value
            .pointer("/message/stop_reason")
            .and_then(serde_json::Value::as_str)
            == Some("max_tokens")
        {
            return Some(ClaudeJsonlServiceCapSignal {
                pattern: "claude_jsonl_max_tokens_since_dispatch",
                fragment: "max_tokens".to_owned(),
            });
        }

        if value.get("error").and_then(serde_json::Value::as_str) == Some("rate_limit") {
            return Some(ClaudeJsonlServiceCapSignal {
                pattern: "claude_jsonl_rate_limit_since_dispatch",
                fragment: "rate_limit".to_owned(),
            });
        }

        if value
            .get("apiErrorStatus")
            .and_then(serde_json::Value::as_u64)
            == Some(429)
        {
            return Some(ClaudeJsonlServiceCapSignal {
                pattern: "claude_jsonl_api_429_since_dispatch",
                fragment: "apiErrorStatus=429".to_owned(),
            });
        }

        if value
            .get("isApiErrorMessage")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            let serialized = serde_json::to_string(&value).unwrap_or_default();
            if crate::signals::classify_provider_error_with_match(&serialized)
                .map(|m| matches!(m.class, crate::state::ProviderErrorClass::ServiceCap))
                .unwrap_or(false)
            {
                return Some(ClaudeJsonlServiceCapSignal {
                    pattern: "claude_jsonl_api_error_text_since_dispatch",
                    fragment: "isApiErrorMessage service cap".to_owned(),
                });
            }
        }
    }

    None
}

/// Discover a Claude session UUID by thread name from project JSONL logs.
///
/// Scans `~/.claude/projects/<claude_project_key>/*.jsonl`, computes the effective
/// title for each file, and requires exactly one match.
pub fn discover_by_thread(
    repo: &Path,
    thread_name: &str,
    role: crate::state::AgentRole,
) -> OrchestratorResult<String> {
    let key = claude_project_key(repo);
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| crate::errors::OrchestratorError::InvalidInput {
            field: "HOME".to_owned(),
            reason: "HOME environment variable is not set".to_owned(),
        })?;

    let projects_dir = home.join(".claude").join("projects").join(&key);

    let entries = match std::fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(_) => {
            return Err(crate::errors::OrchestratorError::SessionBindFailed {
                role,
                provider: crate::state::ProviderKind::Claude,
                reason: format!("Claude project dir not found: {}", projects_dir.display()),
            });
        }
    };

    struct Candidate {
        session_id: String,
        log_path: std::path::PathBuf,
        mtime: Option<SystemTime>,
        file_size_bytes: Option<u64>,
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };

        let effective_title = match claude_effective_title(&path) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                return Err(crate::errors::OrchestratorError::SessionBindFailed {
                    role,
                    provider: crate::state::ProviderKind::Claude,
                    reason: format!("failed to parse session log {}: {e}", path.display()),
                });
            }
        };

        if effective_title != thread_name {
            continue;
        }

        let meta = std::fs::metadata(&path).ok();
        let mtime = meta.as_ref().and_then(|m| m.modified().ok());
        let file_size_bytes = meta.as_ref().map(|m| m.len());

        candidates.push(Candidate {
            session_id,
            log_path: path,
            mtime,
            file_size_bytes,
        });
    }

    match candidates.len() {
        0 => Err(crate::errors::OrchestratorError::SessionBindFailed {
            role,
            provider: crate::state::ProviderKind::Claude,
            reason: format!("no session with thread name '{thread_name}' found"),
        }),
        1 => Ok(candidates.into_iter().next().unwrap().session_id),
        n => {
            let entries = candidates
                .iter()
                .map(|c| {
                    (
                        c.session_id.clone(),
                        c.log_path.clone(),
                        c.mtime,
                        c.file_size_bytes,
                    )
                })
                .collect::<Vec<_>>();
            Err(crate::errors::OrchestratorError::SessionBindFailed {
                role,
                provider: crate::state::ProviderKind::Claude,
                reason: build_claude_ambiguity_reason(&entries, n, thread_name),
            })
        }
    }
}

/// Compute the effective display title for a Claude JSONL session log.
///
/// Reads every line; returns the last `customTitle` (type=custom-title) or the last
/// `agentName` (type=agent-name) — whichever appears. Returns `Ok(None)` when neither
/// is found. Skips lines that cannot be parsed as valid JSON (e.g. null-byte blocks
/// written by the OS when a write is interrupted) rather than failing the whole scan —
/// session logs are not under orchestrator control and may contain corrupted lines.
pub fn claude_effective_title(path: &Path) -> OrchestratorResult<Option<String>> {
    let content = std::fs::read_to_string(path)?;
    let mut custom_title: Option<String> = None;
    let mut agent_name: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() || line.bytes().all(|b| b == 0) {
            continue;
        }
        let obj = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match obj.get("type").and_then(|v| v.as_str()) {
            Some("custom-title") => {
                custom_title = obj
                    .get("customTitle")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
            }
            Some("agent-name") => {
                agent_name = obj
                    .get("agentName")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
            }
            _ => {} // unrecognized type — skip
        }
    }

    Ok(custom_title.or(agent_name))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn claude_effective_title_custom_title_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"agent-name","agentName":"OldName"}}"#).unwrap();
        writeln!(f, r#"{{"type":"custom-title","customTitle":"MyTask"}}"#).unwrap();
        writeln!(f, r#"{{"type":"agent-name","agentName":"IgnoredName"}}"#).unwrap();

        let title = claude_effective_title(&path).unwrap();
        // custom-title takes precedence
        assert_eq!(title.as_deref(), Some("MyTask"));
    }

    #[test]
    fn claude_effective_title_agent_name_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"agent-name","agentName":"FallbackName"}}"#).unwrap();
        writeln!(f, r#"{{"type":"other","data":123}}"#).unwrap();

        let title = claude_effective_title(&path).unwrap();
        assert_eq!(title.as_deref(), Some("FallbackName"));
    }

    #[test]
    fn claude_effective_title_returns_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"other","data":1}}"#).unwrap();

        let title = claude_effective_title(&path).unwrap();
        assert!(title.is_none());
    }

    #[test]
    fn claude_effective_title_skips_invalid_json_lines() {
        // Corrupted lines (e.g. null-byte blocks) must not abort the scan.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Invalid JSON line followed by a valid title entry.
        writeln!(f, "{{not valid json}}").unwrap();
        writeln!(f, r#"{{"type":"custom-title","customTitle":"MyTask"}}"#).unwrap();

        let title = claude_effective_title(&path).unwrap();
        assert_eq!(title.as_deref(), Some("MyTask"));
    }

    #[test]
    fn claude_effective_title_skips_null_byte_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        // Write a valid title line, then a null-byte block, then more valid lines.
        let mut data = Vec::new();
        data.extend_from_slice(b"{\"type\":\"custom-title\",\"customTitle\":\"GoodTitle\"}\n");
        data.extend_from_slice(&[0u8; 80]);
        data.push(b'\n');
        std::fs::write(&path, data).unwrap();

        let title = claude_effective_title(&path).unwrap();
        assert_eq!(title.as_deref(), Some("GoodTitle"));
    }

    #[test]
    fn jsonl_tail_detects_max_tokens() {
        let tail =
            r#"{"type":"assistant","message":{"stop_reason": "max_tokens","role":"assistant"}}"#;
        assert!(tail_has_max_tokens(tail));
    }

    #[test]
    fn jsonl_tail_detects_max_tokens_compact_form() {
        let tail =
            r#"{"type":"assistant","message":{"stop_reason":"max_tokens","role":"assistant"}}"#;
        assert!(tail_has_max_tokens(tail));
    }

    #[test]
    fn jsonl_tail_no_false_positive_on_end_turn() {
        let tail =
            r#"{"type":"assistant","message":{"stop_reason": "end_turn","role":"assistant"}}"#;
        assert!(!tail_has_max_tokens(tail));
    }

    #[test]
    fn jsonl_tail_no_false_positive_on_mention_in_text() {
        // The word "max_tokens" appears in assistant text but not as stop_reason value.
        let tail = r#"{"type":"assistant","message":{"content":[{"text":"set max_tokens to 1024"}],"stop_reason": "end_turn"}}"#;
        assert!(!tail_has_max_tokens(tail));
    }

    #[test]
    fn jsonl_tail_returns_false_for_missing_file() {
        assert!(!tail_has_max_tokens(""));
    }

    #[test]
    fn jsonl_tail_detects_rate_limit() {
        let tail = r#"{"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429}"#;
        assert!(tail_has_rate_limit(tail));
        assert!(tail_has_rate_limit("You've hit your limit · resets 2:10pm"));
    }

    #[test]
    fn jsonl_tail_detects_fresh_rate_limit_since_dispatch() {
        let since = DateTime::parse_from_rfc3339("2026-05-24T16:25:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let tail = r#"{"timestamp":"2026-05-24T16:25:38.097Z","type":"assistant","message":{"content":[{"type":"text","text":"You've hit your session limit · resets 2:30pm"}],"stop_reason":"stop_sequence"},"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429}"#;

        let signal = tail_service_cap_since(tail, since).unwrap();
        assert_eq!(signal.pattern, "claude_jsonl_rate_limit_since_dispatch");
        assert_eq!(signal.fragment, "rate_limit");
    }

    #[test]
    fn jsonl_tail_ignores_old_rate_limit_before_dispatch() {
        let since = DateTime::parse_from_rfc3339("2026-05-24T16:26:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let tail = r#"{"timestamp":"2026-05-24T16:25:38.097Z","type":"assistant","message":{"content":[{"type":"text","text":"You've hit your session limit · resets 2:30pm"}],"stop_reason":"stop_sequence"},"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429}"#;

        assert!(tail_service_cap_since(tail, since).is_none());
    }

    #[test]
    fn jsonl_tail_detects_fresh_max_tokens_since_dispatch() {
        let since = DateTime::parse_from_rfc3339("2026-05-24T16:25:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let tail = r#"{"timestamp":"2026-05-24T16:25:38.097Z","type":"assistant","message":{"stop_reason":"max_tokens"}}"#;

        let signal = tail_service_cap_since(tail, since).unwrap();
        assert_eq!(signal.pattern, "claude_jsonl_max_tokens_since_dispatch");
        assert_eq!(signal.fragment, "max_tokens");
    }

    #[test]
    fn ambiguity_reason_contains_session_fields() {
        let entries = vec![
            (
                "c1".to_owned(),
                std::path::PathBuf::from("/tmp/c1.jsonl"),
                Some(
                    std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_700_000_000),
                ),
                Some(128),
            ),
            (
                "c2".to_owned(),
                std::path::PathBuf::from("/tmp/c2.jsonl"),
                Some(
                    std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_700_000_100),
                ),
                Some(256),
            ),
        ];

        let reason = build_claude_ambiguity_reason(&entries, entries.len(), "my-task");
        assert!(reason.contains("my-task"));
        assert!(reason.contains("see stderr"));
    }
}
