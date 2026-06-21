use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::constants::{CONSECUTIVE_FAILURE_LIMIT, PHASE_SEPARATOR_WAIT_SEC};
use crate::errors::{OrchestratorError, OrchestratorResult};
use crate::providers::dispatch;
use crate::report::TransportBodyReport;
use crate::sessions::{claude_project_key, run_git};
use crate::signals::format_git_facts;
use crate::state::{
    AgentRole, AttemptState, AttemptStateData, NotionPolicy, ProviderKind, RunState, TemplateId,
    TemplateValues, WorkflowType,
};
use crate::transport::{
    ensure_agent_io_excluded, reset_transport, verify_request_fingerprint, write_request,
};
use crate::workflow::{render_template, reviewer_yaml_schema, workflow_contract_text};
use crate::yaml_check::{parse_reviewer_yaml, ReviewDecision};

use super::OrchestratorCtx;

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

pub(super) fn validate_inputs(ctx: &mut OrchestratorCtx) -> OrchestratorResult<()> {
    if ctx.args.prompt.trim().is_empty() {
        return Err(OrchestratorError::InvalidInput {
            field: "--prompt".to_owned(),
            reason: "must be non-empty".to_owned(),
        });
    }
    if ctx.args.executor_thread_name.trim().is_empty() {
        return Err(OrchestratorError::InvalidInput {
            field: "--executor-thread-name".to_owned(),
            reason: "must be non-empty".to_owned(),
        });
    }
    if ctx.args.reviewer_thread_name.trim().is_empty() {
        return Err(OrchestratorError::InvalidInput {
            field: "--reviewer-thread-name".to_owned(),
            reason: "must be non-empty".to_owned(),
        });
    }
    validate_model_option(
        "--executor-model",
        ctx.args.executor_model.as_deref(),
        ctx.args.executor_provider,
    )?;
    validate_model_option(
        "--reviewer-model",
        ctx.args.reviewer_model.as_deref(),
        ctx.args.reviewer_provider,
    )?;
    // Executor and reviewer must not share the same (provider, thread_name) pair —
    // that would bind them to the same session and mix their contexts.
    if ctx.args.executor_provider == ctx.args.reviewer_provider
        && ctx.args.executor_thread_name == ctx.args.reviewer_thread_name
    {
        return Err(OrchestratorError::InvalidInput {
            field: "--reviewer-thread-name".to_owned(),
            reason: "executor and reviewer must not share the same (provider, thread_name) pair"
                .to_owned(),
        });
    }
    // Canonicalize accepts relative paths, symlinks, and `..` components.
    // Fails with InvalidInput if the path does not exist or cannot be resolved.
    let canonical =
        ctx.args
            .workspace_root
            .canonicalize()
            .map_err(|e| OrchestratorError::InvalidInput {
                field: "--workspace-root".to_owned(),
                reason: format!("cannot resolve path: {e}"),
            })?;
    ctx.args.workspace_root = canonical;
    if !ctx.args.workspace_root.is_dir() {
        return Err(OrchestratorError::InvalidInput {
            field: "--workspace-root".to_owned(),
            reason: "must be a directory".to_owned(),
        });
    }
    Ok(())
}

fn validate_model_option(
    field: &str,
    model: Option<&str>,
    provider: ProviderKind,
) -> OrchestratorResult<()> {
    let Some(model) = model else {
        return Ok(());
    };
    if model.trim().is_empty() {
        return Err(OrchestratorError::InvalidInput {
            field: field.to_owned(),
            reason: "must be non-empty when provided".to_owned(),
        });
    }
    if provider != ProviderKind::Opencode {
        return Err(OrchestratorError::InvalidInput {
            field: field.to_owned(),
            reason: format!(
                "model override is currently supported only for opencode, got {provider}"
            ),
        });
    }
    crate::providers::opencode::parse_model_provider(model).map_err(|reason| {
        OrchestratorError::InvalidInput {
            field: field.to_owned(),
            reason,
        }
    })?;
    Ok(())
}

