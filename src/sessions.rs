use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::errors::{OrchestratorError, OrchestratorResult};
use crate::state::{AgentRole, ProviderKind};

// ---------------------------------------------------------------------------
// Session binding contract
//
// The orchestrator only discovers and reuses EXISTING provider sessions.
// Creating a new session/chat is strictly forbidden for all providers.
// If deterministic bind fails (no match, ambiguous tie, metadata mismatch),
// stop with RUN_FAILED_SESSION_BIND — never silently fall back to a new session.
//
// Executor and reviewer must not share the same (provider, thread_name) pair.
// Same pair means the same session, which would mix executor and reviewer context.
// Different providers with the same thread name are allowed.
// Same provider with different thread names are allowed.
//
// Session discovery requires exactly one match per role: zero matches → no session
// found; two or more matches → ambiguous, user must rename or delete sessions.
//
// ORCHESTRATOR_SESSIONS.json stores both role bindings atomically.
// Session bind behavior:
// - if metadata scope matches current CLI (workspace_root + role provider + role thread name),
//   metadata is reused for this run;
// - otherwise sessions are rediscovered from provider state and metadata is rewritten.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Metadata types
// ---------------------------------------------------------------------------

/// Persisted record for one role's session binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleSessionRecord {
    pub provider: ProviderKind,
    pub session_id: String,
    pub session_title: String,
    pub discovery_source: String,
    pub discovered_at: DateTime<Utc>,
}

/// Root of `ORCHESTRATOR_SESSIONS.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub workspace_root: String,
    pub executor: RoleSessionRecord,
    pub reviewer: RoleSessionRecord,
}

// ---------------------------------------------------------------------------
// Metadata I/O
// ---------------------------------------------------------------------------

