use std::path::PathBuf;
use std::process::ExitStatus;

use crate::state::{AgentRole, ProviderKind};

/// Canonical orchestrator error type.
///
/// ## Error handling policy
/// - `anyhow` is forbidden; all errors use typed variants with `thiserror`.
/// - Every public function returns `OrchestratorResult<T>` or a narrower typed error.
/// - Every terminal variant maps to exactly one canonical `ReasonCode` in `report.rs`.
/// - Low-level provider/process/parser errors must be converted into explicit variants
///   before crossing module boundaries; raw `io::Error` may propagate via `Io { #[from] }`.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("transport reset failed for {path}")]
    TransportResetFailed { path: PathBuf },

    #[error("io error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    #[error("json error: {source}")]
    Json {
        #[from]
        source: serde_json::Error,
    },

    #[error("yaml parse error: {source}")]
    Yaml {
        #[from]
        source: serde_yaml::Error,
    },

    #[error("command `{program}` failed: {status}")]
    CommandFailed { program: String, status: ExitStatus },

    #[error("session bind failed for {role:?} {provider:?}: {reason}")]
    SessionBindFailed {
        role: AgentRole,
        provider: ProviderKind,
        reason: String,
    },

    #[error("session is locked: {provider:?} {session_id}")]
    SessionLocked {
        provider: ProviderKind,
        session_id: String,
    },

    #[error("invalid input `{field}`: {reason}")]
    InvalidInput { field: String, reason: String },

    #[error("dirty worktree before orchestration:\n{status}")]
    DirtyWorktree { status: String },

    #[error("artifact contract failed: {contract}")]
    ArtifactContract { contract: String },

    #[error("request payload changed before retry — aborting retry")]
    RequestChangedAbortRetry,

    #[error("reviewer protocol violation: {detail}")]
    ReviewerProtocolViolation { detail: String },
}

/// Top-level result type for all public orchestrator functions.
pub type OrchestratorResult<T> = Result<T, OrchestratorError>;