pub(super) async fn validate_provider_models(ctx: &OrchestratorCtx) -> OrchestratorResult<()> {
    if let Some(model) = ctx.args.executor_model.as_deref() {
        crate::providers::opencode::ensure_model_available(ctx.repo(), model, "--executor-model")
            .await?;
    }
    if let Some(model) = ctx.args.reviewer_model.as_deref() {
        crate::providers::opencode::ensure_model_available(ctx.repo(), model, "--reviewer-model")
            .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase: CONTEXT_PREP
// ---------------------------------------------------------------------------

/// Build the deterministic context bundle before any session is bound or messaged.
///
/// ## Initial clean repository gate
/// The orchestrator must not start when the repository already has uncommitted work.
/// Staged, unstaged, deleted, renamed, and untracked files all count as dirty.
/// This check runs before any orchestrator write or provider dispatch.
///
/// ## No preliminary existing-result review
/// The orchestrator always starts by sending the first request to the executor.
/// It must not scan existing PLAN.md / INVESTIGATION.md / commits to decide whether
/// work is already done, and must not dispatch the reviewer before the first executor
/// attempt.  Existing artifacts are ordinary repository context passed verbatim to
/// the executor prompt.
pub(super) async fn do_context_prep(ctx: &mut OrchestratorCtx) -> OrchestratorResult<()> {
    let repo = ctx.repo();

    validate_git_worktree(repo).await?;
    validate_git_top_level(repo).await?;
    validate_no_git_locks(repo).await?;
    validate_no_unmerged_index(repo).await?;
    validate_no_git_operation_in_progress(repo).await?;

    // .agent-io/ must be in .git/info/exclude before the dirty-worktree check so that
    // transport files are never reported as untracked, even if the directory already
    // exists from a previous run.
    ensure_agent_io_excluded(repo).await?;

    let git_status = run_git(repo, &["status", "--short", "--untracked-files=all"]).await?;
    if !git_status.trim().is_empty() {
        return Err(OrchestratorError::DirtyWorktree { status: git_status });
    }

    let branch = current_branch_name(repo).await?;

    let initial_git_head = current_head_required(repo).await?;
    log_git_start_warnings(repo).await;

    let project_key = if ctx.args.executor_provider == ProviderKind::Claude
        || ctx.args.reviewer_provider == ProviderKind::Claude
    {
        Some(claude_project_key(repo))
    } else {
        None
    };

    ctx.branch = branch;
    ctx.initial_git_head = Some(initial_git_head);
    ctx.claude_project_key = project_key;
    ctx.run_state = RunState::SessionBind;

    tracing::info!(
        "context_prep done workspace_root={} workflow={} branch={} initial_head={} claude_project_key={}",
        ctx.repo().display(),
        ctx.args.workflow_type,
        ctx.branch,
        ctx.initial_git_head.as_deref().unwrap_or("<none>"),
        ctx.claude_project_key.as_deref().unwrap_or("<none>")
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase: SESSION_BIND
// ---------------------------------------------------------------------------

pub(super) async fn do_session_bind(ctx: &mut OrchestratorCtx) -> OrchestratorResult<()> {
    let repo = ctx.args.workspace_root.clone();
    let executor_thread_name = ctx.args.executor_thread_name.clone();
    let reviewer_thread_name = ctx.args.reviewer_thread_name.clone();

    let executor_session_id = discover_session(
        &repo,
        &executor_thread_name,
        AgentRole::Executor,
        ctx.args.executor_provider,
    )
    .await?;
    let reviewer_session_id = discover_session(
        &repo,
        &reviewer_thread_name,
        AgentRole::Reviewer,
        ctx.args.reviewer_provider,
    )
    .await?;

    if ctx.args.executor_provider == ctx.args.reviewer_provider
        && executor_session_id == reviewer_session_id
    {
        return Err(OrchestratorError::SessionBindFailed {
            role: AgentRole::Reviewer,
            provider: ctx.args.reviewer_provider,
            reason: "executor and reviewer resolved to the same (provider, session_id)".to_owned(),
        });
    }

    ctx.executor_session_id = Some(executor_session_id.clone());
    ctx.reviewer_session_id = Some(reviewer_session_id.clone());
    tracing::info!(
        "sessions discovered executor_provider={} executor_session={} reviewer_provider={} reviewer_session={}",
        ctx.args.executor_provider,
        executor_session_id,
        ctx.args.reviewer_provider,
        reviewer_session_id
    );

    ctx.run_state = RunState::ExecutorDispatch;
    phase_separator_wait().await;
    Ok(())
}

async fn validate_git_worktree(repo: &Path) -> OrchestratorResult<()> {
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(repo)
        .output()
        .await?;

    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != "true" {
        return Err(OrchestratorError::InvalidInput {
            field: "--workspace-root".to_owned(),
            reason: "must be an existing git worktree".to_owned(),
        });
    }

    Ok(())
}

async fn validate_git_top_level(repo: &Path) -> OrchestratorResult<()> {
    let top_level = run_git(repo, &["rev-parse", "--show-toplevel"]).await?;
    let top_level = canonicalize_existing_path("--workspace-root", top_level.trim())?;
    let repo = canonicalize_existing_path("--workspace-root", repo)?;

    if repo != top_level {
        return Err(OrchestratorError::InvalidInput {
            field: "--workspace-root".to_owned(),
            reason: format!(
                "must be the git repository top-level; got `{repo}`, top-level is `{top_level}`",
                repo = repo.display(),
                top_level = top_level.display()
            ),
        });
    }

    Ok(())
}

fn canonicalize_existing_path(field: &str, path: impl AsRef<Path>) -> OrchestratorResult<PathBuf> {
    path.as_ref()
        .canonicalize()
        .map_err(|e| OrchestratorError::InvalidInput {
            field: field.to_owned(),
            reason: format!("cannot resolve path `{}`: {e}", path.as_ref().display()),
        })
}

async fn validate_no_git_locks(repo: &Path) -> OrchestratorResult<()> {
    for git_path in ["index.lock", "HEAD.lock", "packed-refs.lock"] {
        let path = resolve_git_path(repo, git_path).await?;
        if path.exists() {
            return Err(git_state_invalid(format!(
                "git lock file exists: `{}`",
                path.display()
            )));
        }
    }

    let refs = resolve_git_path(repo, "refs").await?;
    if let Some(path) = find_lock_file(&refs)? {
        return Err(git_state_invalid(format!(
            "git refs lock file exists: `{}`",
            path.display()
        )));
    }

    Ok(())
}

async fn validate_no_git_operation_in_progress(repo: &Path) -> OrchestratorResult<()> {
    for (state, git_path) in [
        ("merge", "MERGE_HEAD"),
        ("rebase", "rebase-merge"),
        ("rebase", "rebase-apply"),
        ("cherry-pick", "CHERRY_PICK_HEAD"),
        ("revert", "REVERT_HEAD"),
        ("sequencer", "sequencer"),
        ("bisect", "BISECT_LOG"),
    ] {
        let path = resolve_git_path(repo, git_path).await?;
        if path.exists() {
            return Err(git_state_invalid(format!(
                "git operation in progress: {state} marker `{}` exists",
                path.display()
            )));
        }
    }

    Ok(())
}

async fn validate_no_unmerged_index(repo: &Path) -> OrchestratorResult<()> {
    let unmerged = run_git(repo, &["ls-files", "-u"]).await?;
    if unmerged.trim().is_empty() {
        return Ok(());
    }

    let details = unmerged.lines().take(5).collect::<Vec<_>>().join("; ");
    Err(git_state_invalid(format!(
        "git index contains unmerged/conflicted entries: {details}"
    )))
}

async fn resolve_git_path(repo: &Path, path: &str) -> OrchestratorResult<PathBuf> {
    let path = run_git(repo, &["rev-parse", "--git-path", path]).await?;
    let path = PathBuf::from(path.trim());
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(repo.join(path))
    }
}

fn find_lock_file(path: &Path) -> OrchestratorResult<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(path) = find_lock_file(&path)? {
                return Ok(Some(path));
            }
        } else if path
            .extension()
            .is_some_and(|extension| extension == "lock")
        {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

fn git_state_invalid(reason: String) -> OrchestratorError {
    OrchestratorError::InvalidInput {
        field: "--workspace-root".to_owned(),
        reason,
    }
}

async fn current_branch_name(repo: &Path) -> OrchestratorResult<String> {
    let symbolic = tokio::process::Command::new("git")
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(repo)
        .output()
        .await?;

    if symbolic.status.success() {
        let branch = String::from_utf8_lossy(&symbolic.stdout).trim().to_owned();
        if !branch.is_empty() {
            return Ok(branch);
        }
    }

    Err(git_state_invalid(
        "workspace must be on a named branch; detached HEAD is unsupported".to_owned(),
    ))
}

async fn current_head_required(repo: &Path) -> OrchestratorResult<String> {
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(repo)
        .output()
        .await?;

    if output.status.success() {
        let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if head.is_empty() {
            return Err(OrchestratorError::CommandFailed {
                program: "git rev-parse --verify HEAD".to_owned(),
                status: output.status,
            });
        }
        return Ok(head);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("Needed a single revision") || stderr.contains("unknown revision") {
        return Err(git_state_invalid(
            "workspace must have a valid HEAD commit; unborn branches are unsupported".to_owned(),
        ));
    }

    Err(OrchestratorError::CommandFailed {
        program: "git rev-parse --verify HEAD".to_owned(),
        status: output.status,
    })
}

async fn log_git_start_warnings(repo: &Path) {
    let upstream = try_run_git(
        repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .await
    .map(|upstream| upstream.trim().to_owned())
    .filter(|upstream| !upstream.is_empty());

    if let Some(upstream) = upstream {
        if let Some(counts) = try_run_git(
            repo,
            &["rev-list", "--left-right", "--count", "HEAD...@{u}"],
        )
        .await
        {
            let counts = counts.trim();
            let mut parts = counts.split_whitespace();
            let ahead = parts.next().and_then(|value| value.parse::<u32>().ok());
            let behind = parts.next().and_then(|value| value.parse::<u32>().ok());
            if let (Some(ahead), Some(behind)) = (ahead, behind) {
                if ahead > 0 || behind > 0 {
                    tracing::warn!(
                        "git startup warning: branch differs from upstream={} ahead={} behind={}",
                        upstream,
                        ahead,
                        behind
                    );
                }
            }
        }
    } else {
        tracing::warn!("git startup warning: branch has no upstream");
    }

    if try_run_git(repo, &["rev-parse", "--is-shallow-repository"])
        .await
        .is_some_and(|value| value.trim() == "true")
    {
        tracing::warn!("git startup warning: repository is shallow");
    }

    if try_run_git(repo, &["config", "--bool", "core.sparseCheckout"])
        .await
        .is_some_and(|value| value.trim() == "true")
    {
        tracing::warn!("git startup warning: sparse checkout is enabled");
    }
}

async fn try_run_git(repo: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .await
        .ok()?;

    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Discover a session for the given provider and role.
async fn discover_session(
    repo: &std::path::Path,
    thread_name: &str,
    role: AgentRole,
    provider: ProviderKind,
) -> OrchestratorResult<String> {
    match provider {
        ProviderKind::Claude => {
            crate::providers::claude::discover_by_thread(repo, thread_name, role)
        }
        ProviderKind::Opencode => {
            crate::providers::opencode::discover_by_thread(repo, thread_name, role).await
        }
        ProviderKind::Codex => crate::providers::codex::discover_by_thread(repo, thread_name, role),
    }
}

// ---------------------------------------------------------------------------
// Phase: EXECUTOR_DISPATCH / REVIEWER_DISPATCH
// ---------------------------------------------------------------------------

pub(super) async fn do_executor_dispatch(ctx: &mut OrchestratorCtx) -> OrchestratorResult<()> {
    dispatch_role(ctx, AgentRole::Executor).await?;
    ctx.run_state = RunState::ExecutorMonitor;
    phase_separator_wait().await;
    Ok(())
}

pub(super) async fn do_reviewer_dispatch(ctx: &mut OrchestratorCtx) -> OrchestratorResult<()> {
    dispatch_role(ctx, AgentRole::Reviewer).await?;
    ctx.run_state = RunState::ReviewerMonitor;
    phase_separator_wait().await;
    Ok(())
}

async fn dispatch_role(ctx: &mut OrchestratorCtx, role: AgentRole) -> OrchestratorResult<()> {
    let provider = match role {
        AgentRole::Executor => ctx.args.executor_provider,
        AgentRole::Reviewer => ctx.args.reviewer_provider,
    };
    let model = match role {
        AgentRole::Executor => ctx.args.executor_model.as_deref(),
        AgentRole::Reviewer => ctx.args.reviewer_model.as_deref(),
    }
    .map(str::to_owned);
    let session_id = ctx
        .session_id(role)
        .ok_or_else(|| OrchestratorError::InvalidInput {
            field: "session_id".to_owned(),
            reason: format!("{role:?} session not bound"),
        })?
        .to_owned();

    let lock = crate::locks::acquire_session_lock(provider, &session_id)?;
    match role {
        AgentRole::Executor => ctx.executor_lock = Some(lock),
        AgentRole::Reviewer => ctx.reviewer_lock = Some(lock),
    }

    // On retry, verify inbox.txt hasn't been externally modified since the previous dispatch.
    if let Some(prev) = ctx.attempt.as_ref() {
        verify_request_fingerprint(ctx.repo(), &prev.request_fingerprint).await?;
    }

    // Capture baseline git state before dispatch (used by grace-period and work-signal logic).
    let status_out = run_git(ctx.repo(), &["status", "--short"]).await?;
    let dispatch_git_status_hash = crate::transport::sha256_hex(status_out.as_bytes());
    let dispatch_git_status_lines = git_status_line_set(&status_out);
    let dispatch_git_head_hash = crate::signals::git_head_hash(ctx.repo()).await?;
    reset_attempt_git_telemetry(ctx, dispatch_git_status_lines.len());

    reset_transport(ctx.repo(), role).await?;

    // For reviewer: snapshot outbox mtime and git status immediately before dispatch.
    // pre_reviewer_outbox_mtime must be carried into the NEW attempt (not the old executor
    // attempt) because ctx.attempt is replaced below when the reviewer is spawned.
    // The git status snapshot is the correct baseline for reviewer workspace checks:
    // it reflects the worktree after executor finished, not the clean state from CONTEXT_PREP.
    // During workspace-cleanup retry, keep the original baseline so reviewer leftovers do not
    // become the new accepted state.
    let pre_reviewer_outbox_mtime = if role == AgentRole::Reviewer {
        let outbox_path = ctx
            .repo()
            .join(crate::constants::TRANSPORT_DIR)
            .join(crate::constants::OUTBOX_FILE);
        let mtime = optional_file_mtime(&outbox_path).await?;
        if ctx.reviewer_workspace_rejection.is_none() {
            ctx.pre_reviewer_git_status =
                Some(run_git(ctx.repo(), &["status", "--short", "--untracked-files=all"]).await?);
        }
        mtime
    } else {
        None
    };

    let template_id = if role == AgentRole::Executor {
        if ctx.review_result.is_some() {
            TemplateId::ExecutorFeedback
        } else {
            TemplateId::ExecutorInitial
        }
    } else if ctx.reviewer_workspace_rejection.is_some() {
        TemplateId::ReviewerRepairWorkspace
    } else if ctx.reviewer_yaml_rejection.is_some() {
        TemplateId::ReviewerRepairYaml
    } else {
        TemplateId::ReviewerReview
    };

    let values = build_template_values(ctx, role).await?;
    let prompt_text = render_template(template_id, &values);
    let fingerprint = write_request(ctx.repo(), &prompt_text).await?;
    let inbox_path = ctx
        .repo()
        .join(crate::constants::TRANSPORT_DIR)
        .join(crate::constants::INBOX_FILE);
    ctx.last_inbox_snapshot =
        read_transport_snapshot(&inbox_path, role, provider, &session_id).await?;
    if let Some(snapshot) = &ctx.last_inbox_snapshot {
        log_transport_snapshot("inbox", snapshot);
    }

    let outbox_path = ctx
        .repo()
        .join(crate::constants::TRANSPORT_DIR)
        .join(crate::constants::OUTBOX_FILE);
    let trigger_prompt = crate::constants::trigger_prompt(&inbox_path, &outbox_path);
    let process = dispatch(
        provider,
        &session_id,
        ctx.repo(),
        &trigger_prompt,
        model.as_deref(),
    )
    .await?;
    let pid = process.child.id();

    let now = Utc::now();
    ctx.attempt = Some(AttemptStateData {
        state: AttemptState::Dispatching,
        role,
        dispatch_ts: now,
        pid,
        exit_code: None,
        request_fingerprint: fingerprint.clone(),
        last_work_signal_ts: Some(now),
        grace_until_commit: None,
        grace_until_outbox: None,
        pre_reviewer_outbox_mtime,
        next_probe_at: now,
        dispatch_git_status_hash: dispatch_git_status_hash.clone(),
        prev_probe_git_status_hash: dispatch_git_status_hash,
        prev_probe_git_status_lines: dispatch_git_status_lines,
        dispatch_git_head_hash: dispatch_git_head_hash.clone(),
        prev_probe_git_head_hash: dispatch_git_head_hash,
        prev_probe_outbox_meta: None,
        prev_probe_log_mtime: None,
        provider_log_ever_seen: false,
        last_heartbeat_ts: None,
    });
    ctx.active_process = Some(process);
    ctx.current_role = Some(role);
    ctx.last_provider_action = None;
    ctx.last_provider_action_ts = None;

    tracing::info!(
        "dispatched {role:?} via {provider} session={session_id} template={template_id:?} request_sha256={fingerprint} pid={pid:?}"
    );
    Ok(())
}

fn git_status_line_set(raw: &str) -> BTreeSet<String> {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn reset_attempt_git_telemetry(ctx: &mut OrchestratorCtx, dirty_files_count: usize) {
    ctx.dirty_file_touch_counts.clear();
    ctx.committed_file_touch_counts.clear();
    ctx.last_dirty_files_count = dirty_files_count;
    ctx.committed_files_seen_count = 0;
}

async fn build_template_values(
    ctx: &OrchestratorCtx,
    role: AgentRole,
) -> OrchestratorResult<TemplateValues> {
    let workflow_type = ctx.args.workflow_type;
    let provider = match role {
        AgentRole::Executor => ctx.args.executor_provider,
        AgentRole::Reviewer => ctx.args.reviewer_provider,
    };
    let git_facts = format_git_facts(ctx.repo(), ctx.initial_git_head.as_deref()).await?;
    let transport_dir = ctx.repo().join(crate::constants::TRANSPORT_DIR);
    let inbox_path = transport_dir.join(crate::constants::INBOX_FILE);
    let outbox_path = transport_dir.join(crate::constants::OUTBOX_FILE);
    let orchestrator_docs_dir = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("docs");

    let review_result_yaml = ctx.review_result_yaml_raw.clone();
    let feedback_for_executor = ctx.review_result.as_ref().map(|r| {
        r.feedback_for_executor
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {s}", i + 1))
            .collect::<Vec<_>>()
            .join("\n")
    });
    Ok(TemplateValues {
        provider,
        workflow_type,
        workspace_root: ctx.repo().to_string_lossy().into_owned(),
        transport_dir: transport_dir.to_string_lossy().into_owned(),
        inbox_path: inbox_path.to_string_lossy().into_owned(),
        outbox_path: outbox_path.to_string_lossy().into_owned(),
        orchestrator_docs_dir: orchestrator_docs_dir.to_string_lossy().into_owned(),
        branch: ctx.branch.clone(),
        user_prompt: ctx.args.prompt.clone(),
        notion_policy: ctx.args.notion_policy,
        remote_network_policy: ctx.args.remote_network_policy,
        workflow_contract: workflow_contract_text(workflow_type).to_owned(),
        git_facts,
        executor_outbox_present: ctx.outbox_present,
        reviewer_yaml_schema: if role == AgentRole::Reviewer {
            Some(reviewer_yaml_schema().to_owned())
        } else {
            None
        },
        reviewer_yaml_rejection: if role == AgentRole::Reviewer {
            ctx.reviewer_yaml_rejection.clone()
        } else {
            None
        },
        reviewer_workspace_rejection: if role == AgentRole::Reviewer {
            ctx.reviewer_workspace_rejection.clone()
        } else {
            None
        },
        review_result_yaml,
        feedback_for_executor,
    })
}

// ---------------------------------------------------------------------------
// Phase: ROUND_RETRY_DECIDE
// ---------------------------------------------------------------------------

pub(super) fn do_round_retry_decide(ctx: &mut OrchestratorCtx) -> OrchestratorResult<()> {
    if ctx.consecutive_failure_count >= CONSECUTIVE_FAILURE_LIMIT {
        if let Some(attempt) = ctx.attempt.as_mut() {
            attempt.state = AttemptState::RetryExhausted;
        }
        ctx.run_state = RunState::RunFailedConsecutiveFailureLimit;
        ctx.failures.push(format!(
            "consecutive failure limit reached ({}/{})",
            ctx.consecutive_failure_count, CONSECUTIVE_FAILURE_LIMIT
        ));
        return Ok(());
    }

    if let Some(attempt) = ctx.attempt.as_mut() {
        attempt.state = AttemptState::RetryPending;
    }

    let role = ctx.current_role.unwrap_or(AgentRole::Executor);
    ctx.run_state = match role {
        AgentRole::Executor => RunState::ExecutorDispatch,
        AgentRole::Reviewer => RunState::ReviewerDispatch,
    };

    tracing::info!(
        "retry {role:?} (failure count={})",
        ctx.consecutive_failure_count
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase: EXECUTOR_OUTPUT_COLLECT
// ---------------------------------------------------------------------------

pub(super) async fn do_executor_output_collect(
    ctx: &mut OrchestratorCtx,
) -> OrchestratorResult<()> {
    let outbox_path = ctx
        .repo()
        .join(crate::constants::TRANSPORT_DIR)
        .join(crate::constants::OUTBOX_FILE);

    let executor_session_id = ctx
        .executor_session_id
        .as_deref()
        .unwrap_or("<unbound>")
        .to_owned();
    ctx.executor_outbox_snapshot = read_transport_snapshot(
        &outbox_path,
        AgentRole::Executor,
        ctx.args.executor_provider,
        &executor_session_id,
    )
    .await?;
    if let Some(snapshot) = &ctx.executor_outbox_snapshot {
        log_transport_snapshot("executor_outbox", snapshot);
    } else {
        tracing::info!("executor_outbox absent path={}", outbox_path.display());
    }

    let workflow_type = ctx.args.workflow_type;
    let artifact_names: &[&str] = match workflow_type {
        WorkflowType::Plan => &["PLAN.md"],
        WorkflowType::Investigate => &["INVESTIGATION.md"],
        WorkflowType::Implement => &[],
    };
    for name in artifact_names {
        let exists = artifact_file_present(ctx.repo().join(name)).await?;
        ctx.artifact_map.insert((*name).to_owned(), exists);
        if exists {
            ctx.artifact_paths.push((*name).to_owned());
        }
    }

    // Collect commits made since dispatch using the initial HEAD as the range base.
    let log_out = if let Some(base) = &ctx.initial_git_head {
        run_git(ctx.repo(), &["log", "--oneline", &format!("{base}..HEAD")]).await?
    } else {
        run_git(ctx.repo(), &["log", "--oneline", "-20"]).await?
    };
    for line in log_out.lines().filter(|l| !l.trim().is_empty()) {
        let hash = line.split_whitespace().next().unwrap_or("").to_owned();
        if !hash.is_empty() && !ctx.commit_hashes.contains(&hash) {
            ctx.commit_hashes.push(hash);
        }
    }

    ctx.run_state = RunState::OrchVerify;
    phase_separator_wait().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase: ORCH_VERIFY
// ---------------------------------------------------------------------------

/// Gate before reviewer dispatch.
///
/// Outbox presence (checked in EXECUTOR_MONITOR via `check_success_contract`) is the only
/// routing precondition. Artifact quality — PLAN.md, INVESTIGATION.md, commit presence —
/// is a semantic concern for the reviewer, not an orchestration gate. Removing it here
/// lets the reviewer classify `blocked`, `revise`, or `poisoned_session` even when
/// executor output is incomplete.
///
/// ORCH_VERIFY must NOT grep, regex, parse, summarize, semantically inspect, or route based on
/// executor outbox.txt. A raw diagnostic snapshot may be logged and emitted in the final JSON,
/// but the reviewer remains the only semantic consumer of executor output.
pub(super) async fn do_orch_verify(ctx: &mut OrchestratorCtx) -> OrchestratorResult<()> {
    // A new executor round invalidates the previous semantic reviewer verdict.
    // Keep saved reviewer YAML only across reviewer cleanup retries, not across
    // executor feedback rounds.
    ctx.review_result = None;
    ctx.review_result_yaml_raw = None;
    ctx.reviewer_yaml_rejection = None;
    ctx.reviewer_workspace_rejection = None;
    ctx.run_state = RunState::ReviewerDispatch;
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase: REVIEWER_OUTPUT_COLLECT
// ---------------------------------------------------------------------------

pub(super) async fn do_reviewer_output_collect(
    ctx: &mut OrchestratorCtx,
) -> OrchestratorResult<()> {
    let outbox_path = ctx
        .repo()
        .join(crate::constants::TRANSPORT_DIR)
        .join(crate::constants::OUTBOX_FILE);

    // Check that outbox mtime changed (reviewer actually wrote something).
    let pre_mtime = ctx
        .attempt
        .as_ref()
        .and_then(|a| a.pre_reviewer_outbox_mtime);

    let current_mtime = optional_file_mtime(&outbox_path).await?;

    if pre_mtime == current_mtime && pre_mtime.is_some() {
        retry_reviewer_output_failure(
            ctx,
            "reviewer outbox mtime unchanged after reviewer exit — retrying reviewer",
        );
        return Ok(());
    }

    let raw = if ctx.review_result.is_some() {
        ctx.review_result_yaml_raw.clone().unwrap_or_default()
    } else {
        match tokio::fs::read_to_string(&outbox_path).await {
            Ok(r) => r,
            Err(_) => {
                retry_reviewer_output_failure(ctx, "reviewer outbox not readable");
                return Ok(());
            }
        }
    };
    let reviewer_session_id = ctx
        .reviewer_session_id
        .as_deref()
        .unwrap_or("<unbound>")
        .to_owned();
    ctx.reviewer_outbox_snapshot = read_transport_snapshot(
        &outbox_path,
        AgentRole::Reviewer,
        ctx.args.reviewer_provider,
        &reviewer_session_id,
    )
    .await?;
    if let Some(snapshot) = &ctx.reviewer_outbox_snapshot {
        log_transport_snapshot("reviewer_outbox", snapshot);
    }

    if ctx.review_result.is_none() {
        match parse_reviewer_yaml(&raw) {
            Ok(yaml) => {
                if let Err(e) = enforce_reviewer_notion_policy(ctx.args.notion_policy, &yaml) {
                    let rejection = format!("{e}");
                    ctx.reviewer_yaml_rejection = Some(rejection.clone());
                    tracing::warn!("reviewer YAML rejected: {rejection}");
                    retry_reviewer_output_failure(
                        ctx,
                        format!("reviewer YAML parse failed: {rejection}"),
                    );
                    return Ok(());
                }
                ctx.reviewer_yaml_rejection = None;
                ctx.review_result_yaml_raw = Some(raw);
                ctx.review_result = Some(yaml);
            }
            Err(e) => {
                let rejection = format!("{e}");
                ctx.reviewer_yaml_rejection = Some(rejection.clone());
                tracing::warn!("reviewer YAML rejected: {rejection}");
                retry_reviewer_output_failure(
                    ctx,
                    format!("reviewer YAML parse failed: {rejection}"),
                );
                return Ok(());
            }
        }
    }

    if let Some(reason) = reviewer_workspace_dirty_reason(ctx).await? {
        ctx.reviewer_workspace_rejection = Some(reason.clone());
        retry_reviewer_output_failure(
            ctx,
            format!("reviewer workspace cleanup required: {reason}"),
        );
        return Ok(());
    }
    ctx.reviewer_workspace_rejection = None;

    // If cleanup retry succeeded, use the already-saved reviewer YAML from the first
    // semantic review attempt. Cleanup must not require regenerating the verdict.
    ctx.run_state = RunState::QualityGate;

    phase_separator_wait().await;
    Ok(())
}

fn retry_reviewer_output_failure(ctx: &mut OrchestratorCtx, reason: impl Into<String>) {
    let reason = reason.into();
    ctx.consecutive_failure_count += 1;
    tracing::warn!("reviewer retry reason: {reason}");
    ctx.failures.push(reason);
    ctx.run_state = RunState::RoundRetryDecide;
}

fn enforce_reviewer_notion_policy(
    policy: NotionPolicy,
    yaml: &crate::yaml_check::ReviewerYaml,
) -> OrchestratorResult<()> {
    if policy != NotionPolicy::Required {
        return Ok(());
    }

    let notion_ok = yaml.notion_requirements_satisfied.ok_or_else(|| {
        OrchestratorError::ArtifactContract {
            contract: "notion_policy=required requires explicit notion_requirements_satisfied=true|false in reviewer YAML".to_owned(),
        }
    })?;

    if yaml.decision == ReviewDecision::Accept && !notion_ok {
        return Err(OrchestratorError::ArtifactContract {
            contract: "decision=accept is forbidden when notion_policy=required and notion_requirements_satisfied=false".to_owned(),
        });
    }

    Ok(())
}

/// Return a cleanup reason when the reviewer leaves new dirty worktree entries.
///
/// Uses `pre_reviewer_git_status` as the baseline — captured immediately before reviewer
/// dispatch — not `initial_git_status` from CONTEXT_PREP. The executor may have left
/// uncommitted artifacts (PLAN.md, INVESTIGATION.md) in the worktree; baseline entries are
/// not treated as reviewer leftovers.
///
/// Reviewer commits are intentionally ignored here. If the reviewer committed its changes and
/// no new dirty worktree entries remain, the workspace is clean enough to continue.
async fn reviewer_workspace_dirty_reason(
    ctx: &OrchestratorCtx,
) -> OrchestratorResult<Option<String>> {
    let current_status =
        run_git(ctx.repo(), &["status", "--short", "--untracked-files=all"]).await?;
    let initial = ctx.pre_reviewer_git_status.as_deref().unwrap_or("");
    let outbox_rel = format!(
        "{}/{}",
        crate::constants::TRANSPORT_DIR,
        crate::constants::OUTBOX_FILE
    );
    Ok(reviewer_workspace_dirty_lines(initial, &current_status, &outbox_rel).map(|lines| {
        format!(
            "reviewer left dirty worktree entries after review; clean by committing or reverting before writing final YAML: {}",
            lines.join("; ")
        )
    }))
}

/// Pure inner logic of [`reviewer_workspace_dirty_reason`]; extracted for unit testing.
///
/// Compares `initial` and `current` git-status outputs (each a newline-separated
/// list of `git status --short` lines) and returns any new lines whose
/// path is not `outbox_rel`.
fn reviewer_workspace_dirty_lines(
    initial: &str,
    current: &str,
    outbox_rel: &str,
) -> Option<Vec<String>> {
    let initial_lines: std::collections::HashSet<&str> = initial.lines().collect();
    let dirty_lines: Vec<String> = current
        .lines()
        .filter(|l| !initial_lines.contains(l))
        .filter(|line| {
            // `git status --short` format: "XY path" — 2 status chars + space + path.
            let path_part = if line.len() > 3 {
                line[3..].trim()
            } else {
                line.trim()
            };
            path_part != outbox_rel
        })
        .map(str::to_owned)
        .collect();

    if dirty_lines.is_empty() {
        None
    } else {
        Some(dirty_lines)
    }
}

// ---------------------------------------------------------------------------
// Phase: QUALITY_GATE
// ---------------------------------------------------------------------------

pub(super) fn do_quality_gate(ctx: &mut OrchestratorCtx) -> OrchestratorResult<()> {
    let review = match ctx.review_result.as_ref() {
        Some(r) => r.clone(),
        None => {
            ctx.run_state = RunState::RunFailedProtocol;
            return Ok(());
        }
    };

    tracing::info!(
        "quality gate: decision={:?} score={}",
        review.decision,
        review.quality_score
    );

    // Reset consecutive failure count on any complete successful semantic round.
    // reviewer `decision: revise` counts as success here — it is a productive
    // semantic exchange, not an orchestration failure.  Only infra failures
    // (crashes, silent exits, hangs) increment the counter.
    ctx.consecutive_failure_count = 0;

    match review.decision {
        ReviewDecision::Accept => {
            ctx.run_state = RunState::RunDone;
            ctx.next_action_required = None;
        }
        ReviewDecision::Revise => {
            ctx.run_state = RunState::RoundFeedbackPrep;
        }
        ReviewDecision::Blocked => {
            ctx.run_state = RunState::RunFailedExternalBlocker;
            ctx.next_action_required = review.blocking_reason.clone();
        }
        ReviewDecision::IrreconcilableDisagreement => {
            ctx.run_state = RunState::RunFailedIrreconcilableDisagreement;
            ctx.next_action_required = review.irreconcilable_reason.clone();
        }
        ReviewDecision::PoisonedSession => {
            ctx.run_state = RunState::RunFailedPoisonedSession;
            ctx.next_action_required = review.poisoned_session_reason.clone();
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase: ROUND_FEEDBACK_PREP
// ---------------------------------------------------------------------------

pub(super) async fn do_round_feedback_prep(ctx: &mut OrchestratorCtx) -> OrchestratorResult<()> {
    // Transport files will be reset in next executor dispatch; no work here.
    ctx.run_state = RunState::ExecutorDispatch;
    phase_separator_wait().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

async fn optional_file_mtime(
    path: &std::path::Path,
) -> OrchestratorResult<Option<std::time::SystemTime>> {
    match tokio::fs::metadata(path).await {
        Ok(meta) => Ok(Some(meta.modified()?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

async fn artifact_file_present(path: std::path::PathBuf) -> OrchestratorResult<bool> {
    match tokio::fs::metadata(path).await {
        Ok(meta) => Ok(meta.len() > 0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

async fn read_transport_snapshot(
    path: &std::path::Path,
    role: AgentRole,
    provider: ProviderKind,
    session_id: &str,
) -> OrchestratorResult<Option<TransportBodyReport>> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let meta = tokio::fs::metadata(path).await?;
    let mtime = DateTime::<Utc>::from(meta.modified()?).to_rfc3339();
    let utf8_lossy = std::str::from_utf8(&bytes).is_err();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let lines = body.lines().count();
    Ok(Some(TransportBodyReport {
        role,
        provider,
        session_id: session_id.to_owned(),
        path: path.display().to_string(),
        sha256: crate::transport::sha256_hex(&bytes),
        bytes: bytes.len() as u64,
        lines,
        utf8_lossy,
        mtime: Some(mtime),
        body,
    }))
}

fn log_transport_snapshot(label: &str, snapshot: &TransportBodyReport) {
    tracing::info!(
        "{label} snapshot role={:?} provider={} session={} path={} bytes={} lines={} sha256={} mtime={} utf8_lossy={}",
        snapshot.role,
        snapshot.provider,
        snapshot.session_id,
        snapshot.path,
        snapshot.bytes,
        snapshot.lines,
        snapshot.sha256,
        snapshot.mtime.as_deref().unwrap_or("<none>"),
        snapshot.utf8_lossy
    );
    tracing::info!("{label} body begin\n{}\n{label} body end", snapshot.body);
}

async fn phase_separator_wait() {
    tokio::time::sleep(std::time::Duration::from_secs(PHASE_SEPARATOR_WAIT_SEC)).await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn outbox_rel() -> String {
        format!(
            "{}/{}",
            crate::constants::TRANSPORT_DIR,
            crate::constants::OUTBOX_FILE
        )
    }

    #[tokio::test]
    async fn read_transport_snapshot_captures_body_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.txt");
        tokio::fs::write(&path, "line one\nline two\n")
            .await
            .unwrap();

        let snapshot = read_transport_snapshot(
            &path,
            AgentRole::Executor,
            ProviderKind::Opencode,
            "session-123",
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(snapshot.role, AgentRole::Executor);
        assert_eq!(snapshot.provider, ProviderKind::Opencode);
        assert_eq!(snapshot.session_id, "session-123");
        assert_eq!(snapshot.bytes, 18);
        assert_eq!(snapshot.lines, 2);
        assert!(!snapshot.utf8_lossy);
        assert_eq!(snapshot.body, "line one\nline two\n");
        assert_eq!(
            snapshot.sha256,
            crate::transport::sha256_hex(b"line one\nline two\n")
        );
        assert!(snapshot.mtime.is_some());
    }

    #[tokio::test]
    async fn read_transport_snapshot_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = read_transport_snapshot(
            &dir.path().join("missing.txt"),
            AgentRole::Reviewer,
            ProviderKind::Codex,
            "session-456",
        )
        .await
        .unwrap();

        assert!(snapshot.is_none());
    }

    #[test]
    fn reviewer_git_state_no_new_lines_is_ok() {
        let status = " M src/foo.rs\n?? bar.txt";
        assert!(reviewer_workspace_dirty_lines(status, status, &outbox_rel()).is_none());
    }

    #[test]
    fn reviewer_git_state_empty_both_is_ok() {
        assert!(reviewer_workspace_dirty_lines("", "", &outbox_rel()).is_none());
    }

    #[test]
    fn reviewer_git_state_outbox_only_is_ok() {
        let outbox = outbox_rel();
        // "??" = untracked in git status --short; 3rd char is space before path
        let current = format!("?? {outbox}");
        assert!(reviewer_workspace_dirty_lines("", &current, &outbox).is_none());
    }

    #[test]
    fn reviewer_git_state_extra_file_is_violation() {
        let result = reviewer_workspace_dirty_lines("", " M src/lib.rs", &outbox_rel()).unwrap();
        assert_eq!(result, vec![" M src/lib.rs"]);
    }

    #[test]
    fn reviewer_git_state_outbox_and_extra_file_is_violation() {
        let outbox = outbox_rel();
        let current = format!("?? {outbox}\n M src/lib.rs");
        assert_eq!(
            reviewer_workspace_dirty_lines("", &current, &outbox).unwrap(),
            vec![" M src/lib.rs"]
        );
    }

    #[test]
    fn reviewer_git_state_executor_files_in_initial_are_ignored() {
        // Executor left PLAN.md; it appears in both initial and current — not a violation.
        let initial = " M PLAN.md";
        let current = " M PLAN.md";
        assert!(reviewer_workspace_dirty_lines(initial, current, &outbox_rel()).is_none());
    }

    #[test]
    fn reviewer_git_state_new_file_beyond_initial_is_violation() {
        let initial = " M PLAN.md";
        let current = " M PLAN.md\n M README.md";
        assert_eq!(
            reviewer_workspace_dirty_lines(initial, current, &outbox_rel()).unwrap(),
            vec![" M README.md"]
        );
    }

    fn make_validate_ctx(workspace_root: std::path::PathBuf) -> OrchestratorCtx {
        OrchestratorCtx::new(crate::orchestrator::CliArgs {
            workflow_type: WorkflowType::Implement,
            notion_policy: crate::state::NotionPolicy::Optional,
            remote_network_policy: crate::state::RemoteNetworkPolicy::Forbidden,
            workspace_root,
            executor_thread_name: "exec".to_owned(),
            reviewer_thread_name: "review".to_owned(),
            prompt: "do work".to_owned(),
            executor_provider: ProviderKind::Claude,
            reviewer_provider: ProviderKind::Opencode,
            executor_model: None,
            reviewer_model: None,
        })
    }

    async fn git(repo: &Path, args: &[&str]) -> String {
        let output = tokio::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    async fn init_git_repo_with_commit() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init"]).await;
        git(
            dir.path(),
            &["config", "user.email", "orchestrator-test@example.com"],
        )
        .await;
        git(dir.path(), &["config", "user.name", "Orchestrator Test"]).await;
        tokio::fs::write(dir.path().join("README.md"), "initial\n")
            .await
            .unwrap();
        git(dir.path(), &["add", "README.md"]).await;
        git(dir.path(), &["commit", "-m", "initial"]).await;
        dir
    }

    fn invalid_input_reason(err: OrchestratorError) -> String {
        match err {
            OrchestratorError::InvalidInput { reason, .. } => reason,
            err => panic!("expected InvalidInput, got {err:?}"),
        }
    }

    #[test]
    fn reset_attempt_git_telemetry_clears_stale_hot_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_validate_ctx(dir.path().to_path_buf());
        ctx.dirty_file_touch_counts
            .insert("README.md".to_owned(), 4);
        ctx.committed_file_touch_counts
            .insert("CONTRIBUTING.md".to_owned(), 1);
        ctx.last_dirty_files_count = 3;
        ctx.committed_files_seen_count = 31;

        reset_attempt_git_telemetry(&mut ctx, 0);

        assert!(ctx.dirty_file_touch_counts.is_empty());
        assert!(ctx.committed_file_touch_counts.is_empty());
        assert_eq!(ctx.last_dirty_files_count, 0);
        assert_eq!(ctx.committed_files_seen_count, 0);
    }

    #[test]
    fn git_status_line_set_uses_dispatch_status_as_delta_baseline() {
        let lines = git_status_line_set(" M README.md\n?? new.txt\n\n");

        assert_eq!(
            lines,
            BTreeSet::from([" M README.md".to_owned(), "?? new.txt".to_owned()])
        );
    }

    #[test]
    fn validate_inputs_resolves_relative_path() {
        let cwd = std::env::current_dir().unwrap();
        let target_dir = cwd.join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let dir = tempfile::tempdir_in(&target_dir).unwrap();
        let relative = dir.path().strip_prefix(&cwd).unwrap().to_path_buf();
        let expected = dir.path().canonicalize().unwrap();

        let mut ctx = make_validate_ctx(relative);
        validate_inputs(&mut ctx).unwrap();
        assert_eq!(ctx.args.workspace_root, expected);
    }

    #[tokio::test]
    async fn context_prep_rejects_unborn_repo() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init"]).await;

        let mut ctx = make_validate_ctx(dir.path().to_path_buf());
        let reason = invalid_input_reason(do_context_prep(&mut ctx).await.unwrap_err());

        assert!(reason.contains("valid HEAD commit"), "{reason}");
        assert!(
            reason.contains("unborn branches are unsupported"),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn context_prep_rejects_detached_head() {
        let dir = init_git_repo_with_commit().await;
        git(dir.path(), &["checkout", "--detach", "HEAD"]).await;

        let mut ctx = make_validate_ctx(dir.path().to_path_buf());
        let reason = invalid_input_reason(do_context_prep(&mut ctx).await.unwrap_err());

        assert!(reason.contains("detached HEAD"), "{reason}");
    }

    #[tokio::test]
    async fn context_prep_rejects_non_top_level_workspace_root() {
        let dir = init_git_repo_with_commit().await;
        let subdir = dir.path().join("nested");
        tokio::fs::create_dir(&subdir).await.unwrap();

        let mut ctx = make_validate_ctx(subdir);
        let reason = invalid_input_reason(do_context_prep(&mut ctx).await.unwrap_err());

        assert!(reason.contains("git repository top-level"), "{reason}");
    }

    #[tokio::test]
    async fn context_prep_rejects_git_operation_in_progress() {
        let dir = init_git_repo_with_commit().await;
        let merge_head = resolve_git_path(dir.path(), "MERGE_HEAD").await.unwrap();
        tokio::fs::write(&merge_head, "deadbeef\n").await.unwrap();

        let mut ctx = make_validate_ctx(dir.path().to_path_buf());
        let reason = invalid_input_reason(do_context_prep(&mut ctx).await.unwrap_err());

        assert!(
            reason.contains("git operation in progress: merge"),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn context_prep_rejects_git_lock_file() {
        let dir = init_git_repo_with_commit().await;
        let index_lock = resolve_git_path(dir.path(), "index.lock").await.unwrap();
        tokio::fs::write(&index_lock, "").await.unwrap();

        let mut ctx = make_validate_ctx(dir.path().to_path_buf());
        let reason = invalid_input_reason(do_context_prep(&mut ctx).await.unwrap_err());

        assert!(reason.contains("git lock file exists"), "{reason}");
    }

    #[tokio::test]
    async fn validate_no_unmerged_index_rejects_conflicted_entries() {
        let dir = init_git_repo_with_commit().await;
        let base = git(dir.path(), &["hash-object", "-w", "--stdin"])
            .await
            .trim()
            .to_owned();
        let ours = git(dir.path(), &["hash-object", "-w", "--stdin"])
            .await
            .trim()
            .to_owned();
        let theirs = git(dir.path(), &["hash-object", "-w", "--stdin"])
            .await
            .trim()
            .to_owned();
        let index_info = format!(
            "100644 {base} 1\tconflict.txt\n100644 {ours} 2\tconflict.txt\n100644 {theirs} 3\tconflict.txt\n"
        );
        let mut child = tokio::process::Command::new("git")
            .args(["update-index", "--index-info"])
            .current_dir(dir.path())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use tokio::io::AsyncWriteExt;

            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(index_info.as_bytes()).await.unwrap();
        }
        let output = child.wait_with_output().await.unwrap();
        assert!(
            output.status.success(),
            "git update-index failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let reason =
            invalid_input_reason(validate_no_unmerged_index(dir.path()).await.unwrap_err());

        assert!(reason.contains("unmerged/conflicted entries"), "{reason}");
    }

    #[test]
    fn validate_inputs_nonexistent_path_is_invalid_input() {
        let mut ctx = make_validate_ctx(std::path::PathBuf::from("/nonexistent_path_xyz_abc_123"));
        let err = validate_inputs(&mut ctx).unwrap_err();
        assert!(matches!(err, OrchestratorError::InvalidInput { .. }));
    }

    #[test]
    fn validate_inputs_empty_prompt_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = OrchestratorCtx::new(crate::orchestrator::CliArgs {
            workflow_type: WorkflowType::Implement,
            notion_policy: crate::state::NotionPolicy::Optional,
            remote_network_policy: crate::state::RemoteNetworkPolicy::Forbidden,
            workspace_root: dir.path().to_path_buf(),
            executor_thread_name: "exec".to_owned(),
            reviewer_thread_name: "review".to_owned(),
            prompt: "   ".to_owned(),
            executor_provider: ProviderKind::Claude,
            reviewer_provider: ProviderKind::Opencode,
            executor_model: None,
            reviewer_model: None,
        });
        let err = validate_inputs(&mut ctx).unwrap_err();
        assert!(
            matches!(err, OrchestratorError::InvalidInput { ref field, .. } if field == "--prompt")
        );
    }

    #[test]
    fn validate_inputs_rejects_model_for_non_opencode_provider() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = OrchestratorCtx::new(crate::orchestrator::CliArgs {
            workflow_type: WorkflowType::Implement,
            notion_policy: crate::state::NotionPolicy::Optional,
            remote_network_policy: crate::state::RemoteNetworkPolicy::Forbidden,
            workspace_root: dir.path().to_path_buf(),
            executor_thread_name: "exec".to_owned(),
            reviewer_thread_name: "review".to_owned(),
            prompt: "do work".to_owned(),
            executor_provider: ProviderKind::Claude,
            reviewer_provider: ProviderKind::Opencode,
            executor_model: Some("deepseek/deepseek-v4-flash".to_owned()),
            reviewer_model: None,
        });
        let err = validate_inputs(&mut ctx).unwrap_err();
        assert!(
            matches!(err, OrchestratorError::InvalidInput { ref field, .. } if field == "--executor-model")
        );
    }

    #[test]
    fn validate_inputs_rejects_invalid_model_format() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = OrchestratorCtx::new(crate::orchestrator::CliArgs {
            workflow_type: WorkflowType::Implement,
            notion_policy: crate::state::NotionPolicy::Optional,
            remote_network_policy: crate::state::RemoteNetworkPolicy::Forbidden,
            workspace_root: dir.path().to_path_buf(),
            executor_thread_name: "exec".to_owned(),
            reviewer_thread_name: "review".to_owned(),
            prompt: "do work".to_owned(),
            executor_provider: ProviderKind::Opencode,
            reviewer_provider: ProviderKind::Claude,
            executor_model: Some("deepseek".to_owned()),
            reviewer_model: None,
        });
        let err = validate_inputs(&mut ctx).unwrap_err();
        assert!(
            matches!(err, OrchestratorError::InvalidInput { ref field, .. } if field == "--executor-model")
        );
    }

    fn test_ctx(repo: &std::path::Path) -> OrchestratorCtx {
        let mut ctx = OrchestratorCtx::new(crate::orchestrator::CliArgs {
            workflow_type: WorkflowType::Implement,
            notion_policy: crate::state::NotionPolicy::Optional,
            remote_network_policy: crate::state::RemoteNetworkPolicy::Forbidden,
            workspace_root: repo.to_path_buf(),
            executor_thread_name: "executor".to_owned(),
            reviewer_thread_name: "reviewer".to_owned(),
            prompt: "do work".to_owned(),
            executor_provider: ProviderKind::Claude,
            reviewer_provider: ProviderKind::Opencode,
            executor_model: None,
            reviewer_model: None,
        });
        ctx.branch = "main".to_owned();
        ctx.current_role = Some(AgentRole::Reviewer);
        ctx.attempt = Some(AttemptStateData {
            state: AttemptState::Success,
            role: AgentRole::Reviewer,
            dispatch_ts: Utc::now(),
            pid: None,
            exit_code: Some(0),
            request_fingerprint: "sha".to_owned(),
            last_work_signal_ts: Some(Utc::now()),
            grace_until_commit: None,
            grace_until_outbox: None,
            pre_reviewer_outbox_mtime: None,
            next_probe_at: Utc::now(),
            dispatch_git_status_hash: String::new(),
            prev_probe_git_status_hash: String::new(),
            prev_probe_git_status_lines: std::collections::BTreeSet::new(),
            dispatch_git_head_hash: String::new(),
            prev_probe_git_head_hash: String::new(),
            prev_probe_outbox_meta: None,
            prev_probe_log_mtime: None,
            provider_log_ever_seen: false,
            last_heartbeat_ts: None,
        });
        ctx
    }

    #[tokio::test]
    async fn reviewer_output_collect_malformed_yaml_routes_retry_decide() {
        let dir = tempfile::tempdir().unwrap();
        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        let transport = dir.path().join(crate::constants::TRANSPORT_DIR);
        tokio::fs::create_dir_all(&transport).await.unwrap();
        tokio::fs::write(transport.join(crate::constants::OUTBOX_FILE), "not: [valid")
            .await
            .unwrap();

        let mut ctx = test_ctx(dir.path());

        do_reviewer_output_collect(&mut ctx).await.unwrap();

        assert_eq!(ctx.run_state, RunState::RoundRetryDecide);
        assert_eq!(ctx.consecutive_failure_count, 1);
        assert!(ctx
            .failures
            .last()
            .unwrap()
            .contains("reviewer YAML parse failed"));
    }

    #[tokio::test]
    async fn reviewer_output_collect_unchanged_mtime_routes_retry_decide() {
        let dir = tempfile::tempdir().unwrap();
        let transport = dir.path().join(crate::constants::TRANSPORT_DIR);
        tokio::fs::create_dir_all(&transport).await.unwrap();
        let outbox = transport.join(crate::constants::OUTBOX_FILE);
        tokio::fs::write(&outbox, "executor output").await.unwrap();
        let mtime = tokio::fs::metadata(&outbox)
            .await
            .unwrap()
            .modified()
            .unwrap();

        let mut ctx = test_ctx(dir.path());
        ctx.attempt.as_mut().unwrap().pre_reviewer_outbox_mtime = Some(mtime);

        do_reviewer_output_collect(&mut ctx).await.unwrap();

        assert_eq!(ctx.run_state, RunState::RoundRetryDecide);
        assert_eq!(ctx.consecutive_failure_count, 1);
        assert!(ctx
            .failures
            .last()
            .unwrap()
            .contains("reviewer outbox mtime unchanged"));
    }

    #[tokio::test]
    async fn reviewer_output_collect_dirty_workspace_routes_cleanup_retry() {
        let dir = tempfile::tempdir().unwrap();
        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        let transport = dir.path().join(crate::constants::TRANSPORT_DIR);
        tokio::fs::create_dir_all(&transport).await.unwrap();
        let yaml = r#"
quality_score: 8
decision: accept
rationale: ok
contract_satisfied: true
hard_blockers_present: false
notion_requirements_satisfied: true
feedback_for_executor: []
checks_performed: []
findings: []
verification_commands: []
blocking_reason: null
irreconcilable_reason: null
poisoned_session_reason: null
"#;
        tokio::fs::write(transport.join(crate::constants::OUTBOX_FILE), yaml)
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("reviewer-leftover.txt"), "dirty")
            .await
            .unwrap();

        let mut ctx = test_ctx(dir.path());

        do_reviewer_output_collect(&mut ctx).await.unwrap();

        assert_eq!(ctx.run_state, RunState::RoundRetryDecide);
        assert_eq!(ctx.consecutive_failure_count, 1);
        assert!(ctx.review_result.is_some());
        assert!(ctx.review_result_yaml_raw.is_some());
        assert!(ctx.reviewer_workspace_rejection.is_some());
        assert!(ctx
            .failures
            .last()
            .unwrap()
            .contains("reviewer workspace cleanup required"));
    }

    #[tokio::test]
    async fn reviewer_output_collect_rejects_accept_when_required_notion_not_satisfied() {
        let dir = tempfile::tempdir().unwrap();
        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        let transport = dir.path().join(crate::constants::TRANSPORT_DIR);
        tokio::fs::create_dir_all(&transport).await.unwrap();
        let yaml = r#"
quality_score: 9.0
decision: accept
rationale: ok
contract_satisfied: true
hard_blockers_present: false
notion_requirements_satisfied: false
feedback_for_executor: []
checks_performed: []
findings: []
verification_commands: []
blocking_reason: null
irreconcilable_reason: null
poisoned_session_reason: null
"#;
        tokio::fs::write(transport.join(crate::constants::OUTBOX_FILE), yaml)
            .await
            .unwrap();

        let mut ctx = test_ctx(dir.path());
        ctx.args.notion_policy = crate::state::NotionPolicy::Required;

        do_reviewer_output_collect(&mut ctx).await.unwrap();

        assert_eq!(ctx.run_state, RunState::RoundRetryDecide);
        assert_eq!(ctx.consecutive_failure_count, 1);
        assert!(ctx
            .failures
            .last()
            .unwrap()
            .contains("decision=accept is forbidden"));
    }

    #[tokio::test]
    async fn orch_verify_clears_previous_reviewer_verdict_before_next_review() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(dir.path());
        ctx.review_result_yaml_raw = Some("quality_score: 5\ndecision: revise\n".to_owned());
        ctx.review_result = Some(crate::yaml_check::ReviewerYaml {
            quality_score: 5.0,
            decision: ReviewDecision::Revise,
            rationale: "old revise".to_owned(),
            contract_satisfied: false,
            hard_blockers_present: true,
            notion_requirements_satisfied: Some(true),
            feedback_for_executor: vec!["fix it".to_owned()],
            checks_performed: None,
            findings: None,
            verification_commands: None,
            blocking_reason: None,
            irreconcilable_reason: None,
            poisoned_session_reason: None,
        });
        ctx.reviewer_yaml_rejection = Some("old yaml rejection".to_owned());
        ctx.reviewer_workspace_rejection = Some("old workspace rejection".to_owned());

        do_orch_verify(&mut ctx).await.unwrap();

        assert_eq!(ctx.run_state, RunState::ReviewerDispatch);
        assert!(ctx.review_result.is_none());
        assert!(ctx.review_result_yaml_raw.is_none());
        assert!(ctx.reviewer_yaml_rejection.is_none());
        assert!(ctx.reviewer_workspace_rejection.is_none());
    }
}
