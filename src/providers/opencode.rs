use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::process::Stdio;

use crate::errors::{OrchestratorError, OrchestratorResult};
use crate::providers::{temp_log_path, DispatchedProcess};

const SYSTEMD_MEMORY_HIGH: &str = "MemoryHigh=18G";
const SYSTEMD_MEMORY_MAX: &str = "MemoryMax=20G";
const SYSTEMD_MEMORY_SWAP_MAX: &str = "MemorySwapMax=2G";

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
pub async fn dispatch(
    session_id: &str,
    repo: &Path,
    trigger_prompt: &str,
    model: Option<&str>,
) -> OrchestratorResult<DispatchedProcess> {
    let stdout_path = temp_log_path("opencode_stdout");
    let stderr_path = temp_log_path("opencode_stderr");

    let stdout_file = std::fs::File::create(&stdout_path)?;
    let stderr_file = std::fs::File::create(&stderr_path)?;

    let args = build_run_args(session_id, trigger_prompt, model);
    let mut cmd = if systemd_run_available(repo).await {
        let unit = build_systemd_unit_name();
        let systemd_args = build_systemd_run_args(&unit, &args);
        tracing::info!("dispatching OpenCode through systemd-run unit={unit}");
        let mut cmd = tokio::process::Command::new("systemd-run");
        cmd.args(&systemd_args);
        cmd
    } else {
        tracing::info!("dispatching OpenCode directly; systemd-run user scope is unavailable");
        let mut cmd = tokio::process::Command::new("opencode");
        cmd.args(&args);
        cmd
    };
    cmd.current_dir(repo)
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

async fn systemd_run_available(repo: &Path) -> bool {
    let unit = format!("orchestrate-opencode-probe-{}", unique_unit_suffix());
    let status = tokio::process::Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--collect",
            "--same-dir",
            "--quiet",
            &format!("--unit={unit}"),
            "true",
        ])
        .current_dir(repo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    matches!(status, Ok(status) if status.success())
}

fn build_systemd_unit_name() -> String {
    format!("orchestrate-opencode-{}", unique_unit_suffix())
}

fn unique_unit_suffix() -> String {
    let timestamp = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| Utc::now().timestamp_micros() * 1_000);
    format!("{}-{timestamp}", std::process::id())
}

/// Validate that `model` is listed by the local OpenCode CLI before dispatch.
pub async fn ensure_model_available(
    repo: &Path,
    model: &str,
    field: &str,
) -> OrchestratorResult<()> {
    let provider =
        parse_model_provider(model).map_err(|reason| OrchestratorError::InvalidInput {
            field: field.to_owned(),
            reason,
        })?;

    let output = tokio::process::Command::new("opencode")
        .args(["models", provider])
        .current_dir(repo)
        .output()
        .await?;

    if !output.status.success() {
        return Err(OrchestratorError::InvalidInput {
            field: field.to_owned(),
            reason: format!(
                "failed to list OpenCode models for provider '{provider}' (exit {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if model_list_contains(&stdout, model) {
        return Ok(());
    }

    Err(OrchestratorError::InvalidInput {
        field: field.to_owned(),
        reason: format!("OpenCode model '{model}' was not found in `opencode models {provider}`"),
    })
}

pub fn parse_model_provider(model: &str) -> Result<&str, String> {
    let model = model.trim();
    let Some((provider, model_name)) = model.split_once('/') else {
        return Err(
            "must use provider/model format, for example deepseek/deepseek-v4-flash".to_owned(),
        );
    };
    if provider.is_empty() || model_name.is_empty() || model_name.contains('/') {
        return Err("must use provider/model format with exactly one '/' separator".to_owned());
    }
    Ok(provider)
}

fn model_list_contains(output: &str, model: &str) -> bool {
    output.lines().any(|line| line.trim() == model)
}

fn build_run_args(session_id: &str, trigger_prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec!["run".to_owned(), "-s".to_owned(), session_id.to_owned()];
    if let Some(model) = model {
        args.push("--model".to_owned());
        args.push(model.to_owned());
    }
    args.push(trigger_prompt.to_owned());
    args
}

fn build_systemd_run_args(unit: &str, opencode_args: &[String]) -> Vec<String> {
    let mut args = vec![
        "--user".to_owned(),
        "--scope".to_owned(),
        "--collect".to_owned(),
        "--same-dir".to_owned(),
        "--quiet".to_owned(),
        format!("--unit={unit}"),
        "-p".to_owned(),
        SYSTEMD_MEMORY_HIGH.to_owned(),
        "-p".to_owned(),
        SYSTEMD_MEMORY_MAX.to_owned(),
        "-p".to_owned(),
        SYSTEMD_MEMORY_SWAP_MAX.to_owned(),
        "opencode".to_owned(),
    ];
    args.extend(opencode_args.iter().cloned());
    args
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

    #[test]
    fn parse_model_provider_accepts_provider_model() {
        assert_eq!(
            parse_model_provider("deepseek/deepseek-v4-flash").unwrap(),
            "deepseek"
        );
    }

    #[test]
    fn parse_model_provider_rejects_invalid_format() {
        assert!(parse_model_provider("deepseek").is_err());
        assert!(parse_model_provider("/model").is_err());
        assert!(parse_model_provider("provider/").is_err());
        assert!(parse_model_provider("provider/model/extra").is_err());
    }

    #[test]
    fn model_list_contains_requires_exact_line_match() {
        let output = "deepseek/deepseek-v4-flash\ndeepseek/deepseek-r1\n";
        assert!(model_list_contains(output, "deepseek/deepseek-v4-flash"));
        assert!(!model_list_contains(output, "deepseek/deepseek-v4"));
    }

    #[test]
    fn build_run_args_includes_model_when_present() {
        let args = build_run_args("sid", "prompt", Some("zai/glm-4.7"));
        assert_eq!(
            args,
            vec!["run", "-s", "sid", "--model", "zai/glm-4.7", "prompt"]
        );
    }

    #[test]
    fn build_run_args_omits_model_when_absent() {
        let args = build_run_args("sid", "prompt", None);
        assert_eq!(args, vec!["run", "-s", "sid", "prompt"]);
    }

    #[test]
    fn build_systemd_run_args_wraps_opencode_command() {
        let opencode_args = build_run_args("sid", "prompt", Some("zai/glm-4.7"));
        let args = build_systemd_run_args("orchestrate-opencode-test", &opencode_args);

        assert_eq!(args[0], "--user");
        assert!(args.contains(&"--scope".to_owned()));
        assert!(args.contains(&"--collect".to_owned()));
        assert!(args.contains(&"--same-dir".to_owned()));
        assert!(args.contains(&"--quiet".to_owned()));
        assert!(args.contains(&"--unit=orchestrate-opencode-test".to_owned()));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-p" && w[1] == SYSTEMD_MEMORY_HIGH));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-p" && w[1] == SYSTEMD_MEMORY_MAX));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-p" && w[1] == SYSTEMD_MEMORY_SWAP_MAX));

        let opencode_pos = args.iter().position(|x| x == "opencode").unwrap();
        assert_eq!(&args[opencode_pos + 1..], opencode_args.as_slice());
    }

    #[test]
    fn build_systemd_unit_name_has_expected_prefix() {
        assert!(build_systemd_unit_name().starts_with("orchestrate-opencode-"));
    }
}
