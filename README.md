# multi-agent-orchestrator-rs

Deterministic Rust orchestrator for delegated coding-agent workflows. Coordinates an executor agent and an independent reviewer agent through a file-based transport protocol and an FSM-driven main loop.

## What it does

Runs a structured workflow where an executor agent (Claude, OpenCode, or Codex) performs the work and a reviewer agent independently evaluates the result. The orchestrator handles all state transitions, retries, hang detection, and quality gating deterministically — no LLM reasoning inside the orchestrator itself.

Three workflow types:

- **plan** — produce `PLAN.md` with implementation plan
- **investigate** — produce `INVESTIGATION.md` with root cause and fix direction
- **implement** — executor attempts implementation; reviewer verifies commits, worktree state, tests, and accepts or rejects the result via strict YAML

The loop runs until the reviewer accepts (`quality_score ≥ 8`), a hard stop condition is reached, or the operator cancels.

## Prerequisites

- Rust (stable)
- Provider CLIs installed and authenticated for each provider you intend to use as executor or reviewer: `claude`, `opencode`, and/or `codex`
- Two pre-existing provider sessions — one for executor, one for reviewer. Each session must have a title (thread name) that the orchestrator uses to discover it. Executor and reviewer may use the same or different providers, but must not share the same `(provider, thread_name)` pair.
- A **clean git worktree** in the target repository. The orchestrator aborts immediately if `git status` shows any uncommitted changes.

The orchestrator does not create sessions. It discovers existing ones by thread name at startup.

## Build

```bash
cargo build --release
# or via make:
make build
```

## Usage

```bash
orchestrate \
  --workflow <plan|investigate|implement> \
  --workspace-root <path> \
  --executor-thread-name <session-title> \
  --reviewer-thread-name <session-title> \
  --prompt <task-description> \
  --notion-policy <required|optional> \
  --executor-provider <claude|opencode|codex> \
  --reviewer-provider <claude|opencode|codex>
```

Both provider flags are required. There are no default providers because provider choice controls session discovery and execution semantics.
`--notion-policy` defaults to `optional`.

Notion behavior is defined by [`docs/notion_access_protocol.md`](docs/notion_access_protocol.md).

**Example — different providers, same thread name:**

```bash
orchestrate \
  --workflow implement \
  --workspace-root /home/user/projects/myapp \
  --executor-thread-name "TASK-42 add pagination" \
  --reviewer-thread-name "TASK-42 add pagination" \
  --prompt "Add cursor-based pagination to GET /api/posts. See PLAN.md for spec." \
  --notion-policy optional \
  --executor-provider opencode \
  --reviewer-provider codex
```

**Example — same provider, different thread names:**

```bash
orchestrate \
  --workflow implement \
  --workspace-root /home/user/projects/myapp \
  --executor-thread-name "TASK-42 executor" \
  --reviewer-thread-name "TASK-42 reviewer" \
  --prompt "Add cursor-based pagination to GET /api/posts. See PLAN.md for spec." \
  --notion-policy optional \
  --executor-provider claude \
  --reviewer-provider claude
```

Executor and reviewer must not share the same `(provider, thread_name)` pair. Output is always JSON on stdout; logs go to stderr.

**Exit codes:** `0` on `done_quality_reached`; `1` on any other terminal state.

## Output

A single JSON report on stdout. Optional fields (`quality_score`, `reviewer_decision`, `reviewer_rationale`, `next_action_required`, `detail`, etc.) are omitted entirely when absent. Reviewer payload fields (`reviewer_findings`, `reviewer_checks`, `reviewer_verification_commands`) are free-form JSON values converted from reviewer YAML and are omitted only when absent.

The report includes raw transport snapshots for maximum observability:

- `last_inbox`: the most recent generated prompt written to `.agent-io/inbox.txt`.
- `executor_outbox`: the executor's raw `.agent-io/outbox.txt` body captured after executor completion.
- `reviewer_outbox`: the reviewer's raw YAML `.agent-io/outbox.txt` body captured after reviewer completion.

Each snapshot contains `role`, `provider`, `session_id`, `path`, `sha256`, `bytes`, `lines`, `utf8_lossy`, `mtime`, and `body`.

