# GitLab Access Protocol

This document is the single source of truth for how executor and reviewer read GitLab merge requests
and discussions in orchestrator runs.

## 1. Scope and authority

- Applies to all providers (`claude`, `opencode`, `codex`) and both roles (executor/reviewer).
- Trigger: user prompt mentions GitLab/MR/merge request/review comments/discussions.
- Tooling source: `glab` CLI for GitLab reads.
- Agents must not reinterpret read-only constraints.

## 2. Read-only policy

- Allowed: read-only GitLab operations (`mr view`, `mr diff`, `mr view --comments`, `glab api` GET for MR notes/discussions, pipelines read).
- Forbidden: any mutating GitLab actions in orchestrator rounds, including:
  - creating/updating/closing/reopening MR;
  - posting/editing/deleting notes/comments;
  - approving/unapproving, assigning/reassigning, labeling;
  - pushing commits, rebasing, merging from CLI.
- If user explicitly asks for a mutating GitLab action, it must be handled as a separate explicit task, not as part of read-only investigate/plan/review flow.

## 3. Canonical command set (read-only)

### 3.1 Merge request context

```bash
# Current branch MR
glab mr view

# Explicit MR id/iid/url
glab mr view <mr>

# Diff
glab mr diff <mr>

# Quick comments/activity view in CLI output
glab mr view <mr> --comments
```

### 3.2 Notes/discussions via API (version-compatible, read-only)

`glab 1.53.0` does not provide `glab mr note list <mr>` read command.
Use read-only API endpoints instead:

```bash
# Option A: from inside the target repository (uses current repo placeholders)
glab api "projects/:fullpath/merge_requests/<iid>/notes?per_page=100"
glab api "projects/:fullpath/merge_requests/<iid>/discussions?per_page=100"

# Option B: explicit GitLab host and URL-encoded project path
glab api --hostname <gitlab-host> "projects/<urlencoded_group%2Fproject>/merge_requests/<iid>/notes?per_page=100"
glab api --hostname <gitlab-host> "projects/<urlencoded_group%2Fproject>/merge_requests/<iid>/discussions?per_page=100"
```

Practical way to get `<iid>` and `<group/project>`:

```bash
glab mr view <mr> --output json
```

### 3.3 Pipeline status (read-only)

```bash
glab pipeline list
glab pipeline status
```

### 3.4 Optional Notion cross-reference

If MR description contains `TASK-<id>` or Notion URL, read task context:

```bash
notion task view <id>
```

## 4. Deterministic read algorithm

When GitLab MR review/study is requested:

1. Resolve target MR:
   - if argument provided, use it;
   - otherwise use MR for current branch (`glab mr view`).
2. Read MR metadata:
   - `glab mr view <mr>`
3. Read diff:
   - `glab mr diff <mr>`
4. Read notes/discussions:
   - first try `glab mr view <mr> --comments` for quick context;
   - then fetch full notes/discussions via API:
     - from inside the target repository:
       - `glab api "projects/:fullpath/merge_requests/<iid>/notes?per_page=100"`
       - `glab api "projects/:fullpath/merge_requests/<iid>/discussions?per_page=100"`
     - or with explicit host/project:
       - `glab api --hostname <gitlab-host> "projects/<urlencoded_group%2Fproject>/merge_requests/<iid>/notes?per_page=100"`
       - `glab api --hostname <gitlab-host> "projects/<urlencoded_group%2Fproject>/merge_requests/<iid>/discussions?per_page=100"`
5. Parse and report:
   - title, author, target branch, CI/pipeline status;
   - changed files;
   - comments/notes, including open vs resolved concerns where detectable;
   - key reviewer concerns already raised (avoid duplicating already-addressed points).
6. For each changed file, read full file in repo (not only diff hunks) before final conclusions.

## 5. Executor obligations

- In outbox, explicitly list:
  - exact GitLab commands run;
  - MR identity used (current branch or explicit `<mr>`);
  - what was read from notes/discussions;
  - what could not be verified (if any).
- Do not perform or suggest hidden mutating actions under read-only workflow.

## 6. Reviewer obligations

- Independently verify executor claims using the same canonical read-only commands where possible.
- Validate that:
  - conclusions are based on MR metadata + diff + notes, not only self-report;
  - open concerns in notes/discussions are accounted for in conclusions;
  - no mutating GitLab action was performed.

## 7. Failure and limitation handling

- If MR cannot be resolved/read (`glab` auth/network/not found), report exact failing command and error.
- Under investigation/planning flows, continue only if task remains meaningful without MR data; otherwise mark as blocked with concrete unblock requirements.
- Typical unblock actions:
  1. Fix GitLab auth for `glab`.
  2. Provide explicit MR id/url.
  3. Confirm repository/remote visibility for current environment.