/// Load `ORCHESTRATOR_SESSIONS.json` from `repo`. Returns `None` when not present.
pub async fn load_session_metadata(
    repo: impl AsRef<Path>,
    provider_for_error: ProviderKind,
) -> OrchestratorResult<Option<SessionMetadata>> {
    let path = repo.as_ref().join(crate::constants::SESSION_METADATA_FILE);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let meta = serde_json::from_slice(&bytes).map_err(|e| {
                OrchestratorError::SessionBindFailed {
                    role: AgentRole::Executor,
                    provider: provider_for_error,
                    reason: format!(
                        "failed to parse {}: {e}",
                        crate::constants::SESSION_METADATA_FILE
                    ),
                }
            })?;
            Ok(Some(meta))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Validate loaded `SessionMetadata` against CLI-supplied parameters.
pub fn validate_session_metadata(
    meta: &SessionMetadata,
    executor_thread_name: &str,
    reviewer_thread_name: &str,
    workspace_root: &str,
    executor_provider: ProviderKind,
    reviewer_provider: ProviderKind,
) -> OrchestratorResult<()> {
    if meta.executor.session_title != executor_thread_name {
        return Err(OrchestratorError::SessionBindFailed {
            role: AgentRole::Executor,
            provider: executor_provider,
            reason: format!(
                "executor_thread_name mismatch: metadata='{}' cli='{executor_thread_name}'",
                meta.executor.session_title
            ),
        });
    }
    if meta.reviewer.session_title != reviewer_thread_name {
        return Err(OrchestratorError::SessionBindFailed {
            role: AgentRole::Reviewer,
            provider: reviewer_provider,
            reason: format!(
                "reviewer_thread_name mismatch: metadata='{}' cli='{reviewer_thread_name}'",
                meta.reviewer.session_title
            ),
        });
    }
    if meta.workspace_root != workspace_root {
        return Err(OrchestratorError::SessionBindFailed {
            role: AgentRole::Executor,
            provider: executor_provider,
            reason: format!(
                "workspace_root mismatch: metadata='{}' cli='{workspace_root}'",
                meta.workspace_root
            ),
        });
    }
    if meta.executor.provider != executor_provider {
        return Err(OrchestratorError::SessionBindFailed {
            role: AgentRole::Executor,
            provider: executor_provider,
            reason: format!(
                "executor provider mismatch: metadata={} cli={}",
                meta.executor.provider, executor_provider
            ),
        });
    }
    if meta.reviewer.provider != reviewer_provider {
        return Err(OrchestratorError::SessionBindFailed {
            role: AgentRole::Reviewer,
            provider: reviewer_provider,
            reason: format!(
                "reviewer provider mismatch: metadata={} cli={}",
                meta.reviewer.provider, reviewer_provider
            ),
        });
    }
    if meta.executor.provider == meta.reviewer.provider
        && meta.executor.session_id == meta.reviewer.session_id
    {
        return Err(OrchestratorError::SessionBindFailed {
            role: AgentRole::Reviewer,
            provider: reviewer_provider,
            reason: "executor and reviewer resolved to the same (provider, session_id)".to_owned(),
        });
    }
    Ok(())
}

/// Write `SessionMetadata` to disk atomically and commit only that file.
///
/// Returns `Some(commit_hash)` when a commit was created, `None` when the file was
/// already current and committed (nothing to commit).
pub async fn write_and_commit_session_metadata(
    repo: impl AsRef<Path>,
    meta: &SessionMetadata,
) -> OrchestratorResult<Option<String>> {
    let repo = repo.as_ref();
    let meta_path = repo.join(crate::constants::SESSION_METADATA_FILE);
    let tmp_path = repo.join(format!("{}.tmp", crate::constants::SESSION_METADATA_FILE));

    // Write atomically via rename
    let json = serde_json::to_vec_pretty(meta)?;
    tokio::fs::write(&tmp_path, &json).await?;
    tokio::fs::rename(&tmp_path, &meta_path).await?;

    commit_session_metadata(repo).await
}

/// Create a git commit for the session metadata file only.
///
/// Returns `Some(hash)` on a new commit, `None` when there was nothing to commit.
async fn commit_session_metadata(repo: &Path) -> OrchestratorResult<Option<String>> {
    // Abort if the index is dirty (staged files unrelated to metadata)
    let index_output = run_git(repo, &["diff", "--cached", "--name-only"]).await?;
    if !index_output.trim().is_empty() {
        return Err(OrchestratorError::DirtyWorktree {
            status: format!("staged files before metadata commit:\n{index_output}"),
        });
    }

    // Check whether the metadata file changed vs HEAD
    let diff_output = run_git(
        repo,
        &[
            "diff",
            "--name-only",
            "HEAD",
            "--",
            crate::constants::SESSION_METADATA_FILE,
        ],
    )
    .await;

    let is_tracked = run_git(
        repo,
        &[
            "ls-files",
            "--error-unmatch",
            crate::constants::SESSION_METADATA_FILE,
        ],
    )
    .await
    .is_ok();

    let has_diff = diff_output.map(|s| !s.trim().is_empty()).unwrap_or(true);

    if !has_diff && is_tracked {
        return Ok(None); // nothing to commit
    }

    // Stage only the metadata file
    run_git(
        repo,
        &["add", "--", crate::constants::SESSION_METADATA_FILE],
    )
    .await?;

    // Verify that only the metadata file is staged
    let staged = run_git(repo, &["diff", "--cached", "--name-only"]).await?;
    let staged_files: Vec<&str> = staged.lines().filter(|l| !l.trim().is_empty()).collect();
    if staged_files
        .iter()
        .any(|f| *f != crate::constants::SESSION_METADATA_FILE)
    {
        // Unstage so the index is not left dirty on error.
        let _ = run_git(
            repo,
            &[
                "reset",
                "HEAD",
                "--",
                crate::constants::SESSION_METADATA_FILE,
            ],
        )
        .await;
        return Err(OrchestratorError::DirtyWorktree {
            status: format!("unexpected staged files for metadata commit:\n{staged}"),
        });
    }

    let commit_result = run_git(
        repo,
        &[
            "commit",
            "-m",
            "Record orchestrator session bindings",
            "--",
            crate::constants::SESSION_METADATA_FILE,
        ],
    )
    .await;

    if let Err(e) = commit_result {
        // Roll back the staged file so the index is clean for the next attempt.
        let _ = run_git(
            repo,
            &[
                "reset",
                "HEAD",
                "--",
                crate::constants::SESSION_METADATA_FILE,
            ],
        )
        .await;
        return Err(e);
    }

    let hash = run_git(repo, &["rev-parse", "HEAD"]).await?;
    Ok(Some(hash.trim().to_owned()))
}

/// Run a git command in `repo`, returning stdout on success.
pub async fn run_git(repo: &Path, args: &[&str]) -> OrchestratorResult<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .await?;

    if !output.status.success() {
        return Err(OrchestratorError::CommandFailed {
            program: format!("git {}", args.join(" ")),
            status: output.status,
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Session discovery helpers
// ---------------------------------------------------------------------------

/// Derive the Claude project key from a workspace root path.
///
/// Replaces every `/` in the absolute path with `-`.
pub fn claude_project_key(workspace_root: impl AsRef<Path>) -> String {
    workspace_root.as_ref().to_string_lossy().replace('/', "-")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_project_key_replaces_slashes() {
        assert_eq!(
            claude_project_key("/home/user/projects/app-core"),
            "-home-user-projects-app-core"
        );
    }

    #[tokio::test]
    async fn load_session_metadata_parse_error_is_session_bind_failed() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join(crate::constants::SESSION_METADATA_FILE),
            b"{not json",
        )
        .await
        .unwrap();

        let result = load_session_metadata(dir.path(), ProviderKind::Claude).await;

        assert!(matches!(
            result,
            Err(OrchestratorError::SessionBindFailed { .. })
        ));
    }

    fn make_meta(
        exec_thread: &str,
        rev_thread: &str,
        exec_id: &str,
        rev_id: &str,
    ) -> SessionMetadata {
        SessionMetadata {
            workspace_root: "/repo".to_owned(),
            executor: RoleSessionRecord {
                provider: ProviderKind::Claude,
                session_id: exec_id.to_owned(),
                session_title: exec_thread.to_owned(),
                discovery_source: "test".to_owned(),
                discovered_at: Utc::now(),
            },
            reviewer: RoleSessionRecord {
                provider: ProviderKind::Opencode,
                session_id: rev_id.to_owned(),
                session_title: rev_thread.to_owned(),
                discovery_source: "test".to_owned(),
                discovered_at: Utc::now(),
            },
        }
    }

    #[test]
    fn validate_metadata_detects_workspace_mismatch() {
        let meta = make_meta("TASK-1234", "TASK-1234-rev", "exec-session", "rev-session");
        let result = validate_session_metadata(
            &meta,
            "TASK-1234",
            "TASK-1234-rev",
            "/other-repo",
            ProviderKind::Claude,
            ProviderKind::Opencode,
        );
        assert!(result.is_err());
    }

    #[test]
    fn validate_metadata_detects_executor_thread_name_mismatch() {
        let meta = make_meta("TASK-1234", "TASK-1234-rev", "exec-session", "rev-session");
        let result = validate_session_metadata(
            &meta,
            "WRONG",
            "TASK-1234-rev",
            "/repo",
            ProviderKind::Claude,
            ProviderKind::Opencode,
        );
        assert!(result.is_err());
    }

    #[test]
    fn validate_metadata_detects_reviewer_thread_name_mismatch() {
        let meta = make_meta("TASK-1234", "TASK-1234-rev", "exec-session", "rev-session");
        let result = validate_session_metadata(
            &meta,
            "TASK-1234",
            "WRONG",
            "/repo",
            ProviderKind::Claude,
            ProviderKind::Opencode,
        );
        assert!(result.is_err());
    }

    #[test]
    fn validate_metadata_allows_same_session_id_across_different_providers() {
        let meta = make_meta("T", "T-rev", "same-id", "same-id");
        let result = validate_session_metadata(
            &meta,
            "T",
            "T-rev",
            "/repo",
            ProviderKind::Claude,
            ProviderKind::Opencode,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_metadata_detects_same_provider_session_id() {
        let mut meta = make_meta("T", "T-rev", "same-id", "same-id");
        meta.reviewer.provider = ProviderKind::Claude;
        let result = validate_session_metadata(
            &meta,
            "T",
            "T-rev",
            "/repo",
            ProviderKind::Claude,
            ProviderKind::Claude,
        );
        assert!(matches!(
            result,
            Err(OrchestratorError::SessionBindFailed { .. })
        ));
    }

    #[test]
    fn validate_metadata_accepts_valid_binding() {
        let meta = make_meta("TASK-1", "TASK-1-rev", "exec-session", "rev-session");
        let result = validate_session_metadata(
            &meta,
            "TASK-1",
            "TASK-1-rev",
            "/repo",
            ProviderKind::Claude,
            ProviderKind::Opencode,
        );
        assert!(result.is_ok());
    }
}
