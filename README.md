# multi-agent-orchestrator-rs

Deterministic Rust orchestrator for delegated coding-agent workflows. Coordinates an executor agent and an independent reviewer agent through a file-based transport protocol and an FSM-driven main loop.

## What it does

Runs a structured workflow where an executor agent (Claude, OpenCode, or Codex) performs the work and a reviewer agent independently evaluates the result. The orchestrator handles all state transitions, retries, hang detection, and quality gating deterministically — no LLM reasoning inside the orchestrator itself.

Three workflow types:

- **investigate** — input: user prompt; output: `INVESTIGATION.md` with evidence-backed research findings and conclusions
- **plan** — input: user prompt plus optional `INVESTIGATION.md`; output: `PLAN.md` with implementation plan and automated test strategy
- **implement** — input: user prompt plus optional `PLAN.md`; output: local commit(s) with implementation and test changes

The loop runs until the reviewer returns `decision: accept` with `quality_score >= 8`, a hard stop condition is reached, or the operator cancels.

## Prerequisites

- Rust (stable)
- Provider CLIs installed and authenticated for each provider you intend to use as executor or reviewer: `claude`, `opencode`, and/or `codex`
- Two pre-existing provider sessions — one for executor, one for reviewer. Each session must have a discoverable title/thread name. Executor and reviewer may use the same or different providers, but they must resolve to distinct `(provider, session_id)` bindings.
- A **clean git worktree** in the target repository. The orchestrator aborts immediately if `git status` shows any uncommitted changes.

The orchestrator never creates provider sessions or chats. It deterministically discovers existing sessions at startup and stops with `failed_session_bind` if binding is missing, ambiguous, or resolves executor and reviewer to the same provider session.

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
  --remote-network-policy <forbidden|read_only|operational> \
  --executor-provider <claude|opencode|codex> \
  --reviewer-provider <claude|opencode|codex> \
  [--executor-model <provider/model>] \
  [--reviewer-model <provider/model>]
