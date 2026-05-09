# Notion Access Protocol

This document is the single source of truth for how executor and reviewer read Notion tasks
in orchestrator runs.

## 1. Scope and authority

- Applies to all providers (`claude`, `opencode`, `codex`) and both roles (executor/reviewer).
- Trigger: user prompt mentions a Notion task (for example `8088`, `TASK-8088`, `notion task`).
- Policy source: only orchestrator runtime flag `--notion-policy required|optional`.
- Agents must not reinterpret policy intent.

## 2. Policy semantics

- `required`:
  - Reading the Notion task is mandatory.
  - If required reading fails, executor must stop immediately.
  - Reviewer must not return `decision: accept`.
  - Reviewer YAML must set `notion_requirements_satisfied: false` when required read failed/skipped.
- `optional`:
  - Reading the Notion task is not required.
  - Deterministic retry policy (up to 5 attempts with 60s timeout) is not required.
  - Work may continue when Notion is unavailable.
  - If Notion was not read, limitation should be explicitly reported.

## 3. Canonical command set

These commands are considered valid Notion CLI operations and may be referenced by agents.

### 3.1 Read commands (primary for this protocol)

```bash
# Discover / list
notion task list --limit 10
notion task list --status "In Progress"
notion task list --assignee "Name"

# Read rendered task
notion task view <id>

# Read raw markdown task
notion task pull <id>
notion task pull <id> --save-to /tmp/task.md
```

### 3.2 Mutation commands are out of scope

This protocol is strictly read-only. Do not use any mutating Notion commands (`update`, `push`,
`create`, `comment`) in orchestrator investigate/plan/implement rounds.

## 4. Deterministic read algorithm

When Notion is mentioned in user prompt:

1. Verify CLI availability:
   - `notion --help`
   - `notion task --help`
2. Normalize task id:
   - Accept short form (`8088`) and full form (`TASK-8088`).
3. Read primary task with bounded retries (`required` policy only):
   - retry policy is mandatory and deterministic:
     - max attempts: `5`
     - per-attempt timeout: `60s`
     - pause between attempts: `5s`
   - within each attempt:
     - first try `notion task view <id>`;
     - if failed, try `notion task pull <id>`.
   - success condition: at least one of `view` or `pull` succeeded in any attempt.
4. Parse references to related tasks (`TASK-<n>`) from fetched content.
5. Read each related task using the same retry policy and `view -> pull` fallback sequence (`required` policy only).
6. Optional repository cross-check for each related task:
   - `git log --all --grep="TASK-<id>" --oneline`
7. Under `required` policy, if any required read step failed:
   - executor must stop immediately and not continue code work;
   - reviewer must return non-accept decision (`blocked` preferred).
8. Under `optional` policy:
   - executor may continue work without reading Notion;
   - retries are optional;
   - if Notion was attempted and failed, report limitation and failed attempts.

## 5. Executor obligations

When Notion is mentioned in prompt and `notion_policy=required`:

- Include in outbox:
  - exact commands attempted;
  - attempt counters (`attempt X/5`);
  - command result per step: `ok` / `failed`;
  - short stderr/stdout tail for failures;
  - final boolean outcome: required task read succeeded or not.
- Under `required` and failed read:
  - outbox must contain only stop reason + unblock requirements;
  - no further code investigation/plan/implementation in this round.

When Notion is mentioned in prompt and `notion_policy=optional`:

- You may skip Notion reading entirely and continue work.
- If you skip Notion, explicitly record that this is an `optional` policy path.
- If you attempted Notion commands, report attempts/results, but 5x60s retries are optional.

## 6. Reviewer obligations

- Independently verify executor claims where possible.
- Under `required` policy:
  - `notion_requirements_satisfied: true` only when required Notion reading actually succeeded;
  - if not succeeded, `decision: accept` is forbidden.

## 7. Unblock guidance for `required`

Typical unblock actions:

1. Configure Notion access for the agent environment.
2. Provide direct Notion task URL/ID in accessible form.
3. Re-run with `--notion-policy optional` only if business context allows working without Notion.
