use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::errors::{OrchestratorError, OrchestratorResult};
use crate::state::ProviderKind;
use crate::transport::sha256_hex;

/// Acquire an exclusive OS-level file lock for the given `(provider, session_id)` pair.
///
/// Lock path: `$XDG_RUNTIME_DIR/multi-agent-orchestrator/session-locks/<hash>.lock`
/// or `$HOME/.cache/multi-agent-orchestrator/session-locks/<hash>.lock`.
///
/// Returns the open lock file (drop to release).
pub fn acquire_session_lock(
    provider: ProviderKind,
    session_id: &str,
) -> OrchestratorResult<fs::File> {
    let base = lock_file_path(provider, session_id)?;
    fs::create_dir_all(base.parent().expect("lock path has parent"))?;

    // Open without truncating — do not zero the PID before the lock is acquired.
    let lock = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&base)?;

    fs2::FileExt::try_lock_exclusive(&lock).map_err(|err| {
        if err.kind() == std::io::ErrorKind::WouldBlock {
            OrchestratorError::SessionLocked {
                provider,
                session_id: session_id.to_owned(),
            }
        } else {
            OrchestratorError::Io { source: err }
        }
    })?;

    // Lock acquired — safe to record PID for diagnostics.
    lock.set_len(0)?;
    write!(&lock, "{}", std::process::id())?;

    Ok(lock)
}

/// Returns the expected lock file path for `(provider, session_id)`.
fn lock_file_path(provider: ProviderKind, session_id: &str) -> OrchestratorResult<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or_else(|| OrchestratorError::InvalidInput {
            field: "lock_dir".to_owned(),
            reason: "XDG_RUNTIME_DIR and HOME are both unset".to_owned(),
        })?;

    let provider = provider.as_str();
    let key = sha256_hex(format!("{provider}:{session_id}").as_bytes());
    Ok(base
        .join("multi-agent-orchestrator")
        .join("session-locks")
        .join(format!("{key}.lock")))
}
