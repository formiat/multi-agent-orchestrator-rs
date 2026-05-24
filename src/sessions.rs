use std::path::Path;

use crate::errors::{OrchestratorError, OrchestratorResult};

// ---------------------------------------------------------------------------
// Session binding contract
//
// The orchestrator only discovers and reuses EXISTING provider sessions.
// Creating a new session/chat is strictly forbidden for all providers.
// Every run performs fresh deterministic discovery from provider state.
// If deterministic bind fails (no match or ambiguous tie), stop with
// RUN_FAILED_SESSION_BIND — never silently fall back to a new session.
//
// Executor and reviewer must not share the same (provider, thread_name) pair.
// Same pair means the same session, which would mix executor and reviewer context.
// Different providers with the same thread name are allowed.
// Same provider with different thread names are allowed.
//
// Session discovery requires exactly one match per role: zero matches → no session
// found; two or more matches → ambiguous, user must rename or delete sessions.
// ---------------------------------------------------------------------------

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

/// Derive the Claude project key from a workspace root path.
///
/// Replaces every `/` in the absolute path with `-`.
pub fn claude_project_key(workspace_root: impl AsRef<Path>) -> String {
    workspace_root.as_ref().to_string_lossy().replace('/', "-")
}

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
}
