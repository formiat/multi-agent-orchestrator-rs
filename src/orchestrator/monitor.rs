use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use crate::constants::{
    CANCEL_CHILD_WAIT_SEC, COMMIT_GRACE_SEC, HANG_CONFIRM_SEC, HANG_MAX_WITH_WORK_SIGNALS_SEC,
    HEARTBEAT_INTERVAL_SEC, OUTBOX_FINALIZING_KILL_SEC, OUTBOX_GRACE_SEC, PROBE_INTERVAL_SEC,
};
use crate::errors::OrchestratorResult;
use crate::providers::read_diagnostics;
use crate::signals::{
    classify_provider_error_with_match, collect_probe_signals, ProviderErrorMatch,
};
use crate::state::{
    AgentRole, AttemptState, ProbeSignals, ProbeSnapshot, ProviderErrorClass, ProviderKind,
    RunState, WorkflowType,
};
use crate::workflow::soft_success_allowed;
use crate::yaml_check::parse_reviewer_yaml;

use super::OrchestratorCtx;

fn is_cancelled() -> bool {
    super::CANCEL_FLAG.load(Ordering::SeqCst)
}

fn parse_git_status_path(line: &str) -> Option<String> {
    let line = line.trim_end();
    if line.trim().is_empty() {
        return None;
    }
    let rest = if line.as_bytes().get(2) == Some(&b' ') {
        &line[3..]
    } else if let Some((status, path)) = line.split_once(' ') {
        if status.len() <= 2 && status.bytes().all(|b| b.is_ascii_graphic()) {
            path
        } else {
            line
        }
    } else {
        line
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    if let Some((_, rhs)) = rest.rsplit_once(" -> ") {
        return Some(rhs.trim().to_owned());
    }
    Some(rest.to_owned())
}

fn git_status_lines(raw: &str) -> BTreeSet<String> {
    raw.lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_git_name_status_path(line: &str) -> Option<String> {
    let line = line.trim_end();
    if line.trim().is_empty() {
        return None;
    }

    let mut tab_parts = line.split('\t');
    let first = tab_parts.next()?.trim();
    let rest = tab_parts.collect::<Vec<_>>();
    if !rest.is_empty() {
        return rest.last().map(|part| part.trim().to_owned());
    }

    let mut parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() >= 2 {
        return Some(parts.pop()?.trim().to_owned());
    }
    if first.is_empty() {
        return None;
    }
    Some(first.to_owned())
}

fn format_git_name_status_line(line: &str) -> Option<String> {
    let line = line.trim_end();
    if line.trim().is_empty() {
        return None;
    }

    let mut tab_parts = line.split('\t');
    let status = tab_parts.next()?.trim();
    let paths = tab_parts
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    if !paths.is_empty() {
        return Some(format!("{status} {}", paths.join(" -> ")));
    }

    if let Some((status, path)) = line.split_once(' ') {
        let status = status.trim();
        let path = path.trim();
        if !status.is_empty() && !path.is_empty() {
            return Some(format!("{status} {}", path.replace('\t', " ")));
        }
    }

    Some(line.replace('\t', " "))
}

fn git_name_status_lines(raw: &str) -> BTreeSet<String> {
    raw.lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn top_hot_files(
    counts: &std::collections::HashMap<String, u32>,
    limit: usize,
) -> Vec<(String, u32)> {
    let mut items = counts
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect::<Vec<_>>();
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    items.truncate(limit);
    items
}

fn truncate_at_char_boundary(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }

    let mut new_len = max_bytes;
    while !value.is_char_boundary(new_len) {
        new_len -= 1;
    }
    value.truncate(new_len);
}

fn normalize_provider_action(raw: &str) -> Option<String> {
    let action = raw.trim();
    if action.is_empty() {
        return None;
    }
    let action = action.replace('\n', " ");
    let action = action.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut action = action.trim().to_owned();
    if action.len() > 220 {
        truncate_at_char_boundary(&mut action, 220);
        action.push_str("...");
    }
    Some(action)
}

fn json_path_str<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a str> {
    v.pointer(path).and_then(serde_json::Value::as_str)
}

fn extract_action_from_json_line(line: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;

    // Common structured event paths for command/tool execution.
    for path in [
        "/command",
        "/cmd",
        "/input/command",
        "/input/cmd",
        "/tool_input/command",
        "/payload/command",
    ] {
        if let Some(cmd) = json_path_str(&value, path) {
            if let Some(normalized) = normalize_provider_action(cmd) {
                return Some(normalized);
            }
        }
    }

    // Codex/OpenCode style fallback: explicit command_execution event with nested command field.
    let event_type = json_path_str(&value, "/type")
        .or_else(|| json_path_str(&value, "/event/type"))
        .unwrap_or("");
    if event_type.contains("command") || event_type.contains("tool") || event_type.contains("exec")
    {
        for path in [
            "/event/command",
            "/event/input/command",
            "/event/tool_input/command",
        ] {
            if let Some(cmd) = json_path_str(&value, path) {
                if let Some(normalized) = normalize_provider_action(cmd) {
                    return Some(normalized);
                }
            }
        }
    }

    None
}

fn extract_action_from_plain_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    // Best-effort fallback for less-structured providers/logs.
    let prefixes = [
        "cargo ", "make ", "notion ", "git ", "rg ", "timeout ", "pytest ", "python ", "bash ",
        "sh ",
    ];
    if prefixes.iter().any(|p| line.starts_with(p)) {
        return normalize_provider_action(line);
    }

    None
}

fn extract_provider_action_tail(stdout: &str, stderr: &str) -> Option<String> {
    let combined = format!("{stdout}\n{stderr}");
    // Scan tail-first to keep only the latest actionable command.
    for line in combined.lines().rev().take(300) {
        if let Some(action) = extract_action_from_json_line(line) {
            return Some(action);
        }
        if let Some(action) = extract_action_from_plain_line(line) {
            return Some(action);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Phase: EXECUTOR_MONITOR / REVIEWER_MONITOR
// ---------------------------------------------------------------------------

pub(super) async fn do_monitor(
    ctx: &mut OrchestratorCtx,
    role: AgentRole,
) -> OrchestratorResult<()> {
    let workflow_type = ctx.args.workflow_type;
    let artifact_names: Vec<&str> = match workflow_type {
        WorkflowType::Plan => vec!["PLAN.md"],
        WorkflowType::Investigate => vec!["INVESTIGATION.md"],
        WorkflowType::Implement => vec![],
    };

    loop {
        if is_cancelled() {
            ctx.run_state = RunState::RunAborted;
            terminate_active_child(ctx).await;
            return Ok(());
        }

        let now = Utc::now();

        // Wait until next probe time.
        if let Some(attempt) = &ctx.attempt {
            let wait = attempt.next_probe_at - now;
            if wait > chrono::Duration::zero() {
                tokio::time::sleep(wait.to_std().unwrap_or_default()).await;
            }
        }

        if is_cancelled() {
            ctx.run_state = RunState::RunAborted;
            terminate_active_child(ctx).await;
            return Ok(());
        }

        let now = Utc::now();

        // Check if process has exited.
        if let Some(proc) = ctx.active_process.as_mut() {
            if let Ok(Some(status)) = proc.child.try_wait() {
                let code = status.code();
                if let Some(attempt) = ctx.attempt.as_mut() {
                    attempt.exit_code = code;
                    attempt.pid = None;
                }
            }
        }

        let pid = ctx.attempt.as_ref().and_then(|a| a.pid);
        let provider = match role {
            AgentRole::Executor => ctx.args.executor_provider,
            AgentRole::Reviewer => ctx.args.reviewer_provider,
        };
        let session_id =
            ctx.session_id(role)
                .ok_or_else(|| crate::errors::OrchestratorError::InvalidInput {
                    field: "session_id".to_owned(),
                    reason: format!("{role:?} session not bound"),
                })?;
        let signals =
            collect_probe_signals(ctx.repo(), pid, &artifact_names, provider, session_id).await?;

        ctx.artifact_map = signals.artifact_map.clone();
        let git_lines = git_status_lines(&signals.git_status_short);

        let attempt = ctx.attempt.as_ref().unwrap();
        let git_changed = signals.git_status_hash != attempt.prev_probe_git_status_hash;
        // A new commit cleans up the worktree so git_status_hash can return to the dispatch
        // baseline; detect committed changes separately via HEAD movement.
        let git_committed = signals.git_head_hash != attempt.prev_probe_git_head_hash;
        let prev_probe_git_head_hash = attempt.prev_probe_git_head_hash.clone();
        let outbox_changed = signals.outbox_meta != attempt.prev_probe_outbox_meta;
        let log_changed = match (signals.provider_log_mtime, attempt.prev_probe_log_mtime) {
            (Some(curr), Some(prev)) => curr != prev,
            (Some(_), None) => true,
            _ => false,
        };

        // Determine if any work signal changed since the previous probe cycle.
        // `process_alive` is deliberately excluded: it is a liveness signal, not an activity
        // signal. Including it would reset `last_work_signal_ts` every cycle and prevent
        // hang detection from ever triggering on live-but-idle processes.
        //
        // `has_child_processes` is included: live descendants (cargo, rustc, clippy-driver,
        // make, pytest) are active work signals that suppress hang detection during build/test
        // phases. The `hang_max_with_work_signals_exceeded` ceiling still fires after 30 min
        // even when children are alive, guarding against perpetually-hung child processes.
        let work_signals_changed = git_changed
            || git_committed
            || outbox_changed
            || log_changed
            || signals.has_child_processes;

        let (newly_changed, resolved, touched_paths) = {
            let attempt = ctx.attempt.as_ref().unwrap();
            let newly_changed = git_lines
                .difference(&attempt.prev_probe_git_status_lines)
                .cloned()
                .collect::<Vec<_>>();
            let resolved = attempt
                .prev_probe_git_status_lines
                .difference(&git_lines)
                .cloned()
                .collect::<Vec<_>>();
            let touched_paths = newly_changed
                .iter()
                .chain(resolved.iter())
                .filter_map(|line| parse_git_status_path(line))
                .collect::<Vec<_>>();
            (newly_changed, resolved, touched_paths)
        };

        if !newly_changed.is_empty() || !resolved.is_empty() {
            for path in &touched_paths {
                let c = ctx.dirty_file_touch_counts.entry(path.clone()).or_insert(0);
                *c += 1;
            }
            let hot = top_hot_files(&ctx.dirty_file_touch_counts, 3);
            tracing::info!(
                "git-delta: role={role:?} newly_changed={:?} resolved={:?} dirty_hot_files={:?}",
                newly_changed,
                resolved,
                hot
            );
        }

        if git_committed {
            let commit_range = format!("{}..{}", prev_probe_git_head_hash, signals.git_head_hash);
            let raw =
                crate::sessions::run_git(ctx.repo(), &["diff", "--name-status", &commit_range])
                    .await?;
            let committed_lines = git_name_status_lines(&raw);
            let committed_display_lines = committed_lines
                .iter()
                .filter_map(|line| format_git_name_status_line(line))
                .collect::<Vec<_>>();
            let committed_paths = committed_lines
                .iter()
                .filter_map(|line| parse_git_name_status_path(line))
                .collect::<Vec<_>>();
            for path in &committed_paths {
                let c = ctx
                    .committed_file_touch_counts
                    .entry(path.clone())
                    .or_insert(0);
                *c += 1;
            }
            ctx.committed_files_seen_count = ctx.committed_file_touch_counts.len();
            let committed_hot = top_hot_files(&ctx.committed_file_touch_counts, 3);
            tracing::info!(
                "git-commit-delta: role={role:?} old_head={} new_head={} committed={:?} committed_hot_files={:?}",
                prev_probe_git_head_hash,
                signals.git_head_hash,
                committed_display_lines,
                committed_hot
            );
        }

        // Warn when the provider log was previously accessible and has now disappeared.
        // "Never seen" at startup is normal; disappearing after being seen is an anomaly.
        if let Some(attempt) = ctx.attempt.as_ref() {
            if attempt.provider_log_ever_seen && signals.provider_log_mtime.is_none() {
                tracing::warn!(
                    "provider session log became absent for {role:?} — \
                     treating as stale for hang and exit classification"
                );
            }
        }

        // Persist signal baselines for the next probe.
        if let Some(attempt) = ctx.attempt.as_mut() {
            attempt.prev_probe_git_status_hash = signals.git_status_hash.clone();
            attempt.prev_probe_git_status_lines = git_lines.clone();
            attempt.prev_probe_git_head_hash = signals.git_head_hash.clone();
            attempt.prev_probe_outbox_meta = signals.outbox_meta.clone();
            attempt.prev_probe_log_mtime = signals.provider_log_mtime;
            if signals.provider_log_mtime.is_some() {
                attempt.provider_log_ever_seen = true;
            }
        }
        ctx.last_dirty_files_count = git_lines.len();

        let dispatch_ts = ctx.attempt.as_ref().map(|a| a.dispatch_ts).unwrap_or(now);
        let hang_max_exceeded =
            (now - dispatch_ts).num_seconds().max(0) as u64 >= HANG_MAX_WITH_WORK_SIGNALS_SEC;

        let process_exited = ctx
            .attempt
            .as_ref()
            .map(|a| a.pid.is_none())
            .unwrap_or(false);

        // A log that was seen and then disappeared is anomalous: treat it as stale so that
        // `provider_activity_after_exit_is_fresh` is false and FailedSilent fires for dead
        // processes. A log that was never seen while the process is still running is normal
        // (provider hasn't written anything yet). A log that was never seen after the process
        // has already exited means there is no provider activity to wait for — also stale.
        let log_disappeared = ctx
            .attempt
            .as_ref()
            .map(|a| a.provider_log_ever_seen && signals.provider_log_mtime.is_none())
            .unwrap_or(false);

        let log_never_seen = ctx
            .attempt
            .as_ref()
            .map(|a| !a.provider_log_ever_seen)
            .unwrap_or(false);

        let provider_stale = log_disappeared
            || (process_exited && log_never_seen)
            || signals
                .provider_log_mtime
                .and_then(|mtime| mtime.elapsed().ok())
                .map(|d| d.as_secs() >= HANG_CONFIRM_SEC)
                .unwrap_or(false);

        if role == AgentRole::Executor {
            update_grace_periods(ctx, &signals, now);
        }

        let grace_deadline = ctx
            .attempt
            .as_ref()
            .and_then(|a| a.grace_until_outbox.or(a.grace_until_commit));

        let last_work_signal_ts = ctx.attempt.as_ref().and_then(|a| a.last_work_signal_ts);
        let diagnostics = read_provider_diagnostics(ctx).await?;
        let provider_error = detect_provider_error(ctx, &diagnostics.combined).await?;
        let dispatch_ts = ctx.attempt.as_ref().map(|a| a.dispatch_ts);
        let outbox_present_recent = signals
            .outbox_meta
            .as_ref()
            .and_then(|m| {
                dispatch_ts
                    .map(|dispatch| chrono::DateTime::<chrono::Utc>::from(m.mtime) >= dispatch)
            })
            .unwrap_or(false)
            && signals
                .outbox_meta
                .as_ref()
                .map(|m| m.size > 0)
                .unwrap_or(false);

        let snapshot = ProbeSnapshot {
            cancelled: is_cancelled(),
            process_exited,
            process_alive: signals.process_alive,
            outbox_present: outbox_present_recent,
            provider_error: provider_error.as_ref().map(|m| m.class),
            success_contract: check_success_contract(ctx, &signals, role),
            soft_success_contract: check_soft_success_contract(ctx, &signals, role, now),
            protocol_invalid: check_protocol_invalid(ctx, role).await,
            provider_activity_after_exit_is_fresh: !provider_stale,
            work_signals_changed,
            last_work_signal_ts,
            provider_stale,
            grace_deadline,
            hang_max_with_work_signals_exceeded: hang_max_exceeded,
        };

        let prev_state = ctx.attempt.as_ref().map(|a| a.state);
        let attempt_state = crate::fsm::classify(&snapshot, now);
        ctx.last_phase_hint = Some(if signals.has_child_processes {
            "verify_or_build".to_owned()
        } else if !newly_changed.is_empty() || !resolved.is_empty() {
            "editing".to_owned()
        } else if signals
            .outbox_meta
            .as_ref()
            .map(|m| m.size > 0)
            .unwrap_or(false)
        {
            "writing_or_finalizing_outbox".to_owned()
        } else {
            "idle_or_reading".to_owned()
        });

        tracing::debug!(
            "probe: role={role:?} state={attempt_state:?} work_changed={work_signals_changed} \
             provider_stale={provider_stale} children={} alive={}",
            signals.has_child_processes,
            signals.process_alive,
        );

        let current_action = extract_provider_action_tail(&diagnostics.stdout, &diagnostics.stderr);
        match (&ctx.last_provider_action, &current_action) {
            (Some(prev), Some(curr)) if prev == curr => {}
            (_, Some(curr)) => {
                ctx.last_provider_action = Some(curr.clone());
                ctx.last_provider_action_ts = Some(now);
                tracing::info!(
                    "agent_started_command: role={role:?} provider={provider} cmd={curr:?}"
                );
            }
            (_, None) => {
                ctx.last_provider_action = None;
                ctx.last_provider_action_ts = None;
            }
        }

        if prev_state != Some(attempt_state) {
            tracing::info!("state change: role={role:?} {prev_state:?} → {attempt_state:?}");
        }

        if let Some(attempt) = ctx.attempt.as_mut() {
            attempt.state = attempt_state;
            if work_signals_changed {
                attempt.last_work_signal_ts = Some(now);
            }
            attempt.next_probe_at = now + chrono::Duration::seconds(PROBE_INTERVAL_SEC as i64);
        }

        // Emit heartbeat at the configured interval.
        let emit_heartbeat = ctx
            .attempt
            .as_ref()
            .map(|a| {
                a.last_heartbeat_ts
                    .map(|t| (now - t).num_seconds() as u64 >= HEARTBEAT_INTERVAL_SEC)
                    .unwrap_or(true)
            })
            .unwrap_or(false);
        if emit_heartbeat {
            let stale_secs = ctx
                .attempt
                .as_ref()
                .and_then(|a| a.last_work_signal_ts)
                .map(|t| (now - t).num_seconds().max(0))
                .unwrap_or(0);
            let phase_hint = ctx.last_phase_hint.as_deref().unwrap_or("unknown_phase");
            let dirty_hot = top_hot_files(&ctx.dirty_file_touch_counts, 3);
            let committed_hot = top_hot_files(&ctx.committed_file_touch_counts, 3);
            tracing::info!(
                "heartbeat: role={role:?} state={attempt_state:?} \
                 last_signal={stale_secs}s ago provider_stale={provider_stale} \
                 phase_hint={phase_hint} dirty_files={} committed_files={} \
                 dirty_hot_files={:?} committed_hot_files={:?}",
                ctx.last_dirty_files_count,
                ctx.committed_files_seen_count,
                dirty_hot,
                committed_hot
            );
            if let (Some(cmd), Some(ts)) = (&ctx.last_provider_action, ctx.last_provider_action_ts)
            {
                let elapsed = (now - ts).num_seconds().max(0);
                tracing::info!(
                    "agent_still_running_command: role={role:?} provider={provider} elapsed={}s cmd={cmd:?}",
                    elapsed
                );
            }
            if let Some(attempt) = ctx.attempt.as_mut() {
                attempt.last_heartbeat_ts = Some(now);
            }
        }

        // Route based on classified state.
        match attempt_state {
            AttemptState::Success | AttemptState::SoftSuccess => {
                let allowed = if attempt_state == AttemptState::SoftSuccess {
                    let outbox_present = signals
                        .outbox_meta
                        .as_ref()
                        .map(|m| m.size > 0)
                        .unwrap_or(false);
                    ctx.outbox_present = outbox_present;
                    soft_success_allowed(ctx.args.workflow_type, &ctx.artifact_map)
                } else {
                    ctx.outbox_present = signals
                        .outbox_meta
                        .as_ref()
                        .map(|m| m.size > 0)
                        .unwrap_or(false);
                    true
                };

                if !allowed {
                    ctx.run_state = RunState::RunFailedProtocol;
                    ctx.failures.push(
                        "soft_success not allowed for this workflow/artifact state".to_owned(),
                    );
                } else {
                    release_lock(ctx, role);
                    ctx.run_state = if role == AgentRole::Executor {
                        RunState::ExecutorOutputCollect
                    } else {
                        RunState::ReviewerOutputCollect
                    };
                }
                return Ok(());
            }

            AttemptState::FailedServiceCap => {
                release_lock(ctx, role);
                ctx.run_state = RunState::RunFailedServiceCap;
                return Ok(());
            }

            AttemptState::FailedProviderAccess => {
                release_lock(ctx, role);
                if let Some(matched) = provider_error {
                    ctx.failures.push(format!(
                        "provider access error: class={:?} pattern={} fragment={}",
                        matched.class, matched.pattern, matched.fragment
                    ));
                } else {
                    ctx.failures.push(
                        "provider access error: matched by classifier (pattern/fragment unavailable)"
                            .to_owned(),
                    );
                }
                ctx.run_state = RunState::RunFailedProviderAccess;
                return Ok(());
            }

            AttemptState::FailedTransport
            | AttemptState::FailedSilent
            | AttemptState::FailedProtocol => {
                release_lock(ctx, role);
                ctx.consecutive_failure_count += 1;
                ctx.run_state = RunState::RoundRetryDecide;
                return Ok(());
            }

            AttemptState::HangConfirmed => {
                if let Some(attempt) = ctx.attempt.as_mut() {
                    attempt.state = AttemptState::TerminatingStale;
                }
                terminate_active_child(ctx).await;
                release_lock(ctx, role);
                ctx.consecutive_failure_count += 1;
                ctx.run_state = RunState::RoundRetryDecide;
                return Ok(());
            }

            AttemptState::Finalizing => {
                if let Some(outbox_meta) = signals.outbox_meta.as_ref() {
                    let outbox_ts = chrono::DateTime::<chrono::Utc>::from(outbox_meta.mtime);
                    if dispatch_ts
                        .map(|dispatch| outbox_ts >= dispatch)
                        .unwrap_or(false)
                    {
                        let finalize_after = outbox_ts
                            + chrono::Duration::seconds(OUTBOX_FINALIZING_KILL_SEC as i64);
                        if now >= finalize_after {
                            tracing::warn!(
                                "finalizing timeout: role={role:?} process still alive {}s after outbox write; force-stopping",
                                OUTBOX_FINALIZING_KILL_SEC
                            );
                            terminate_active_child(ctx).await;
                            if let Some(attempt) = ctx.attempt.as_mut() {
                                attempt.pid = None;
                            }
                        }
                    }
                }
                // Keep monitoring loop alive until next probe classifies Success.
            }

            AttemptState::Cancelled => {
                terminate_active_child(ctx).await;
                release_lock(ctx, role);
                ctx.run_state = RunState::RunAborted;
                return Ok(());
            }

            _ => {
                // Still running/waiting — loop.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: detect provider error from logs
// ---------------------------------------------------------------------------

struct ProviderDiagnostics {
    stdout: String,
    stderr: String,
    combined: String,
}

async fn read_provider_diagnostics(
    ctx: &OrchestratorCtx,
) -> OrchestratorResult<ProviderDiagnostics> {
    let Some(proc) = &ctx.active_process else {
        return Ok(ProviderDiagnostics {
            stdout: String::new(),
            stderr: String::new(),
            combined: String::new(),
        });
    };

    // stdout/stderr are captured to files from spawn — safe to read while the process is alive.
    // This matters for providers like OpenCode that hang on service cap without exiting.
    let (stdout, stderr) = read_diagnostics(proc).await?;
    let combined = format!("{stdout}\n{stderr}");
    Ok(ProviderDiagnostics {
        stdout,
        stderr,
        combined,
    })
}

async fn detect_provider_error(
    ctx: &mut OrchestratorCtx,
    combined: &str,
) -> OrchestratorResult<Option<ProviderErrorMatch>> {
    if let Some(matched) = classify_provider_error_with_match(combined) {
        if matches!(
            matched.class,
            ProviderErrorClass::Auth
                | ProviderErrorClass::Permission
                | ProviderErrorClass::Transport
        ) {
            let diagnostic = format!(
                "provider diagnostic (non-fatal): class={:?} pattern={} fragment={}",
                matched.class, matched.pattern, matched.fragment
            );
            if !ctx.warnings.iter().any(|w| w == &diagnostic) {
                tracing::error!("{diagnostic}");
                ctx.warnings.push(diagnostic);
            }
            return Ok(None);
        }
        return Ok(Some(matched));
    }

    // Fallback: check Claude JSONL for service-cap signals that may not be present
    // in stdout/stderr (max_tokens overflow, rate_limit synthetic API errors).
    // This runs while the process is alive; the parser ignores incomplete JSONL records.
    let Some(role) = ctx.attempt.as_ref().map(|a| a.role) else {
        return Ok(None);
    };
    let provider = match role {
        AgentRole::Executor => ctx.args.executor_provider,
        AgentRole::Reviewer => ctx.args.reviewer_provider,
    };
    if provider == ProviderKind::Claude {
        if let Some(session_id) = ctx.session_id(role) {
            let Some(dispatch_ts) = ctx.attempt.as_ref().map(|a| a.dispatch_ts) else {
                return Ok(None);
            };
            if let Some(signal) = crate::providers::claude::session_jsonl_service_cap_since(
                ctx.repo(),
                session_id,
                dispatch_ts,
            )
            .await?
            {
                tracing::warn!(
                    "provider error: claude JSONL service cap — pattern={} fragment={}",
                    signal.pattern,
                    signal.fragment
                );
                return Ok(Some(ProviderErrorMatch {
                    class: ProviderErrorClass::ServiceCap,
                    pattern: signal.pattern,
                    fragment: signal.fragment,
                }));
            }
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Helper: check contract conditions
// ---------------------------------------------------------------------------

fn check_success_contract(ctx: &OrchestratorCtx, signals: &ProbeSignals, role: AgentRole) -> bool {
    let process_exited = ctx
        .attempt
        .as_ref()
        .map(|a| a.pid.is_none())
        .unwrap_or(false);

    if !process_exited {
        return false;
    }

    match role {
        AgentRole::Executor => {
            // Outbox present is sufficient to route to reviewer; artifact presence
            // is a quality concern for the reviewer, not a routing precondition.
            signals
                .outbox_meta
                .as_ref()
                .map(|m| m.size > 0)
                .unwrap_or(false)
        }
        AgentRole::Reviewer => {
            // Reviewer success is checked in ReviewerOutputCollect after YAML parse.
            signals
                .outbox_meta
                .as_ref()
                .map(|m| m.size > 0)
                .unwrap_or(false)
        }
    }
}

fn check_soft_success_contract(
    ctx: &OrchestratorCtx,
    signals: &ProbeSignals,
    role: AgentRole,
    now: DateTime<Utc>,
) -> bool {
    if role == AgentRole::Reviewer {
        return false; // no soft success for reviewer
    }
    let process_exited = ctx
        .attempt
        .as_ref()
        .map(|a| a.pid.is_none())
        .unwrap_or(false);
    if !process_exited {
        return false;
    }

    let workflow_type = ctx.args.workflow_type;
    let outbox_ok = signals
        .outbox_meta
        .as_ref()
        .map(|m| m.size > 0)
        .unwrap_or(false);
    let grace_expired = ctx
        .attempt
        .as_ref()
        .and_then(|a| a.grace_until_outbox.or(a.grace_until_commit))
        .map(|deadline| now >= deadline)
        .unwrap_or(false);

    if !grace_expired {
        return false;
    }

    match workflow_type {
        WorkflowType::Plan => {
            !outbox_ok
                && signals
                    .artifact_map
                    .get("PLAN.md")
                    .copied()
                    .unwrap_or(false)
        }
        WorkflowType::Investigate => {
            !outbox_ok
                && signals
                    .artifact_map
                    .get("INVESTIGATION.md")
                    .copied()
                    .unwrap_or(false)
        }
        WorkflowType::Implement => !outbox_ok,
    }
}

async fn check_protocol_invalid(ctx: &OrchestratorCtx, role: AgentRole) -> bool {
    if role == AgentRole::Executor {
        return false; // protocol_invalid only applies to reviewer YAML
    }
    // Do not open outbox while process is still alive (executor outbox opacity rule).
    let process_alive = ctx
        .attempt
        .as_ref()
        .map(|a| a.pid.is_some())
        .unwrap_or(false);
    if process_alive {
        return false;
    }

    let outbox_path = ctx
        .repo()
        .join(crate::constants::TRANSPORT_DIR)
        .join(crate::constants::OUTBOX_FILE);

    match tokio::fs::read_to_string(&outbox_path).await {
        Ok(raw) if !raw.trim().is_empty() => parse_reviewer_yaml(&raw).is_err(),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Helper: update grace periods (executor only)
// ---------------------------------------------------------------------------

fn update_grace_periods(ctx: &mut OrchestratorCtx, signals: &ProbeSignals, now: DateTime<Utc>) {
    let Some(attempt) = ctx.attempt.as_mut() else {
        return;
    };

    let git_dirty = signals.git_status_hash != attempt.dispatch_git_status_hash;
    let git_committed = signals.git_head_hash != attempt.dispatch_git_head_hash;
    let outbox_present = signals
        .outbox_meta
        .as_ref()
        .map(|m| m.size > 0)
        .unwrap_or(false);

    let (new_commit, new_outbox) = apply_grace_transitions(
        git_dirty,
        git_committed,
        outbox_present,
        attempt.grace_until_commit,
        attempt.grace_until_outbox,
        now,
    );

    if new_commit != attempt.grace_until_commit {
        attempt.grace_until_commit = new_commit;
        tracing::debug!("set commit grace until {:?}", attempt.grace_until_commit);
    }
    if new_outbox != attempt.grace_until_outbox {
        attempt.grace_until_outbox = new_outbox;
        tracing::debug!("set outbox grace until {:?}", attempt.grace_until_outbox);
    }
}

/// Pure grace-transition logic — separated from `OrchestratorCtx` for testability.
///
/// Returns `(grace_until_commit, grace_until_outbox)` after applying transitions.
fn apply_grace_transitions(
    git_dirty: bool,
    git_committed: bool,
    outbox_present: bool,
    grace_until_commit: Option<DateTime<Utc>>,
    grace_until_outbox: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let mut commit = grace_until_commit;
    let mut outbox = grace_until_outbox;

    if outbox_present {
        return (None, None);
    }

    // Commit appeared and outbox is still missing: wait for executor's final report.
    if git_committed {
        if outbox.is_none() {
            outbox = Some(now + chrono::Duration::seconds(OUTBOX_GRACE_SEC as i64));
        }
        return (None, outbox);
    }

    // Dirty worktree without commit yet: wait for the executor to finalize a commit.
    if git_dirty && commit.is_none() && outbox.is_none() {
        commit = Some(now + chrono::Duration::seconds(COMMIT_GRACE_SEC as i64));
    }

    (commit, outbox)
}

// ---------------------------------------------------------------------------
// Helper: terminate active child / release lock
// ---------------------------------------------------------------------------

async fn terminate_active_child(ctx: &mut OrchestratorCtx) {
    let Some(proc) = ctx.active_process.as_mut() else {
        return;
    };
    if let Some(pid) = proc.child.id() {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }

    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(CANCEL_CHILD_WAIT_SEC);

    loop {
        if proc.child.try_wait().ok().flatten().is_some() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = proc.child.kill().await;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

pub(super) fn release_lock(ctx: &mut OrchestratorCtx, role: AgentRole) {
    match role {
        AgentRole::Executor => ctx.executor_lock = None,
        AgentRole::Reviewer => ctx.reviewer_lock = None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn t(offset_secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + offset_secs, 0).unwrap()
    }

    const NOW: fn() -> DateTime<Utc> = || t(0);

    #[test]
    fn parse_git_status_path_handles_standard_short_status() {
        assert_eq!(
            parse_git_status_path(" M PLAN.md").as_deref(),
            Some("PLAN.md")
        );
        assert_eq!(
            parse_git_status_path("?? PLAN.md").as_deref(),
            Some("PLAN.md")
        );
        assert_eq!(
            parse_git_status_path(" R old.md -> new.md").as_deref(),
            Some("new.md")
        );
    }

    #[test]
    fn parse_git_status_path_handles_trimmed_short_status() {
        assert_eq!(
            parse_git_status_path("M PLAN.md").as_deref(),
            Some("PLAN.md")
        );
        assert_eq!(
            parse_git_status_path("R old.md -> new.md").as_deref(),
            Some("new.md")
        );
    }

    #[test]
    fn git_status_lines_preserve_index_worktree_columns() {
        let lines = git_status_lines(" M PLAN.md\n?? NEW.md\n\n");

        assert!(lines.contains(" M PLAN.md"));
        assert!(lines.contains("?? NEW.md"));
        assert!(!lines.contains("M PLAN.md"));
    }

    #[test]
    fn parse_git_name_status_path_handles_standard_name_status() {
        assert_eq!(
            parse_git_name_status_path("A\tPLAN.md").as_deref(),
            Some("PLAN.md")
        );
        assert_eq!(
            parse_git_name_status_path("M\tmodules/app.rs").as_deref(),
            Some("modules/app.rs")
        );
        assert_eq!(
            parse_git_name_status_path("D\told.md").as_deref(),
            Some("old.md")
        );
    }

    #[test]
    fn parse_git_name_status_path_handles_rename_name_status() {
        assert_eq!(
            parse_git_name_status_path("R100\told.md\tnew.md").as_deref(),
            Some("new.md")
        );
    }

    #[test]
    fn format_git_name_status_line_removes_raw_tabs() {
        let added = format_git_name_status_line("A\tdrone_city_nav/src/grid_overlay.cpp")
            .expect("line should format");
        assert_eq!(added, "A drone_city_nav/src/grid_overlay.cpp");
        assert!(!added.contains('\t'));

        let renamed = format_git_name_status_line("R100\told name.md\tnew name.md")
            .expect("line should format");
        assert_eq!(renamed, "R100 old name.md -> new name.md");
        assert!(!renamed.contains('\t'));
    }

    #[test]
    fn normalize_provider_action_truncates_utf8_safely() {
        let raw = format!("{}é", "a".repeat(219));

        let action = normalize_provider_action(&raw).unwrap();

        assert_eq!(action, format!("{}...", "a".repeat(219)));
    }

    #[test]
    fn grace_opens_on_dirty_worktree() {
        let (commit, outbox) = apply_grace_transitions(true, false, false, None, None, NOW());
        assert!(commit.is_some());
        assert!(outbox.is_none());
        let secs = (commit.unwrap() - NOW()).num_seconds();
        assert_eq!(secs, COMMIT_GRACE_SEC as i64);
    }

    #[test]
    fn grace_opens_on_new_commit_even_when_worktree_clean() {
        // Agent committed → wait for the final outbox report.
        let (commit, outbox) = apply_grace_transitions(false, true, false, None, None, NOW());
        assert!(commit.is_none());
        assert!(outbox.is_some());
        let secs = (outbox.unwrap() - NOW()).num_seconds();
        assert_eq!(secs, OUTBOX_GRACE_SEC as i64);
    }

    #[test]
    fn grace_does_not_open_when_nothing_happened() {
        let (commit, outbox) = apply_grace_transitions(false, false, false, None, None, NOW());
        assert!(commit.is_none());
        assert!(outbox.is_none());
    }

    #[test]
    fn outbox_grace_promotes_from_commit_grace() {
        let existing_commit_grace = Some(t(100));
        let (commit, outbox) =
            apply_grace_transitions(false, true, false, existing_commit_grace, None, NOW());
        assert!(
            commit.is_none(),
            "commit grace must be cleared after commit"
        );
        assert!(outbox.is_some());
        let secs = (outbox.unwrap() - NOW()).num_seconds();
        assert_eq!(secs, OUTBOX_GRACE_SEC as i64);
    }

    #[test]
    fn outbox_present_clears_grace() {
        let (commit, outbox) = apply_grace_transitions(false, true, true, None, None, NOW());
        assert!(commit.is_none());
        assert!(outbox.is_none());
    }

    #[test]
    fn grace_not_reopened_when_already_set() {
        let existing = Some(t(500));
        let (commit, outbox) = apply_grace_transitions(true, false, false, existing, None, NOW());
        assert_eq!(commit, existing);
        assert!(outbox.is_none());
    }

    #[test]
    fn outbox_grace_not_reopened_when_already_set() {
        let existing_outbox = Some(t(500));
        let (commit, outbox) =
            apply_grace_transitions(true, true, false, None, existing_outbox, NOW());
        assert!(commit.is_none());
        assert_eq!(outbox, existing_outbox);
    }
}