```json
{
  "workflow_type": "implement",
  "state": "run_done",
  "reason_code": "done_quality_reached",
  "workspace_root": "/home/user/projects/myapp",
  "branch": "feature/pagination",
  "notion_policy": "optional",
  "executor_provider": "opencode",
  "executor_session_id": "sess_abc123",
  "executor_thread_name": "TASK-42 add pagination",
  "reviewer_provider": "codex",
  "reviewer_session_id": "sess_def456",
  "reviewer_thread_name": "TASK-42 add pagination",
  "metadata_commit_hash": "0f1e2d3c4b5a",
  "consecutive_failure_count": 0,
  "quality_score": 9,
  "reviewer_decision": "accept",
  "reviewer_rationale": "Implementation is complete and correct. All tests pass. Code follows existing conventions.",
  "reviewer_findings": [
    {
      "id": "F-001",
      "severity": "minor",
      "category": "tests",
      "path": "src/api/posts.rs",
      "line": 42,
      "message": "Edge case for empty cursor not tested",
      "evidence": "No test covers cursor='' in the test suite",
      "recommendation": "Add a test for empty cursor value"
    }
  ],
  "reviewer_checks": [
    {
      "name": "artifact_contract",
      "status": "passed",
      "evidence": "outbox.txt present, non-empty"
    },
    {
      "name": "verification_commands",
      "status": "passed",
      "evidence": "cargo test: 47 tests passed"
    }
  ],
  "reviewer_verification_commands": [
    {
      "command": "cargo test",
      "status": "passed",
      "summary": "47 tests passed, 0 failed"
    }
  ],
  "artifact_paths": [],
  "commit_hashes": ["a1b2c3d4e5f6"],
  "warnings": [],
  "failures": [],
  "last_inbox": {
    "role": "reviewer",
    "provider": "codex",
    "session_id": "sess_def456",
    "path": "/home/user/projects/myapp/.agent-io/inbox.txt",
    "sha256": "7a38...",
    "bytes": 9120,
    "lines": 212,
    "utf8_lossy": false,
    "mtime": "2026-05-01T12:00:00Z",
    "body": "..."
  },
  "executor_outbox": {
    "role": "executor",
    "provider": "opencode",
    "session_id": "sess_abc123",
    "path": "/home/user/projects/myapp/.agent-io/outbox.txt",
    "sha256": "18be...",
    "bytes": 1840,
    "lines": 36,
    "utf8_lossy": false,
    "mtime": "2026-05-01T11:59:40Z",
    "body": "Executor free-form result..."
  },
  "reviewer_outbox": {
    "role": "reviewer",
    "provider": "codex",
    "session_id": "sess_def456",
    "path": "/home/user/projects/myapp/.agent-io/outbox.txt",
    "sha256": "cd91...",
    "bytes": 1320,
    "lines": 48,
    "utf8_lossy": false,
    "mtime": "2026-05-01T12:03:00Z",
    "body": "role: reviewer\nworkflow_type: implement\n..."
  }
}
```

Terminal `reason_code` values: `done_quality_reached` · `stopped_irreconcilable_disagreement` · `stopped_service_cap` · `stopped_provider_access` · `stopped_poisoned_session` · `stopped_session_locked` · `stopped_consecutive_failure_limit` · `stopped_external_blocker` · `stopped_dirty_worktree` · `failed_session_bind` · `failed_invalid_input` · `failed_protocol` · `internal_error` · `cancelled_by_operator`.

## How it works

```
CONTEXT_PREP → SESSION_BIND → EXECUTOR_DISPATCH ←──────────────────────────────────────┐
                                       │                                                │
                                 EXECUTOR_MONITOR                                       │
                                 ┌─────┴──────────────────────────┐                    │
                        success  │              crash / hang /     │ budget remaining   │
                    soft-success │              silent exit        │                    │
                                 ↓                                 ↓                    │
                   EXECUTOR_OUTPUT_COLLECT              ROUND_RETRY_DECIDE ─────────────┘
                   → ORCH_VERIFY                                   │ budget exhausted
                   → REVIEWER_DISPATCH                             ↓
                   → REVIEWER_MONITOR              RUN_FAILED_CONSECUTIVE_FAILURE_LIMIT
                          │ success
                          ↓
                   REVIEWER_OUTPUT_COLLECT → QUALITY_GATE
                          ├─ accept                → RUN_DONE
                          ├─ revise                → ROUND_FEEDBACK_PREP → EXECUTOR_DISPATCH  (loop)
                          ├─ blocked               → RUN_FAILED_EXTERNAL_BLOCKER
                          └─ irreconcilable_disagreement / poisoned_session → terminal
```