```

Both provider flags are required. There are no default providers because provider choice controls session discovery and execution semantics.
`--notion-policy` defaults to `optional`.
`--remote-network-policy` defaults to `forbidden`.
`--executor-model` and `--reviewer-model` are optional OpenCode-only overrides. When provided, the orchestrator validates the model at startup with `opencode models <provider>` and passes it to `opencode run --model <provider/model>`.

Notion behavior is defined by [`docs/notion_access_protocol.md`](docs/notion_access_protocol.md).
GitLab read-only access behavior is defined by [`docs/gitlab_access_protocol.md`](docs/gitlab_access_protocol.md).

Remote network policy controls what the agents are allowed to do against remote target systems:

- `forbidden` — default; SSH and HTTP requests to remote target systems are forbidden.
- `read_only` — SSH/HTTP access is allowed only for immutable investigation actions; mutating commands, state changes, service restarts, DB writes, and unpacking remote log archives are forbidden.
- `operational` — explicitly allows limited operational actions on the user-specified remote target system, while still forbidding DB writes, OS/system-level destructive changes, actions outside the target system, and unpacking remote log archives.

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

Executor and reviewer must resolve to distinct `(provider, session_id)` bindings. Reusing the same thread name is valid across different providers, but same-provider roles should use separate sessions. Output is always JSON on stdout; logs go to stderr.

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
  "remote_network_policy": "forbidden",
  "executor_provider": "opencode",
  "executor_session_id": "sess_abc123",
  "executor_thread_name": "TASK-42 add pagination",
  "executor_model": "deepseek/deepseek-v4-flash",
  "reviewer_provider": "codex",
  "reviewer_session_id": "sess_def456",
  "reviewer_thread_name": "TASK-42 add pagination",
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
    "body": "quality_score: 9\ndecision: accept\nrationale: Implementation is complete and correct.\ncontract_satisfied: true\n..."
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

**Transport:** executor and reviewer communicate via `.agent-io/inbox.txt` (prompt) and `.agent-io/outbox.txt` (result) inside `--workspace-root`. These files are never committed. On first run, `.agent-io/` is automatically added to `.git/info/exclude` so transport files never appear as untracked. Provider trigger prompts and generated agent prompts use absolute `inbox_path`/`outbox_path` to prevent agents from resolving `.agent-io` relative to the orchestrator repository. The monitor uses outbox metadata only (existence/size/mtime) for orchestration decisions. After executor completion, the orchestrator may read the raw executor outbox body only to log it and include it in the final JSON report; it must not parse, grep, classify, summarize, semantically inspect, or route based on that body. The reviewer is the only semantic consumer of executor outbox. The reviewer reads executor outbox directly and overwrites it with a YAML verdict.

**Monitoring cadence:** the monitor probes local process/file/provider signals every 1 s and emits a heartbeat every 300 s. Heartbeats include `phase_hint`, changed-file count, hot files, provider staleness, and the last detected provider command when available.

**Hang detection:** dual-condition — work signals absent ≥ 420 s AND provider log stale ≥ 420 s advances to `HangConfirmed`. A live attempt with continuing work signals but no result has a 2400 s hard ceiling before provider-log confirmation is required. After `HangConfirmed` the attempt is killed, retried, and the consecutive failure counter increments. Provider signal read failures (permission errors, malformed provider CLI output, failed provider signal commands) are fatal orchestration errors rather than ignored monitoring gaps.

**Finalizing:** when a non-empty outbox written after the current dispatch is detected while the provider process is still alive, the attempt enters `Finalizing`. If the process has not exited 60 s after that outbox mtime, the orchestrator force-stops it so the run can proceed to output collection.

**Retries:** only infra failures count (crashes, silent exits, hangs). Reviewer `revise` is a successful semantic round and resets the counter. The revise loop is intentionally unbounded — the reviewer decides when to return `accept` or `blocked`.

Reviewer retries are intentionally allowed even when the reviewer already modified `.agent-io/outbox.txt`. This relies on persistent provider sessions: if the reviewer cleared or overwrote outbox, it is assumed to have already read the executor result and to retain that context in its session. A retried reviewer batch request is therefore expected to continue in the same reviewer session, not replay from a stateless process snapshot. If the provider loses that context, retries may fail again and eventually stop at the consecutive failure limit.

**Session locking:** a file lock keyed by `(provider, session_id)` prevents two orchestrator processes from dispatching to the same session simultaneously.

**Agent run limits:** the orchestrator performs a `runlim` preflight (`command -v runlim` via shell `PATH`, plus an interactive-bash probe for `~/.bashrc`-provided commands), accepts only an absolute executable file path, logs the resolved source path, and injects that absolute path into normal executor/reviewer prompts for `cargo run`/`cargo test` examples. Reviewer YAML-repair prompts do not include this hint. Provider dispatch env also enriches `PATH` with `~/.local/bin` and `/usr/local/bin` so non-interactive agent subprocesses can resolve `runlim` more reliably.

**Agent test hygiene:** provider processes are dispatched with `PROPTEST_DISABLE_FAILURE_PERSISTENCE=1` so agent-run `cargo test` commands do not create `*.proptest-regressions` files in the target workspace. Normal executor/reviewer prompts also explicitly tell agents to use `PROPTEST_DISABLE_FAILURE_PERSISTENCE=1 cargo test ...` and not to leave or commit proptest regression files.

## Logging

Logs go to stderr via `tracing`; the final JSON report is the only stdout output. Default log level is `info`. Override it with `RUST_LOG`, for example:

```bash
RUST_LOG=debug orchestrate ...
```

At `info` level the orchestrator logs:

- context prep facts: workspace root, workflow, branch, initial HEAD, Claude project key when relevant;
- fresh session discovery facts: executor/reviewer provider and session ID;
- every generated inbox prompt, including path, size, line count, SHA-256, mtime, UTF-8 lossiness, and full body;
- executor outbox raw diagnostic snapshot after executor completion, with the same metadata and full body;
- reviewer outbox raw YAML snapshot after reviewer completion, with the same metadata and full body;
- dispatch facts: role, provider, session ID, fixed template, request SHA-256, child PID;
- git deltas while an agent is working, including newly changed/resolved status lines and top hot files;
- heartbeat context: phase hint, changed-file count, hot files, provider staleness, and last signal age;
- provider action tail when detected from provider diagnostics, for example long-running `cargo test`, `cargo clippy`, `make`, `rg`, or shell commands;
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

The orchestrator does not persist session bindings. Every run performs fresh deterministic discovery from provider state using `--workspace-root`, provider, and thread name.

Before the first run, create two sessions in your provider(s) and give each a title that matches the thread name you will pass on the CLI.

**Claude**

The orchestrator discovers Claude sessions from `~/.claude/projects/<project-key>/*.jsonl` by matching the session title. Create a new conversation in the Claude desktop app or CLI and set its title to match `--executor-thread-name` (or `--reviewer-thread-name`). The session ID is discovered automatically; no manual ID is needed.

**OpenCode**

The orchestrator discovers OpenCode sessions via `opencode session list --format json`, filtering by `directory == workspace-root` and `title == thread-name`. Create a session inside the target workspace directory and set its title accordingly.

OpenCode dispatch automatically uses a user `systemd-run` scope when available. Before dispatch, the orchestrator probes `systemd-run --user --scope --collect --same-dir --quiet ... true`; if it succeeds, OpenCode is launched as:

```bash
systemd-run --user --scope --collect --same-dir --quiet \
  -p MemoryHigh=18G \
  -p MemoryMax=20G \
  -p MemorySwapMax=2G \
  opencode run ...
```

If the probe fails, OpenCode is launched directly as `opencode run ...`.

Optional OpenCode model overrides use OpenCode's canonical `provider/model` format:

```bash
orchestrate \
  --workflow plan \
  --workspace-root /home/user/projects/myapp \
  --executor-thread-name "TASK-42 executor" \
  --reviewer-thread-name "TASK-42 reviewer" \
  --prompt "Plan the fix" \
  --executor-provider opencode \
  --reviewer-provider opencode \
  --executor-model deepseek/deepseek-v4-flash \
  --reviewer-model zai/glm-4.7
```

Before dispatch, each supplied model is validated against the local OpenCode model list. If `zai/glm-4.7` is supplied, the validation command is equivalent to:

```bash
opencode models zai
```

The run fails with `failed_invalid_input` if the model is not listed or if a model override is used with a non-OpenCode provider.

**Codex**

The orchestrator discovers Codex sessions from `~/.codex/session_index.jsonl` by matching the latest `thread_name` row per session ID and filtering candidates to sessions whose rollout metadata records `cwd == --workspace-root`. Dangling index rows without resolvable cwd metadata are ignored. Create a named Codex session/thread from inside the target workspace directory whose `thread_name` matches the CLI thread name.

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
- **Executor and reviewer sessions must be distinct.** If both roles resolve to the same `(provider, session_id)`, the run fails at `SESSION_BIND` instead of risking mixed executor/reviewer context.

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
