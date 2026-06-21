# Generic Project Verification Profile

This profile is mandatory for every target workspace, regardless of language or build system.
Specific profiles (`rust.md`, `cpp.md`, and future profiles) extend it without overriding it.

## Stack And Command Discovery

- First identify the project stack from changed files, manifest/build files, local docs,
  Makefile/task runner files, and CI config.
- Before running verification commands, find repo-approved commands in local sources:
  README, CONTRIBUTING, docs, Makefile, CI config, package/build manifests, scripts, and comments near
  the affected code.
- If a repo-approved command exists, prefer it over a local guess.
- If a command is ambiguous, expensive, requires missing environment, or may mutate state,
  do not invent a replacement. Record a skipped check and the reason.

## Scope Strategy

- Prefer scoped checks before broad checks:
  1. New/changed tests and the smallest relevant target/module scope.
  2. Relevant package/crate/component/library/service scope.
  3. Workspace/repository-level checks when the change is cross-component or local scope is unclear.
- Do not treat a local scoped check as sufficient when the change touches a shared contract,
  public API, schema/DB, generated files, serialization, build config, or shared dependency.
- If a broad check is too expensive or impossible, explicitly record the skipped check, why it was skipped,
  and what CI/manual signal should close the risk.

## Reporting

- Successful verification commands are evidence, but they do not replace critical review of the diff,
  changed files, call sites, contracts, artifacts, and risks.
- In the final report, explicitly list the applied profiles, discovered repo-approved commands,
  executed verification commands, and skipped checks. The exact report location is defined by
  role-specific prompt instructions.

## Command Safety

- Do not run commands that may mutate remote target systems, databases, queues, caches, production state,
  or system settings unless the remote policy allows it.
- Do not run destructive git operations without a direct user command.
- Do not delete build/cache directories for a "clean" check without necessity and a clear understanding of the cost.
- If a command may mutate state, prefer a documented dry-run/check mode or record it as skipped.