**Transport:** executor and reviewer communicate via `.agent-io/inbox.txt` (prompt) and `.agent-io/outbox.txt` (result). These files are never committed. On first run, `.agent-io/` is automatically added to `.git/info/exclude` so transport files never appear as untracked. The monitor uses executor outbox metadata only (existence/size/mtime) for orchestration decisions. After executor completion, the orchestrator may read the raw executor outbox body only to log it and include it in the final JSON report; it must not parse, grep, classify, summarize, semantically inspect, or route based on that body. The reviewer is the only semantic consumer of executor outbox. The reviewer reads executor outbox directly and overwrites it with a YAML verdict.

**Hang detection:** dual-condition — work signals absent ≥ 300 s AND provider log stale ≥ 300 s. One stale condition alone only advances to `HangSuspected`. After `HangConfirmed` the attempt is killed, retried, and the consecutive failure counter increments. Provider signal read failures (permission errors, malformed provider CLI output, failed provider signal commands) are fatal orchestration errors rather than ignored monitoring gaps.

**Retries:** only infra failures count (crashes, silent exits, hangs). Reviewer `revise` is a successful semantic round and resets the counter. The revise loop is intentionally unbounded — the reviewer decides when to return `accept` or `blocked`.

Reviewer retries are intentionally allowed even when the reviewer already modified `.agent-io/outbox.txt`. This relies on persistent provider sessions: if the reviewer cleared or overwrote outbox, it is assumed to have already read the executor result and to retain that context in its session. A retried reviewer batch request is therefore expected to continue in the same reviewer session, not replay from a stateless process snapshot. If the provider loses that context, retries may fail again and eventually stop at the consecutive failure limit.

**Session locking:** a file lock keyed by `(provider, session_id)` prevents two orchestrator processes from dispatching to the same session simultaneously.

## Logging

Logs go to stderr via `tracing`; the final JSON report is the only stdout output. Default log level is `info`. Override it with `RUST_LOG`, for example:

```bash
RUST_LOG=debug orchestrate ...
```

At `info` level the orchestrator logs:

- context prep facts: workspace root, workflow, branch, initial HEAD, Claude project key when relevant;
- session binding facts: executor/reviewer provider and session ID;
- every generated inbox prompt, including path, size, line count, SHA-256, mtime, UTF-8 lossiness, and full body;
- executor outbox raw diagnostic snapshot after executor completion, with the same metadata and full body;
- reviewer outbox raw YAML snapshot after reviewer completion, with the same metadata and full body;
- dispatch facts: role, provider, session ID, fixed template, request SHA-256, child PID;
- quality gate decisions and retry/failure transitions.

Provider stdout/stderr tails and provider session logs are orchestration diagnostics and may be inspected for provider/service errors. Executor outbox body is logged as an opaque blob only; it is never parsed or used for control flow.

## Reviewer YAML schema

The reviewer agent must write a YAML document to `.agent-io/outbox.txt`. The orchestrator parses it strictly — unknown fields, anchors, aliases, custom tags, multiple documents, and Markdown fences are rejected. YAML comments are ignored by the parser and are not part of the protocol; reviewers are instructed not to put any meaningful information in comments.

```yaml
quality_score: 8.5
decision: accept
rationale: "Overall assessment of the work."
contract_satisfied: true
hard_blockers_present: false
notion_requirements_satisfied: true
feedback_for_executor: []
checks_performed: "free_form_or_object_or_array"
findings: "free_form_or_object_or_array"
verification_commands: "free_form_or_object_or_array"
blocking_reason: null
irreconcilable_reason: null
poisoned_session_reason: null
```

Strict rules:

- `decision`: `accept`, `revise`, `blocked`, `irreconcilable_disagreement`, `poisoned_session`.
- `quality_score`: number in `[0, 10]`.
- `decision=accept` requires `quality_score >= 8`.
- `decision=accept` requires `contract_satisfied=true`.
- `decision=accept` requires `hard_blockers_present=false`.
- `decision=revise` requires non-empty `feedback_for_executor`.
- `decision=blocked` requires non-empty `blocking_reason`.
- `decision=irreconcilable_disagreement` requires non-empty `irreconcilable_reason`.
- `decision=poisoned_session` requires non-empty `poisoned_session_reason`.
- With `--notion-policy required`, reviewer YAML must include `notion_requirements_satisfied: true|false`; `decision=accept` is forbidden when it is `false`.

## Session setup

> **Warning:** the orchestrator may write and commit `ORCHESTRATOR_SESSIONS.json` in the target workspace when session binding scope changes (workspace/provider/thread-name changes) or when metadata is absent. Ensure pre-commit hooks are non-blocking and that you have write access.

Before the first run, create two sessions in your provider(s) and give each a title that matches the thread name you will pass on the CLI.

**Claude**

The orchestrator discovers Claude sessions from `~/.claude/projects/<project-key>/*.jsonl` by matching the session title. Create a new conversation in the Claude desktop app or CLI and set its title to match `--executor-thread-name` (or `--reviewer-thread-name`). The session ID is discovered automatically; no manual ID is needed.

**OpenCode**

The orchestrator discovers OpenCode sessions via `opencode session list --format json`, filtering by `directory == workspace-root` and `title == thread-name`. Create a session inside the target workspace directory and set its title accordingly.

**Codex**

The orchestrator discovers Codex sessions from `~/.codex/session_index.jsonl` by matching `thread_name` and filtering candidates to sessions whose recorded cwd equals `--workspace-root`. Create a named Codex session/thread from inside the target workspace directory whose `thread_name` matches the CLI thread name.

**Example — Claude executor + Codex reviewer, same thread name:**

```bash
# 1. Create an executor session in Claude (desktop or CLI), title: "TASK-42 add pagination"
# 2. Create a reviewer session in Codex, thread_name: "TASK-42 add pagination"

orchestrate \
  --workflow implement \
  --workspace-root /home/user/projects/myapp \
  --executor-thread-name "TASK-42 add pagination" \
  --reviewer-thread-name "TASK-42 add pagination" \
  --prompt "..." \
  --executor-provider claude \
  --reviewer-provider codex
```

**Example — Claude executor + Claude reviewer, different thread names:**

```bash
# 1. Create executor session in Claude, title: "TASK-42 executor"
# 2. Create reviewer session in Claude, title: "TASK-42 reviewer"

orchestrate \
  --workflow implement \
  --workspace-root /home/user/projects/myapp \
  --executor-thread-name "TASK-42 executor" \
  --reviewer-thread-name "TASK-42 reviewer" \
  --prompt "..." \
  --executor-provider claude \
  --reviewer-provider claude
```

## Limitations

- **Revise loop is unbounded.** The orchestrator will keep iterating until the reviewer returns `accept`, `blocked`, or another terminal decision. There is no built-in round limit for the revise cycle.
- **`poisoned_session` requires manual intervention.** When the reviewer returns `poisoned_session`, the run terminates immediately. The affected session must be inspected and replaced manually before the next run.
- **Hang detection requires readable provider signals.** Missing provider session logs are treated as absent activity signals, but provider signal read failures are fatal orchestration errors. Examples include permission errors on local provider logs, malformed provider CLI JSON, or failed provider signal commands.
- **Worktree must be clean at start.** The orchestrator rejects dirty worktrees at `CONTEXT_PREP`. Stage or stash any in-progress changes before running.
- **Sessions must already exist.** The orchestrator never creates sessions. A missing or ambiguously named session fails at `SESSION_BIND` with `failed_session_bind`.

## Development

```bash
make check      # cargo check
make clippy     # lint (warnings as errors)
make fmt        # format
make test       # run all tests
```

All policy constants (probe interval, grace periods, hang thresholds, retry limit, quality gate) are in `src/constants.rs` with rationale comments.

## Agent context

`AGENTS.md` at the workspace root provides startup instructions for Claude Code (the CLI). It directs Claude Code to load project memory and `CLAUDE.md` context at the beginning of each session. It is not relevant to running the orchestrator itself.
