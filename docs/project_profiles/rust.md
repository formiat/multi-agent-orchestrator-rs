# Rust Project Profile

This profile is mandatory when the workspace uses Rust: `Cargo.toml` or `Cargo.lock` exists, or
`.rs` files are affected.

## Stack Detection

- First find the relevant crate/workspace from `Cargo.toml`, `Cargo.lock`, `cargo metadata`,
  local docs, `Makefile`, CI config, and changed files.
- If the project is mixed-stack, apply this profile only to the Rust part and also read
  profiles for other affected stacks.
- If the repository has a documented/repo-approved command, prefer it over a raw cargo command.
- Before choosing a command, check the common `generic.md` profile: repo-approved commands and scoped checks
  take priority over local guesses.

## Executor Rules

- If you change git-tracked Rust files, run the mutating formatter before commit:
  `cargo fmt --all`.
- Do not replace `cargo fmt --all` with `cargo fmt --all --check`: the executor must format
  its own changes before commit.
- After formatting, run `make clippy` or a repo-approved equivalent.
- If no repo-approved equivalent exists, use a scoped `cargo clippy` command
  sufficient for the affected crate/workspace.
- A full `cargo build` is not required after all work if `make clippy` or
  `cargo clippy` already provides enough compile/lint coverage for the scope.
- If `Cargo.lock` changed, it must be included in the local commit.
- If you add/change tests, run the new/changed tests and the relevant
  module/crate-level scope.
- Placeholder tests do not count as valid tests.
- In outbox, list exact verification commands and explicitly list skipped checks/failures.

## Reviewer Rules

- The reviewer must not run mutating formatters/fixers: `cargo fmt`, `cargo fmt --all`,
  `cargo fix`, auto-fix linters, or similar commands.
- For format verification, use only the non-mutating command: `cargo fmt --all --check`.
- If formatting fails, return a finding/feedback; do not fix formatting yourself.
- Verification commands (`cargo check`, `cargo test`, `cargo fmt --all --check`, `clippy`, and
  similar commands) are only additional evidence. They do not replace reading, analyzing, and reviewing code.
- If `Cargo.lock` changed in the workspace, verify that it is included in the local commit;
  an uncommitted `Cargo.lock` blocks `decision=accept`.

## Tests

- For changed tests, run the smallest exact scope, then the relevant crate/module scope,
  when practical.

## Command Selection

- Preferred order:
  1. Repo-approved commands from `Makefile`, README/CONTRIBUTING, CI, or local docs.
  2. Scoped cargo commands for the affected crate/package.
  3. Workspace-level cargo commands if scope is unclear or the change is cross-crate.
- If a command is ambiguous or too expensive, do not invent a replacement: explicitly record the skipped check,
  the reason, and what command a human must confirm.
