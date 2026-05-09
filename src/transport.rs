use std::path::{Path, PathBuf};

use crate::errors::{OrchestratorError, OrchestratorResult};
use crate::state::AgentRole;

/// Reset transport files before dispatch.
///
/// Executor dispatch: truncates both `inbox.txt` and `outbox.txt` and verifies size is 0.
/// Reviewer dispatch: truncates only `inbox.txt`; preserves executor `outbox.txt`.
pub async fn reset_transport(repo: impl AsRef<Path>, role: AgentRole) -> OrchestratorResult<()> {
    let repo = repo.as_ref();
    let transport_dir = repo.join(crate::constants::TRANSPORT_DIR);
    tokio::fs::create_dir_all(&transport_dir).await?;

    let inbox: PathBuf = transport_dir.join(crate::constants::INBOX_FILE);
    let outbox: PathBuf = transport_dir.join(crate::constants::OUTBOX_FILE);

    let paths: Vec<PathBuf> = match role {
        AgentRole::Executor => vec![inbox, outbox],
        AgentRole::Reviewer => vec![inbox],
    };

    for path in paths {
        // Truncate to zero bytes (creates if absent)
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .await?;

        let meta = tokio::fs::metadata(&path).await?;
        if meta.len() != 0 {
            return Err(OrchestratorError::TransportResetFailed { path });
        }
    }

    Ok(())
}

/// Render a template, write it to `inbox.txt`, and return the SHA-256 fingerprint.
pub async fn write_request(repo: impl AsRef<Path>, content: &str) -> OrchestratorResult<String> {
    let inbox = repo
        .as_ref()
        .join(crate::constants::TRANSPORT_DIR)
        .join(crate::constants::INBOX_FILE);
    tokio::fs::write(&inbox, content).await?;
    let bytes = tokio::fs::read(&inbox).await?;
    Ok(sha256_hex(&bytes))
}

/// Returns the hex-encoded SHA-256 digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Read a file's size and mtime, returning `None` when the file does not exist.
pub async fn file_meta(
    path: impl AsRef<Path>,
) -> OrchestratorResult<Option<crate::state::FileMeta>> {
    match tokio::fs::metadata(path.as_ref()).await {
        Ok(m) => {
            let mtime = m.modified()?;
            Ok(Some(crate::state::FileMeta {
                size: m.len(),
                mtime,
            }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Verify the current fingerprint of `inbox.txt` against the original dispatch fingerprint.
/// Returns `Err(RequestChangedAbortRetry)` when the payload changed.
pub async fn verify_request_fingerprint(
    repo: impl AsRef<Path>,
    original_sha: &str,
) -> OrchestratorResult<()> {
    let inbox = repo
        .as_ref()
        .join(crate::constants::TRANSPORT_DIR)
        .join(crate::constants::INBOX_FILE);
    let bytes = tokio::fs::read(&inbox).await?;
    let current = sha256_hex(&bytes);
    if current != original_sha {
        return Err(OrchestratorError::RequestChangedAbortRetry);
    }
    Ok(())
}

/// Ensure `.agent-io/` is listed in `.git/info/exclude`, appending it if absent.
///
/// Uses `git rev-parse --git-path info/exclude` to resolve the real path, which
/// handles both normal repos (`.git/info/exclude`) and worktrees where `.git` is
/// a file pointing to a separate gitdir.
pub async fn ensure_agent_io_excluded(repo: impl AsRef<Path>) -> OrchestratorResult<()> {
    let repo = repo.as_ref();

    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--git-path", "info/exclude"])
        .current_dir(repo)
        .output()
        .await?;
    if !output.status.success() {
        return Err(OrchestratorError::CommandFailed {
            program: "git rev-parse --git-path info/exclude".to_owned(),
            status: output.status,
        });
    }
    let relative = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let exclude_path = if std::path::Path::new(&relative).is_absolute() {
        std::path::PathBuf::from(relative)
    } else {
        repo.join(relative)
    };

    let entry = ".agent-io/";

    let content = match tokio::fs::read_to_string(&exclude_path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };

    if !content.lines().any(|line| line.trim() == entry) {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&exclude_path)
            .await?;
        file.write_all(format!("{entry}\n").as_bytes()).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_known_value() {
        // echo -n "hello" | sha256sum
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(sha256_hex(b"hello"), expected);
    }

    #[tokio::test]
    async fn reset_transport_truncates_files() {
        let dir = tempfile::tempdir().unwrap();
        let transport = dir.path().join(".agent-io");
        tokio::fs::create_dir_all(&transport).await.unwrap();

        // Pre-populate outbox with data
        tokio::fs::write(transport.join("outbox.txt"), b"some data")
            .await
            .unwrap();

        reset_transport(dir.path(), AgentRole::Executor)
            .await
            .unwrap();

        let inbox_size = tokio::fs::metadata(transport.join("inbox.txt"))
            .await
            .unwrap()
            .len();
        let outbox_size = tokio::fs::metadata(transport.join("outbox.txt"))
            .await
            .unwrap()
            .len();
        assert_eq!(inbox_size, 0);
        assert_eq!(outbox_size, 0);
    }

    #[tokio::test]
    async fn reset_transport_reviewer_preserves_outbox() {
        let dir = tempfile::tempdir().unwrap();
        let transport = dir.path().join(".agent-io");
        tokio::fs::create_dir_all(&transport).await.unwrap();
        tokio::fs::write(transport.join("outbox.txt"), b"executor output")
            .await
            .unwrap();
        tokio::fs::write(transport.join("inbox.txt"), b"old inbox")
            .await
            .unwrap();

        reset_transport(dir.path(), AgentRole::Reviewer)
            .await
            .unwrap();

        let outbox = tokio::fs::read(transport.join("outbox.txt")).await.unwrap();
        let inbox_size = tokio::fs::metadata(transport.join("inbox.txt"))
            .await
            .unwrap()
            .len();
        assert_eq!(outbox, b"executor output");
        assert_eq!(inbox_size, 0);
    }

    #[tokio::test]
    async fn write_request_returns_sha() {
        let dir = tempfile::tempdir().unwrap();
        let transport = dir.path().join(".agent-io");
        tokio::fs::create_dir_all(&transport).await.unwrap();
        tokio::fs::write(transport.join("inbox.txt"), b"")
            .await
            .unwrap();

        let sha = write_request(dir.path(), "hello world").await.unwrap();
        assert_eq!(sha.len(), 64);

        // Verify fingerprint check
        verify_request_fingerprint(dir.path(), &sha).await.unwrap();

        // Tamper with inbox
        tokio::fs::write(transport.join("inbox.txt"), b"tampered")
            .await
            .unwrap();
        let result = verify_request_fingerprint(dir.path(), &sha).await;
        assert!(matches!(
            result,
            Err(OrchestratorError::RequestChangedAbortRetry)
        ));
    }
}
