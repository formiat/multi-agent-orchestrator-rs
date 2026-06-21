use crate::state::{ProviderKind, RemoteNetworkPolicy, TemplateId, TemplateValues, WorkflowType};

// ---------------------------------------------------------------------------
// Workflow contracts
//
// Artifact language policy:
// - User-facing workflow artifacts (PLAN.md, INVESTIGATION.md) and outbox
//   rationale are written in English by the executor/reviewer agents.
// - Code comments added or edited in project files are English only.
// - inbox.txt and outbox.txt must never be committed to git.
// ---------------------------------------------------------------------------

/// Human-readable description of the artifact contract for a given workflow.
pub fn workflow_contract_text(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Plan => {
            "Required: PLAN.md exists, non-empty, structurally valid; local commit when \
             git-tracked files in workspace changed; executor outbox exists and is non-empty. \
             SOFT_SUCCESS allowed when PLAN.md exists but outbox is missing after grace period."
        }
        WorkflowType::Investigate => {
            "Required: INVESTIGATION.md exists, non-empty, structurally valid; local commit when \
             git-tracked files in workspace changed; executor outbox exists and is non-empty. \
             SOFT_SUCCESS allowed when INVESTIGATION.md exists but outbox is missing after grace."
        }
        WorkflowType::Implement => {
            "Required: local commit for git-tracked workspace changes; clean worktree; executor \
             outbox exists and is non-empty; requested tests/markers validate when deterministic \
             command status is available. SOFT_SUCCESS routes to reviewer when outbox is missing \
             after grace; \
             the reviewer must verify commit presence, worktree cleanliness, tests, and \
             implementation quality."
        }
    }
}

/// Check whether SOFT_SUCCESS routes to EXECUTOR_OUTPUT_COLLECT (allowed) or to failure.
///
/// Soft-success routing table:
/// - plan: PLAN.md present → allowed (reviewer evaluates artifact; outbox absence noted).
///   PLAN.md missing → caller routes to FAILED_PROTOCOL (primary artifact absent).
/// - investigate: INVESTIGATION.md present → allowed; missing → FAILED_PROTOCOL.
/// - implement: always allowed here. Commit presence, worktree cleanliness, tests,
///   and implementation completeness are semantic review concerns for the reviewer,
///   not deterministic pre-conditions for the orchestrator.
/// - review round: no soft success; malformed/missing reviewer YAML → FAILED_PROTOCOL.
///
/// When allowed, the route carries `outbox_present=false` in the collected facts so
/// the reviewer knows outbox was absent.
pub fn soft_success_allowed(
    wf: WorkflowType,
    artifact_map: &std::collections::HashMap<String, bool>,
) -> bool {
    match wf {
        WorkflowType::Plan => artifact_map.get("PLAN.md").copied().unwrap_or(false),
        WorkflowType::Investigate => artifact_map
            .get("INVESTIGATION.md")
            .copied()
            .unwrap_or(false),
        WorkflowType::Implement => true,
    }
}

// ---------------------------------------------------------------------------
// Prompt templates
// ---------------------------------------------------------------------------

/// The hardcoded YAML schema literal embedded in reviewer prompts.
const REVIEWER_YAML_SCHEMA: &str = r#"quality_score: 0
decision: "revise"
rationale: |-
  English user-readable rationale.
contract_satisfied: false
hard_blockers_present: false
notion_requirements_satisfied: true
feedback_for_executor:
  - |-
    A concrete instruction or observation for the executor to evaluate.
checks_performed: "free_form_or_object_or_array"
findings: "free_form_or_object_or_array"
verification_commands: "free_form_or_object_or_array"
blocking_reason: null
irreconcilable_reason: null
poisoned_session_reason: null"#;

const GIT_HISTORY_REWRITE_RULE: &str = "- Do not use `git commit --amend`, `git rebase`, `git reset`, `git push --force`, or any other history-rewriting operation without a direct user command. If another fix is needed after a previous commit, create a new local commit.\n- Do not create commits while in detached HEAD mode.\n";

const REVIEWER_FORMAT_CHECK_RULE: &str = "- The reviewer must not run mutating formatters/fixers/auto-fixers. For formatting verification, use only non-mutating check commands from the relevant project profile or local repo docs. If formatting fails, return a finding/feedback; do not fix formatting yourself.\n";

const CLAUDE_SUBAGENT_RULE: &str = "- If you are Claude: delegate as many independent subtasks as practical to subagents (search, reading related files, checking hypotheses, reviewing code areas), while personally validating their outputs and assembling the final result.\n";

const ARTIFACT_REREAD_RULE: &str = "- Before using `PLAN.md` or `INVESTIGATION.md`, reread the corresponding file from `workspace_root` right now, even if you think you already know its contents from a previous round or session context.\n";

fn claude_subagent_rule(provider: ProviderKind) -> &'static str {
    if provider == ProviderKind::Claude {
        CLAUDE_SUBAGENT_RULE
    } else {
        ""
    }
}

fn opencode_outbox_write_rule(provider: ProviderKind, outbox_path: &str) -> String {
    if provider == ProviderKind::Opencode {
        format!(
            "- If you are OpenCode: first read `{outbox_path}` with the Read tool, then clear the file in-place via truncate, then write the result; do not delete `outbox.txt`.\n"
        )
    } else {
        String::new()
    }
}

fn opencode_reviewer_outbox_read_rule(provider: ProviderKind, outbox_path: &str) -> String {
    if provider == ProviderKind::Opencode {
        format!(
            "- If you are OpenCode: read `{outbox_path}` with the Read tool before any truncate/write.\n"
        )
    } else {
        String::new()
    }
}

fn notion_protocol_path() -> String {
    std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("docs/notion_access_protocol.md")
        .to_string_lossy()
        .into_owned()
}

fn gitlab_protocol_path() -> String {
    std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("docs/gitlab_access_protocol.md")
        .to_string_lossy()
        .into_owned()
}

fn project_profiles_common_section(v: &TemplateValues) -> String {
    let generic_profile_path = std::path::Path::new(&v.orchestrator_docs_dir)
        .join("project_profiles/generic.md")
        .to_string_lossy()
        .into_owned();
    let rust_profile_path = std::path::Path::new(&v.orchestrator_docs_dir)
        .join("project_profiles/rust.md")
        .to_string_lossy()
        .into_owned();
    let cpp_profile_path = std::path::Path::new(&v.orchestrator_docs_dir)
        .join("project_profiles/cpp.md")
        .to_string_lossy()
        .into_owned();

    format!(
        "Project-specific verification profiles:\n\
         - Always read and strictly follow the common profile `{generic_profile_path}`.\n\
         - Independently identify the main workspace stack from manifest/build files and changed files.\n\
         - If the project uses Rust (`Cargo.toml`, `Cargo.lock`, `.rs`), read and strictly follow `{rust_profile_path}`.\n\
         - If the project uses C/C++ (`CMakeLists.txt`, `CMakePresets.json`, `compile_commands.json`, `Makefile`, `.cpp/.hpp/.cc/.cxx/.h`), read and strictly follow `{cpp_profile_path}`.\n\
         - If the project is mixed-stack, apply all relevant profiles to the affected parts.\n\
         - If the stack is not covered by profiles, choose repo-approved commands from local docs/Makefile/CI and explicitly explain the choice in outbox/YAML.\n\
         - Do not invent verification commands: if a repo-approved command is ambiguous, explicitly record the skipped check and reason.\n\
         \n"
    )
}

fn executor_project_profiles_section(v: &TemplateValues) -> String {
    format!(
        "{common}\
         Profile reporting:\n\
         - In outbox, state which project profiles you read, why you selected them, which repo-approved commands you found, and which checks were skipped.\n\
         \n",
        common = project_profiles_common_section(v),
    )
}

fn reviewer_project_profiles_section(v: &TemplateValues) -> String {
    format!(
        "{common}\
         Profile review reporting:\n\
         - In `checks_performed`, state which project profiles you read and why you selected them.\n\
         - Verify that the executor stated and applied the relevant profiles and repo-approved commands in outbox.\n\
         - If the project is clearly Rust or C/C++ and the executor did not state reading/applying the relevant project profile, or its verification commands do not match the profile/repo docs, this is a finding.\n\
         \n",
        common = project_profiles_common_section(v),
    )
}

fn run_paths_section(v: &TemplateValues) -> String {
    format!(
        "Paths for this run:\n\
         - workspace_root: `{workspace_root}`\n\
         - transport_dir: `{transport_dir}`\n\
         - inbox_path: `{inbox_path}`\n\
         - outbox_path: `{outbox_path}`\n\
         - orchestrator_docs_dir: `{orchestrator_docs_dir}`\n\
         \n\
         All project files, `PLAN.md`, `INVESTIGATION.md`, `.agent-io/inbox.txt`, and `.agent-io/outbox.txt` belong only to `workspace_root`.\n\
         The orchestrator docs directory is only for reading instructions; do not use it as the workspace and do not look for `.agent-io` there.\n\
         Protocol files are in the orchestrator repository and are read only as instructions. They are not the task workspace. After reading protocols, perform all actions in `workspace_root`.\n\
         \n",
        workspace_root = v.workspace_root,
        transport_dir = v.transport_dir,
        inbox_path = v.inbox_path,
        outbox_path = v.outbox_path,
        orchestrator_docs_dir = v.orchestrator_docs_dir,
    )
}

fn plan_investigation_rule(wf: WorkflowType) -> &'static str {
    if wf == WorkflowType::Plan {
        "- If `INVESTIGATION.md` exists in workspace_root, you must read it and use it as planning input.\n"
    } else {
        ""
    }
}

fn implement_plan_rule(wf: WorkflowType) -> &'static str {
    if wf == WorkflowType::Implement {
        "- If `PLAN.md` exists in workspace_root, you must read it and use it as implementation input.\n"
    } else {
        ""
    }
}

fn existing_artifact_refine_rule(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Plan => {
            "- If `PLAN.md` already exists, read it first and refine it as the current artifact instead of ignoring it.\n"
        }
        WorkflowType::Investigate => {
            "- If `INVESTIGATION.md` already exists, read it first and refine/correct it as the current artifact instead of ignoring it.\n"
        }
        WorkflowType::Implement => "",
    }
}

fn artifact_structure_name(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Plan => "`PLAN.md`",
        WorkflowType::Investigate => "`INVESTIGATION.md`",
        WorkflowType::Implement => "",
    }
}

fn artifact_structure_items(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Plan => {
            "- Context\n\
             - Investigation context (if `INVESTIGATION.md` exists)\n\
             - Detected stack/profiles\n\
             - Repo-approved commands found\n\
             - Affected components\n\
             - Implementation steps (numbered, with file paths; each item must have a concrete materializable result)\n\
             - Verification plan\n\
             - Testing strategy (categories 1/2/3: no refactoring / light / heavy)\n\
             - Risks and tradeoffs\n\
             - Open questions\n"
        }
        WorkflowType::Investigate => {
            "- Context/Task\n\
             - Research questions\n\
             - Scope and constraints\n\
             - Detected stack/profiles\n\
             - Repo-approved commands found\n\
             - Observed symptom\n\
             - Immediate cause\n\
             - Causal chain / why chain\n\
             - Evidence per causal link\n\
             - Root cause / unresolved boundary\n\
             - Sources checked\n\
             - Evidence\n\
             - Evidence references (file:line refs for code, commit hashes for git history, command/log excerpts for logs/CLI)\n\
             - Findings\n\
             - Relevant code paths\n\
             - Timeline/history (if applicable)\n\
             - Hypotheses/alternatives (if applicable)\n\
             - Risk/impact\n\
             - Conclusions\n\
             - Recommendations/next steps\n\
             - Verification/falsification steps for findings\n\
             - Follow-up verification implications (if applicable; do not format this as an automated test plan)\n\
             - Open questions\n"
        }
        WorkflowType::Implement => "",
    }
}

fn reviewer_artifact_compliance_requirements(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Plan => {
            "- For workflow `plan` (track A), verify the required `PLAN.md` structure:\n\
             - Verify that all required sections exist, are substantive (not placeholders), and are coherent: Affected components <-> Implementation steps <-> Testing strategy.\n\
             - Verify that `Implementation steps` are concrete materializable actions with expected outcomes: change a file/module/function, add/update a test, add a migration/schema/DTO/contract, or run a specific check.\n\
             - Verify that `PLAN.md` contains `Detected stack/profiles`, `Repo-approved commands found`, and `Verification plan`, and that selected checks match the relevant project profiles.\n\
             - Verify that each implementation step names concrete files and code anchors: functions/types/endpoints/modules/test names; if relevant lines are known after investigation, file:line refs must be present.\n\
             - For non-trivial logic, API/DTO/DB contracts, algorithms, and ambiguous areas, verify short code snippets or pseudocode; snippets must capture intent and contract, not replace the full implementation.\n\
             - Standalone implementation steps such as `assess`, `study`, `investigate`, `figure out`, `look into`, `find out`, or `check whether possible` are findings: they defer discovery instead of planning implementation.\n\
             - `decision=accept` is forbidden if a substantial part of `Implementation steps` is non-materializable discovery or leaves solution choice to implementation without an explicit blocker/open question.\n\
             - `Testing strategy` must plan automated testing (unit/integration/e2e where applicable), not manual checks.\n\
             - Manual checks are allowed only as fallback and only with an explicit explanation of why an automated test is objectively impossible in the current context (explicit gap + reason).\n"
        }
        WorkflowType::Investigate => {
            "- For workflow `investigate` (track A), verify the required `INVESTIGATION.md` structure:\n\
             - Verify that all required sections exist, are substantive (not placeholders), and are coherent: Research questions <-> Evidence <-> Findings <-> Conclusions.\n\
             - Verify that `INVESTIGATION.md` contains `Detected stack/profiles`, `Repo-approved commands found`, and steps to verify/falsify findings when applicable to the task.\n\
             - Do not require an automated test plan from investigation; if tests are mentioned, they must be only a brief follow-up implication, not an implementation checklist.\n\
             - Verify that the main conclusion does not stop at a top-level/immediate cause: each primary hypothesis must have a causal chain like observed symptom -> immediate technical cause -> why that happened -> why that condition was possible -> actionable root cause or explicit unresolved boundary.\n\
             - Verify that each causal-chain link is backed by evidence; if the chain stops, it must be recorded as an unresolved boundary with specific missing data/commands.\n\
             - Verify that each key claim/conclusion is backed by a verifiable evidence reference: file:line refs for code, commit hashes for git history, command/log excerpts for logs/CLI.\n\
             - `decision=accept` is forbidden if key conclusions rely on paraphrase without verifiable links to primary sources.\n\
             - `decision=accept` is forbidden if investigation explains only \"what happened\" or the immediate cause, but not \"why it became possible\", when available sources allow deeper analysis.\n\
             - Each research question from the user prompt must be closed by a conclusion or explicitly marked unresolved with a verifiable reason.\n"
        }
        WorkflowType::Implement => "",
    }
}

fn reviewer_investigate_independent_validation_rule(wf: WorkflowType) -> &'static str {
    if wf == WorkflowType::Investigate {
        "- For workflow `investigate`, independently verify every key claim/conclusion and every key piece of evidence from `INVESTIGATION.md` against the primary source (code, git history, logs, commands), not only the artifact text.\n\
         - For code evidence, require file:line refs; for git history, commit hashes; for logs/CLI, concrete command/log excerpts.\n\
         - For each primary hypothesis, independently verify the causal chain: do not accept a conclusion that proves only the top-level cause without cause-of-cause when available sources allow deeper analysis.\n\
         - Do not confirm a conclusion without verifiable evidence; if verification failed, explicitly mark this as a gap/uncertainty in findings.\n"
    } else {
        ""
    }
}

fn reviewer_workflow_coverage_rule(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Investigate => {
            "- For workflow `investigate`: independently formulate research questions from the user prompt and compare them with `INVESTIGATION.md`.\n\
             - Check beyond what the executor mentioned: assess whether important questions, code paths, sources, alternative explanations, risks, or limitations were missed.\n"
        }
        WorkflowType::Plan => {
            "- For workflow `plan`: independently verify that `PLAN.md` covers the user prompt and `INVESTIGATION.md` (if present), including affected components, implementation steps, risks/tradeoffs, and automated tests.\n\
             - Check beyond what the executor mentioned: assess whether important components, edge cases, migrations/data, integrations, risks, or test scenarios were missed.\n"
        }
        WorkflowType::Implement => {
            "- For workflow `implement`: besides every changed file, check relevant neighboring call sites, contracts, integrations, and invariants that may have broken because of the changes.\n\
             - Check beyond what the executor mentioned: assess whether related files, edge cases, tests, migrations/data, runtime paths, or backward compatibility were missed.\n"
        }
    }
}

fn executor_breakage_block_rule(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Plan | WorkflowType::Implement => {
            "- A `What Could Break` block is required: list potential regressions/risks after changes (behavior, API/contracts, data/DB, integrations, performance/resources) and how to verify them.\n"
        }
        WorkflowType::Investigate => "",
    }
}

fn executor_implement_verification_scope_rule(wf: WorkflowType) -> &'static str {
    if wf == WorkflowType::Implement {
        "- A full build is not required after all work if the relevant project profile or repo-approved checks already provide sufficient compile/lint verification for your scope; explicitly state the selected check in outbox.\n"
    } else {
        ""
    }
}

fn executor_plan_test_coverage_rule(wf: WorkflowType) -> &'static str {
    if wf == WorkflowType::Plan {
        "- For workflow `plan`, do not defer discovery/assessment to implementation: before writing `PLAN.md`, read the relevant files, call sites, schemas, tests, migrations, configs, and existing patterns yourself.\n\
         - `Implementation steps` must be concrete and materializable: change a file/module/function, add/update a test, add a migration/schema/DTO/contract, or run a specific check. Each item must have a clear expected outcome.\n\
         - For each implementation step, name concrete files and code anchors: functions/types/endpoints/modules/test names. If relevant lines are known after investigation, add file:line refs.\n\
         - Add short code snippets or pseudocode for non-trivial logic, API/DTO/DB contracts, algorithms, and places where an example prevents ambiguous implementation.\n\
         - Do not turn `PLAN.md` into a full implementation: snippets must capture intent and contract, not replace code.\n\
         - Do not use vague standalone implementation steps such as `assess`, `study`, `investigate`, `figure out`, `look into`, `find out`, or `check whether possible`.\n\
         - If a solution cannot be chosen without additional information, do not hide that in `Implementation steps`: either perform the check now during planning, or put it in `Open questions` / `Blockers` with the exact question, blocking reason, and required unblocking input.\n\
         - Workflow `plan` must plan automated tests (unit/integration/e2e where applicable), not manual checks.\n\
         - Manual checks are allowed only as fallback when an automated test is objectively impossible in the current context.\n\
         - For the automated test plan: cover happy-path + negative-path + edge-case as far as reasonable; if something is not covered by automated tests, explicitly record the gap and reason.\n"
    } else {
        ""
    }
}

fn executor_investigation_causal_chain_rule(wf: WorkflowType) -> &'static str {
    if wf == WorkflowType::Investigate {
        "- For workflow `investigate`, do not stop at the top-level cause / immediate cause. For the main hypothesis, build a causal chain: observed symptom -> immediate technical cause -> why that happened -> why that condition was possible -> actionable root cause or explicit unresolved boundary.\n\
         - For each causal-chain link, provide evidence: file:line, log excerpt, command output, data example, or commit reference.\n\
         - If deeper analysis requires unavailable data, do not guess: explicitly mark an unresolved boundary and state which concrete data/commands are needed.\n\
         - Do not use `main hypothesis` as the final conclusion if it explains only the top level. The final conclusion must answer `why this became possible`.\n\
         - For each primary hypothesis, state the immediate cause, cause-of-cause, deeper/root cause, evidence per link, confidence, and what would falsify this hypothesis.\n\
         - Do not require exactly five causality levels and do not force an artificial chain: go as deep as the actionable root cause or a proven unresolved boundary.\n"
    } else {
        ""
    }
}

fn remote_network_policy_section(policy: RemoteNetworkPolicy) -> &'static str {
    match policy {
        RemoteNetworkPolicy::Forbidden => {
            "Remote access policy:\n\
             - Do not use SSH to access remote target systems.\n\
             - Do not make HTTP requests to remote target systems.\n\
             - Investigate only local code, local artifacts, and explicitly allowed local CLI tools.\n\
             \n"
        }
        RemoteNetworkPolicy::ReadOnly => {
            "Remote access policy:\n\
             - Only read-only access to remote target systems is allowed for investigation.\n\
             - SSH is allowed only for reading: viewing logs, configs, statuses, metrics, versions, time, environment, and other diagnostic information.\n\
             - HTTP is allowed only for read-only requests: GET/HEAD/OPTIONS and necessary auth requests to obtain read-only access.\n\
             - Any changes to files, configs, processes, services, DBs, queues, caches, and runtime state are forbidden.\n\
             - POST/PUT/PATCH/DELETE and any HTTP requests with side effects are forbidden, except explicitly necessary auth requests.\n\
             - Service stop/restart/reload and any commands that change system state are forbidden.\n\
             - Do not unpack log archives on a remote system; if compressed logs must be read, use streaming read/grep without creating files.\n\
             - If a command may mutate state or is ambiguous, do not run it; first record the concern in outbox.\n\
             - If you performed remote SSH/HTTP actions, list them in outbox with classification: read-only or mutating.\n\
             \n"
        }
        RemoteNetworkPolicy::Operational => {
            "Remote access policy:\n\
             - Operational actions are allowed on the remote target system explicitly named by the user.\n\
             - Application config changes, read/write HTTP requests, enabling/disabling diagnostic settings, and application stop/restart/reload are allowed when needed for the task.\n\
             - Reading DBs and running read-only SQL queries is allowed.\n\
             - DB mutations are forbidden: INSERT/UPDATE/DELETE/TRUNCATE/ALTER/DROP/CREATE and any SQL/CLI actions with side effects.\n\
             - Do not unpack log archives on a remote system; if compressed logs must be read, use streaming read/grep without creating files.\n\
             - Use the minimum necessary changes: before any mutating action, understand the goal, expected effect, and how to roll back/verify the result.\n\
             - Non-work, non-operational, and system-destructive actions are forbidden: installing/removing OS packages and tools, changing OS settings, users, firewall/network/systemd outside the application, clearing data without an explicit command, and destructive shell/git operations.\n\
             - Actions outside the user-specified target system are forbidden.\n\
             - List all mutating actions in outbox: command/request, goal, time, result, rollback, or why rollback is unnecessary.\n\
             \n"
        }
    }
}

/// Render the executor initial prompt template.
fn render_executor_initial(v: &TemplateValues) -> String {
    let notion_protocol_path = notion_protocol_path();
    let gitlab_protocol_path = gitlab_protocol_path();
    let run_paths_section = run_paths_section(v);
    let project_profiles_section = executor_project_profiles_section(v);
    let plan_investigation_rule = plan_investigation_rule(v.workflow_type);
    let implement_plan_rule = implement_plan_rule(v.workflow_type);
    let existing_artifact_refine_rule = existing_artifact_refine_rule(v.workflow_type);
    let executor_breakage_block_rule = executor_breakage_block_rule(v.workflow_type);
    let executor_implement_verification_scope_rule =
        executor_implement_verification_scope_rule(v.workflow_type);
    let executor_plan_test_coverage_rule = executor_plan_test_coverage_rule(v.workflow_type);
    let executor_investigation_causal_chain_rule =
        executor_investigation_causal_chain_rule(v.workflow_type);
    let artifact_structure_name = artifact_structure_name(v.workflow_type);
    let artifact_structure_items = artifact_structure_items(v.workflow_type);
    let remote_network_policy_section = remote_network_policy_section(v.remote_network_policy);
    let claude_subagent_rule = claude_subagent_rule(v.provider);
    let opencode_outbox_write_rule = opencode_outbox_write_rule(v.provider, &v.outbox_path);
    let artifact_structure_section = if artifact_structure_name.is_empty() {
        String::new()
    } else {
        format!(
            "Artifact structure:\n\
             - Required structure for {artifact_structure_name}:\n\
             {artifact_structure_items}\n\
             "
        )
    };
    format!(
        "You are the executor agent in workflow `{wf}`.\n\
         \n\
         Run context:\n\
         - workspace_root: `{repo}`\n\
         - branch: `{branch}`\n\
         - notion_policy: `{notion_policy}`\n\
         \n\
         {run_paths_section}\
         User prompt:\n\
         {prompt}\n\
         \n\
         Workflow contract:\n\
         {contract}\n\
         \n\
         {artifact_structure_section}\
         {remote_network_policy_section}\
         Mandatory rules:\n\
         - Read and follow `{notion_protocol_path}` as the mandatory Notion protocol.\n\
         - Read and follow `{gitlab_protocol_path}` as the mandatory GitLab protocol.\n\
         {claude_subagent_rule}\
         \n\
         {project_profiles_section}\
         Executor working rules:\n\
         \n\
         1. Inputs and artifacts\n\
         - Read inbox by absolute path: `{inbox_path}`.\n\
         {artifact_reread_rule}\
         {existing_artifact_refine_rule}\
         {plan_investigation_rule}\
         {implement_plan_rule}\
         \n\
         2. Transport and outbox\n\
         - Before the final response, clear outbox at absolute path `{outbox_path}` in-place via truncate and write the current round result there.\n\
         {opencode_outbox_write_rule}\
         - Do not use `.agent-io` from other directories.\n\
         - Before writing outbox, verify that `{outbox_path}` starts with `workspace_root` (`{repo}`). If outbox path is not inside `workspace_root`, stop and write an error to the correct `{outbox_path}`.\n\
         - Do not commit transport files: `{inbox_path}` and `{outbox_path}`.\n\
         \n\
         3. Git and workspace safety\n\
         - Do not push commits without a direct user command.\n\
         {git_history_rewrite_rule}\
         - If you change git-tracked files in workspace_root (including `PLAN.md`/`INVESTIGATION.md`), create a local commit.\n\
         - Do not run destructive git operations without a direct user command.\n\
         \n\
         4. Language and formatting\n\
         - Write user-facing artifacts in English (`PLAN.md`, `INVESTIGATION.md`, executor outbox, reviewer YAML: `rationale`, `feedback_for_executor`, `checks_performed`, `findings`, `verification_commands`, `blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`).\n\
         - Add/edit project code comments only in English.\n\
         - In outbox, include commit hash and executed verification commands.\n\
         \n\
         5. Checks and unfinished work\n\
         - If you add/change tests, run the new/changed tests and the relevant module/crate-level scope.\n\
         - Placeholder tests do not count as valid tests.\n\
         - Report any unfinished work, skipped checks, or failures in outbox.\n\
         {executor_breakage_block_rule}\
         {executor_implement_verification_scope_rule}\
         {executor_plan_test_coverage_rule}\
         {executor_investigation_causal_chain_rule}\
         \n\
         Outbox format:\n\
         - For this role, outbox is free-form.\n\
         - Write with enough detail for the reviewer to verify the result without guesses.\n\
         \n\
         When finished, write the result to outbox at absolute path `{outbox_path}` and stop.",
        wf = v.workflow_type,
        repo = v.workspace_root,
        branch = v.branch,
        notion_policy = v.notion_policy,
        run_paths_section = run_paths_section,
        inbox_path = v.inbox_path,
        outbox_path = v.outbox_path,
        notion_protocol_path = notion_protocol_path,
        gitlab_protocol_path = gitlab_protocol_path,
        claude_subagent_rule = claude_subagent_rule,
        project_profiles_section = project_profiles_section,
        opencode_outbox_write_rule = opencode_outbox_write_rule,
        git_history_rewrite_rule = GIT_HISTORY_REWRITE_RULE,
        artifact_reread_rule = ARTIFACT_REREAD_RULE,
        remote_network_policy_section = remote_network_policy_section,
        artifact_structure_section = artifact_structure_section,
        existing_artifact_refine_rule = existing_artifact_refine_rule,
        plan_investigation_rule = plan_investigation_rule,
        implement_plan_rule = implement_plan_rule,
        executor_breakage_block_rule = executor_breakage_block_rule,
        executor_implement_verification_scope_rule = executor_implement_verification_scope_rule,
        executor_plan_test_coverage_rule = executor_plan_test_coverage_rule,
        executor_investigation_causal_chain_rule = executor_investigation_causal_chain_rule,
        prompt = v.user_prompt,
        contract = v.workflow_contract,
    )
}

/// Render the reviewer review prompt template.
fn render_reviewer_review(v: &TemplateValues) -> String {
    let notion_protocol_path = notion_protocol_path();
    let gitlab_protocol_path = gitlab_protocol_path();
    let run_paths_section = run_paths_section(v);
    let project_profiles_section = reviewer_project_profiles_section(v);
    let schema = v
        .reviewer_yaml_schema
        .as_deref()
        .unwrap_or(REVIEWER_YAML_SCHEMA);
    let artifact_structure_name = artifact_structure_name(v.workflow_type);
    let artifact_structure_items = artifact_structure_items(v.workflow_type);
    let artifact_structure_section = if artifact_structure_name.is_empty() {
        String::new()
    } else {
        format!(
            "Artifact structure (for review):\n\
             - Required structure for {artifact_structure_name}:\n\
             {artifact_structure_items}\n\
             "
        )
    };
    let reviewer_artifact_compliance_requirements =
        reviewer_artifact_compliance_requirements(v.workflow_type);
    let reviewer_investigate_independent_validation_rule =
        reviewer_investigate_independent_validation_rule(v.workflow_type);
    let reviewer_workflow_coverage_rule = reviewer_workflow_coverage_rule(v.workflow_type);
    let remote_network_policy_section = remote_network_policy_section(v.remote_network_policy);
    let claude_subagent_rule = claude_subagent_rule(v.provider);
    let opencode_reviewer_outbox_read_rule =
        opencode_reviewer_outbox_read_rule(v.provider, &v.outbox_path);

    let outbox_section = if v.executor_outbox_present {
        format!(
            "Executor result:\n\
             - The executor completed the round and wrote a free-form text report to `{outbox_path}`.\n\
             - Read `{outbox_path}` yourself before review.\n\
             {opencode_reviewer_outbox_read_rule}\
             - Do not expect a strict format in executor outbox.\n\
             - After reading executor outbox and completing review, clear `{outbox_path}` in-place and write only your YAML there.",
            outbox_path = v.outbox_path,
            opencode_reviewer_outbox_read_rule = opencode_reviewer_outbox_read_rule,
        )
    } else {
        format!(
            "Executor result:\n\
             - The executor completed the round in soft-success mode: outbox.txt is missing or empty.\n\
             - Do not try to read outbox.txt as the executor report source; it has no data.\n\
             - Review the result only from git facts and required artifact presence in the workspace.\n\
             - Write your YAML directly to `{outbox_path}` (clear in-place via truncate, then write).",
            outbox_path = v.outbox_path,
        )
    };

    format!(
        "You are the reviewer agent in workflow `{wf}`.\n\
         \n\
         Run context:\n\
         - workspace_root: `{repo}`\n\
         - branch: `{branch}`\n\
         - notion_policy: `{notion_policy}`\n\
         \n\
         {run_paths_section}\
         User prompt:\n\
         {prompt}\n\
         \n\
         Workflow contract:\n\
         {contract}\n\
         \n\
         {artifact_structure_section}\
         Mandatory protocols:\n\
         - Read and follow `{notion_protocol_path}` as the mandatory Notion protocol.\n\
         - Read and follow `{gitlab_protocol_path}` as the mandatory GitLab protocol.\n\
         {claude_subagent_rule}\
         \n\
         {project_profiles_section}\
         \n\
         Executor data:\n\
         {outbox_section}\n\
         \n\
         Git facts:\n\
         {git_facts}\n\
         \n\
         {remote_network_policy_section}\
         CRITICAL: review purpose\n\
         - Your main task is not to confirm that commands pass, but to perform a full critical review of the artifact and/or code.\n\
         - Independently verify conclusion correctness, scope completeness, alignment with user prompt/PLAN.md/INVESTIGATION.md, all changed files, related code paths, and possible regressions.\n\
         - Verification commands from project profiles or repo docs are only additional evidence. They do not replace reading, analysis, and code/artifact review.\n\
         - `decision=accept` is forbidden if you did not independently verify key claims, changed files, and material risks.\n\
         \n\
         What to check:\n\
         \n\
         1. Scope reconstruction\n\
         - Independently verify the executor result.\n\
         - Do not trust the executor self-report without verification.\n\
         - Do not limit review to what the executor mentioned: independently reconstruct expected scope from the user prompt, workflow contract, and mandatory input artifacts.\n\
         - If the user prompt mentions a Notion task, separately state in review which Notion commands/evidence you independently verified (not only from executor claims).\n\
         \n\
         2. Artifacts and claims review\n\
         - Artifact review must not be done from memory or from the executor summary: before evaluating, reread `PLAN.md` / `INVESTIGATION.md` from `workspace_root` right now, even if you think you already know their contents.\n\
         - Check whether important questions, code paths, files, sources, edge cases, risks, tests, or alternative explanations/solutions were missed.\n\
         - If the executor did not mention an important area it should have checked from prompt/context, this is a finding; `decision=accept` is forbidden if the omission is material.\n\
         \n\
         3. Code and diff review\n\
         - You must check code and commit changes (diff/files/content), not only outbox and test runs.\n\
         - You must check every changed file from the executor commit(s); selective file review is not allowed.\n\
         \n\
         4. Risks and compatibility\n\
         - Separately check compatibility and regression risks: API/contracts, behavior, data/DB, serialization/deserialization, integration assumptions.\n\
         - A `What Could Break` block is required: list potential regressions and their impact; if no risks were found, explicitly state what was checked and why risk is low.\n\
         - Separately state what was done, not done, partially/controversially done, and changed unnecessarily outside scope.\n\
         \n\
         5. Finding requirements\n\
         - Every material finding must be evidence-backed and actionable: where (file/line), what is wrong, why it is a risk, and the minimal fix.\n\
         - If there are no findings, explicitly state `no findings` and briefly list what was checked.\n\
         - Vague wording (for example, `seems OK`, `looks fine`, `appears correct`) is forbidden without verifiable support.\n\
         \n\
         Workflow-specific checks:\n\
         {reviewer_workflow_coverage_rule}\
         - Perform review in two independent tracks:\n\
         - Track A (compliance): for workflow `implement`, if `PLAN.md` is present, checking implementation against PLAN.md items is mandatory; if `PLAN.md` is absent, checking implementation against the user prompt and workflow contract is mandatory.\n\
         - Track B (independent code review): separately from PLAN.md/user prompt, check quality and correctness of the code changes themselves: logic, regressions, architectural risks, unnecessary changes outside scope.\n\
         {reviewer_investigate_independent_validation_rule}\
         {reviewer_artifact_compliance_requirements}\
         \n\
         Minimum checklist before accept:\n\
         - all changed files were checked;\n\
         - related call sites / code paths were checked;\n\
         - key executor claims were checked;\n\
         - omitted scope / risks / edge cases were checked;\n\
         - tests are not the only basis for accept;\n\
         - no high/critical findings exist.\n\
         \n\
         Reviewer constraints and policies:\n\
         \n\
         1. Workspace and Git\n\
         {reviewer_format_check_rule}\
         - Do not change git-tracked workspace_root files, workflow artifacts, git index, commits, branches, or provider metadata.\n\
         {git_history_rewrite_rule}\
         \n\
         2. Transport and outbox\n\
         - The only file you must change is `{outbox_path}`, where you write the final YAML.\n\
         - Do not use `.agent-io` from other directories.\n\
         - Before writing outbox, verify that `{outbox_path}` starts with `workspace_root` (`{repo}`). If outbox path is not inside `workspace_root`, stop and write an error to the correct `{outbox_path}`.\n\
         - If executor outbox clearly shows the executor is responding to the wrong repo/request/thread or using the wrong transport path, return `decision: poisoned_session` and fill `poisoned_session_reason`.\n\
         \n\
         3. Language and formatting\n\
         - Write user-facing artifacts in English (`PLAN.md`, `INVESTIGATION.md`, executor outbox, reviewer YAML: `rationale`, `feedback_for_executor`, `checks_performed`, `findings`, `verification_commands`, `blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`).\n\
         \n\
         4. Contract, git facts, and artifacts\n\
         - Passing tests are necessary, but not sufficient for `decision=accept` without confirmed correctness of code changes.\n\
         - Verify workflow contract compliance.\n\
         - Verify git facts and required artifact presence in the workspace.\n\
         \n\
         5. Remote access violations\n\
         - Check remote access policy compliance. If the executor performed a forbidden remote action, return `decision: blocked`, `hard_blockers_present: true`, and fill `blocking_reason` with the violation description; do not return `revise`.\n\
         - For remote access policy violations, include in `findings`: what action was performed, why it is forbidden by the current policy, and what traces/risks a human should check.\n\
         - If a remote access policy violation occurred, do not ask the executor to continue investigating or perform compensating actions without a direct new user command.\n\
         \n\
         Decision policy:\n\
         - First independently assign quality_score from 0 to 10 and set contract_satisfied/hard_blockers_present from facts.\n\
         - Set `contract_satisfied=true` only if mandatory workflow contract conditions are met by verifiable facts (artifact/git/notion policy).\n\
         - Set `hard_blockers_present=true` if there is at least one external/critical blocker that prevents successful completion of the round.\n\
         - `decision=accept` is forbidden if mandatory requirements are unmet, key claims are unverified, or there is any serious finding (`major|high|serious|critical`).\n\
         - If there is at least one serious/critical issue (`major|high|serious|critical`), `hard_blockers_present` must be `true`.\n\
         - Then choose decision based on the already assigned score and facts.\n\
         - Return decision: `accept`, `revise`, `blocked`, `irreconcilable_disagreement`, or `poisoned_session`.\n\
         - The orchestrator applies thresholds and gate rules automatically; do not tune score to the desired decision.\n\
         - If fixes are needed, fill `feedback_for_executor` with concrete items.\n\
         \n\
         Output format:\n\
         \n\
         Strict outbox format (`{outbox_path}`):\n\
         - Write exactly one YAML document to `{outbox_path}`.\n\
         - Do not add Markdown fences.\n\
         - Do not add text before or after YAML.\n\
         - Do not add YAML comments: the parser ignores comments, they are not part of the protocol, and they will not appear in the orchestrator report.\n\
         - Do not add anchors, aliases, custom tags, or a second YAML document.\n\
         - Mandatory strict fields: `decision`, `quality_score`, `rationale`, `contract_satisfied`, `hard_blockers_present`, `notion_requirements_satisfied`, `feedback_for_executor`, `blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`.\n\
         - Fields `checks_performed`, `findings`, `verification_commands` are free-structure.\n\
         - For decision/status/category/name values, use Latin snake_case.\n\
         - Use this minimal schema:\n\
         \n\
         {schema}\n\
         \n\
         When finished, write YAML to `{outbox_path}` and stop.",
        wf = v.workflow_type,
        repo = v.workspace_root,
        branch = v.branch,
        notion_policy = v.notion_policy,
        run_paths_section = run_paths_section,
        outbox_path = v.outbox_path,
        notion_protocol_path = notion_protocol_path,
        gitlab_protocol_path = gitlab_protocol_path,
        claude_subagent_rule = claude_subagent_rule,
        project_profiles_section = project_profiles_section,
        prompt = v.user_prompt,
        contract = v.workflow_contract,
        outbox_section = outbox_section,
        artifact_structure_section = artifact_structure_section,
        git_facts = v.git_facts,
        schema = schema,
        reviewer_artifact_compliance_requirements = reviewer_artifact_compliance_requirements,
        reviewer_investigate_independent_validation_rule =
            reviewer_investigate_independent_validation_rule,
        reviewer_workflow_coverage_rule = reviewer_workflow_coverage_rule,
        remote_network_policy_section = remote_network_policy_section,
        reviewer_format_check_rule = REVIEWER_FORMAT_CHECK_RULE,
        git_history_rewrite_rule = GIT_HISTORY_REWRITE_RULE,
    )
}

/// Render reviewer repair prompt after a rejected YAML attempt.
fn render_reviewer_repair_yaml(v: &TemplateValues) -> String {
    let notion_protocol_path = notion_protocol_path();
    let gitlab_protocol_path = gitlab_protocol_path();
    let run_paths_section = run_paths_section(v);
    let schema = v
        .reviewer_yaml_schema
        .as_deref()
        .unwrap_or(REVIEWER_YAML_SCHEMA);
    let rejection = v
        .reviewer_yaml_rejection
        .as_deref()
        .unwrap_or("(missing rejection reason)");

    format!(
        "You are the reviewer agent. The previous YAML was rejected by the orchestrator parser.\n\
         \n\
         {run_paths_section}\
         Task:\n\
         - Your only task now: fix only the YAML protocol and overwrite `{outbox_path}`.\n\
         - Do not reconsider review content unless required to fix the schema.\n\
         {claude_subagent_rule}\
         \n\
         Transport and constraints:\n\
         - Do not change the Notion protocol: it is defined in `{notion_protocol_path}`.\n\
         - Do not change the GitLab protocol: it is defined in `{gitlab_protocol_path}`.\n\
         - Do not use `.agent-io` from other directories.\n\
         - Before writing outbox, verify that `{outbox_path}` starts with `workspace_root` (`{repo}`). If outbox path is not inside `workspace_root`, stop and write an error to the correct `{outbox_path}`.\n\
         - Write user-facing artifacts in English (`PLAN.md`, `INVESTIGATION.md`, executor outbox, reviewer YAML: `rationale`, `feedback_for_executor`, `checks_performed`, `findings`, `verification_commands`, `blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`).\n\
         \n\
         YAML_REJECTION:\n\
         - previous_yaml_rejected: true\n\
         - rejection_reason: {rejection}\n\
         - keep_semantic_decision_if_still_valid: true\n\
         \n\
         STRICT_FIELDS_CHECKLIST:\n\
         - decision: accept|revise|blocked|irreconcilable_disagreement|poisoned_session\n\
         - quality_score: number from 0 to 10\n\
         - contract_satisfied: true|false\n\
         - hard_blockers_present: true|false\n\
         - notion_requirements_satisfied: true|false\n\
         - decision=accept: contract_satisfied=true and hard_blockers_present=false\n\
         - decision=revise requires non-empty feedback_for_executor\n\
         - decision=blocked requires non-empty blocking_reason\n\
         - decision=irreconcilable_disagreement requires non-empty irreconcilable_reason\n\
         - decision=poisoned_session requires non-empty poisoned_session_reason\n\
         - exactly one YAML document, no markdown fences, no text before/after YAML\n\
         \n\
         notion_policy for the current run: `{notion_policy}`\n\
         \n\
         Schema:\n\
         Use this minimal schema:\n\
         \n\
         {schema}\n\
         \n\
         Stop immediately after fixing it.",
        rejection = rejection,
        notion_policy = v.notion_policy,
        run_paths_section = run_paths_section,
        outbox_path = v.outbox_path,
        repo = v.workspace_root,
        notion_protocol_path = notion_protocol_path,
        gitlab_protocol_path = gitlab_protocol_path,
        claude_subagent_rule = claude_subagent_rule(v.provider),
        schema = schema,
    )
}

/// Render reviewer cleanup prompt after the reviewer leaves dirty workspace entries.
fn render_reviewer_repair_workspace(v: &TemplateValues) -> String {
    let notion_protocol_path = notion_protocol_path();
    let gitlab_protocol_path = gitlab_protocol_path();
    let run_paths_section = run_paths_section(v);
    let rejection = v
        .reviewer_workspace_rejection
        .as_deref()
        .unwrap_or("(missing workspace cleanup reason)");

    format!(
        "You are the reviewer agent. After the previous review attempt, the worktree remained dirty.\n\
         \n\
         {run_paths_section}\
         Cleanup task:\n\
         - Your only task now: clean workspace_root from changes you left behind and stop.\n\
         - Do not reconsider the review and do not rewrite the reviewer YAML verdict: the orchestrator already saved the valid YAML from the previous attempt.\n\
         {claude_subagent_rule}\
         \n\
         Allowed:\n\
         - Allowed cleanup methods: commit the leftover changes locally or revert/delete them. Choose based on the facts.\n\
         \n\
         Forbidden:\n\
         - Do not change `{outbox_path}` unless cleanup requires it. If you accidentally changed `{outbox_path}`, restore the previous reviewer YAML verdict without semantic changes.\n\
         - Do not push commits. Do not commit transport files: `{inbox_path}` and `{outbox_path}`.\n\
         {git_history_rewrite_rule}\
         \n\
         Transport and protocols:\n\
         - Do not use `.agent-io` from other directories.\n\
         - If you still need to touch outbox, first verify that `{outbox_path}` starts with `workspace_root` (`{repo}`). If outbox path is not inside `workspace_root`, stop and write an error to the correct `{outbox_path}`.\n\
         - Do not change the Notion protocol: it is defined in `{notion_protocol_path}`.\n\
         - Do not change the GitLab protocol: it is defined in `{gitlab_protocol_path}`.\n\
         \n\
         Cleanup checklist:\n\
         - After cleanup, `git status --short --untracked-files=all` must not contain new dirty entries beyond those present before reviewer dispatch; `{outbox_path}` is not required to change mtime.\n\
         \n\
         WORKSPACE_CLEANUP_REQUIRED:\n\
         - previous_workspace_dirty: true\n\
         - cleanup_reason: {rejection}\n\
         - commit_or_revert_allowed: true\n\
         - push_allowed: false\n\
         - rewrite_reviewer_yaml_verdict: false\n\
         \n\
         Stop immediately after cleanup.",
        run_paths_section = run_paths_section,
        inbox_path = v.inbox_path,
        outbox_path = v.outbox_path,
        repo = v.workspace_root,
        rejection = rejection,
        notion_protocol_path = notion_protocol_path,
        gitlab_protocol_path = gitlab_protocol_path,
        claude_subagent_rule = claude_subagent_rule(v.provider),
        git_history_rewrite_rule = GIT_HISTORY_REWRITE_RULE,
    )
}

/// Render the executor feedback prompt template.
fn render_executor_feedback(v: &TemplateValues) -> String {
    let run_paths_section = run_paths_section(v);
    let project_profiles_section = executor_project_profiles_section(v);
    let executor_breakage_block_rule = executor_breakage_block_rule(v.workflow_type);
    let executor_implement_verification_scope_rule =
        executor_implement_verification_scope_rule(v.workflow_type);
    let executor_plan_test_coverage_rule = executor_plan_test_coverage_rule(v.workflow_type);
    let executor_investigation_causal_chain_rule =
        executor_investigation_causal_chain_rule(v.workflow_type);
    let artifact_structure_name = artifact_structure_name(v.workflow_type);
    let artifact_structure_items = artifact_structure_items(v.workflow_type);
    let remote_network_policy_section = remote_network_policy_section(v.remote_network_policy);
    let claude_subagent_rule = claude_subagent_rule(v.provider);
    let opencode_outbox_write_rule = opencode_outbox_write_rule(v.provider, &v.outbox_path);
    let artifact_structure_section = if artifact_structure_name.is_empty() {
        String::new()
    } else {
        format!(
            "Artifact structure:\n\
             - Required structure for {artifact_structure_name}:\n\
             {artifact_structure_items}\n\
             "
        )
    };
    let review_result_yaml = v.review_result_yaml.as_deref().unwrap_or("(not available)");
    let feedback_for_executor = v
        .feedback_for_executor
        .as_deref()
        .unwrap_or("(no feedback provided)");

    format!(
        "You are the executor agent in workflow `{wf}`.\n\
         \n\
         Run context:\n\
         - workspace_root: `{repo}`\n\
         - branch: `{branch}`\n\
         - notion_policy: `{notion_policy}`\n\
         \n\
         {run_paths_section}\
         Original user prompt:\n\
         {prompt}\n\
         \n\
         Workflow contract:\n\
         {contract}\n\
         \n\
         {artifact_structure_section}\
         Git facts:\n\
         {git_facts}\n\
         \n\
         Reviewer result in YAML:\n\
         {review_result_yaml}\n\
         \n\
         Feedback for you:\n\
         {feedback_for_executor}\n\
         \n\
         {remote_network_policy_section}\
         Interaction rules:\n\
         - Treat feedback as peer review.\n\
         - Do not apply reviewer feedback blindly: first independently assess each item, verify facts, and check applicability to the current repo/request.\n\
         - Fix only feedback items you agree with after verification.\n\
         - If you agree with a feedback item, update artifacts/code/plan according to that feedback.\n\
         - If you disagree with a feedback item, do not make that fix; explicitly explain why in outbox and support it with facts.\n\
         - In outbox, provide status for every feedback item: accepted_fixed, accepted_not_done, or rejected_with_reason.\n\
         - Do not ignore major/critical findings.\n\
         {claude_subagent_rule}\
         \n\
         {project_profiles_section}\
         \n\
         Executor working rules:\n\
         \n\
         1. Inputs and artifacts\n\
         - Read inbox by absolute path: `{inbox_path}`.\n\
         {artifact_reread_rule}\
         \n\
         2. Transport and outbox\n\
         - Before the final response, clear outbox at absolute path `{outbox_path}` in-place via truncate and write the current round result there.\n\
         {opencode_outbox_write_rule}\
         - Do not use `.agent-io` from other directories.\n\
         - Before writing outbox, verify that `{outbox_path}` starts with `workspace_root` (`{repo}`). If outbox path is not inside `workspace_root`, stop and write an error to the correct `{outbox_path}`.\n\
         - Do not commit transport files: `{inbox_path}` and `{outbox_path}`.\n\
         \n\
         3. Git and workspace safety\n\
         - Do not push commits without a direct user command.\n\
         {git_history_rewrite_rule}\
         - If you change git-tracked files in workspace_root (including `PLAN.md`/`INVESTIGATION.md`), create a local commit.\n\
         - Do not run destructive git operations without a direct user command.\n\
         \n\
         4. Language and formatting\n\
         - Write user-facing artifacts in English (`PLAN.md`, `INVESTIGATION.md`, executor outbox, reviewer YAML: `rationale`, `feedback_for_executor`, `checks_performed`, `findings`, `verification_commands`, `blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`).\n\
         - Add/edit project code comments only in English.\n\
         - In outbox, include commit hash and executed verification commands.\n\
         \n\
         5. Checks and unfinished work\n\
         - If you add/change tests, run the new/changed tests and the relevant module/crate-level scope.\n\
         - Placeholder tests do not count as valid tests.\n\
         - Report any unfinished work, skipped checks, or failures in outbox.\n\
         {executor_breakage_block_rule}\
         {executor_implement_verification_scope_rule}\
         {executor_plan_test_coverage_rule}\
         {executor_investigation_causal_chain_rule}\
         \n\
         Outbox format:\n\
         - For this role, outbox is free-form.\n\
         - You must list every feedback item and its status: what was fixed, what was accepted but not completed, what you disagree with and why.\n\
         \n\
         When finished, write the result to outbox at absolute path `{outbox_path}` and stop.",
        wf = v.workflow_type,
        repo = v.workspace_root,
        branch = v.branch,
        notion_policy = v.notion_policy,
        run_paths_section = run_paths_section,
        inbox_path = v.inbox_path,
        outbox_path = v.outbox_path,
        prompt = v.user_prompt,
        contract = v.workflow_contract,
        artifact_structure_section = artifact_structure_section,
        git_facts = v.git_facts,
        review_result_yaml = review_result_yaml,
        feedback_for_executor = feedback_for_executor,
        remote_network_policy_section = remote_network_policy_section,
        claude_subagent_rule = claude_subagent_rule,
        project_profiles_section = project_profiles_section,
        opencode_outbox_write_rule = opencode_outbox_write_rule,
        git_history_rewrite_rule = GIT_HISTORY_REWRITE_RULE,
        artifact_reread_rule = ARTIFACT_REREAD_RULE,
        executor_breakage_block_rule = executor_breakage_block_rule,
        executor_implement_verification_scope_rule = executor_implement_verification_scope_rule,
        executor_plan_test_coverage_rule = executor_plan_test_coverage_rule,
        executor_investigation_causal_chain_rule = executor_investigation_causal_chain_rule,
    )
}

/// Render one of the three fixed prompt templates into a String.
pub fn render_template(id: TemplateId, values: &TemplateValues) -> String {
    match id {
        TemplateId::ExecutorInitial => render_executor_initial(values),
        TemplateId::ReviewerReview => render_reviewer_review(values),
        TemplateId::ReviewerRepairYaml => render_reviewer_repair_yaml(values),
        TemplateId::ReviewerRepairWorkspace => render_reviewer_repair_workspace(values),
        TemplateId::ExecutorFeedback => render_executor_feedback(values),
    }
}

/// Return the hardcoded reviewer YAML schema literal embedded in prompts.
pub fn reviewer_yaml_schema() -> &'static str {
    REVIEWER_YAML_SCHEMA
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{NotionPolicy, RemoteNetworkPolicy, TemplateValues};

    fn sample_values(wf: WorkflowType) -> TemplateValues {
        TemplateValues {
            provider: ProviderKind::Claude,
            workflow_type: wf,
            workspace_root: "/repo".to_owned(),
            transport_dir: "/repo/.agent-io".to_owned(),
            inbox_path: "/repo/.agent-io/inbox.txt".to_owned(),
            outbox_path: "/repo/.agent-io/outbox.txt".to_owned(),
            orchestrator_docs_dir: "/orchestrator/docs".to_owned(),
            branch: "main".to_owned(),
            user_prompt: "Fix the bug".to_owned(),
            notion_policy: NotionPolicy::Optional,
            remote_network_policy: RemoteNetworkPolicy::Forbidden,
            workflow_contract: workflow_contract_text(wf).to_owned(),
            git_facts: "git_status: (clean)\ngit_head: abc1234 initial commit".to_owned(),
            executor_outbox_present: true,
            reviewer_yaml_schema: Some(REVIEWER_YAML_SCHEMA.to_owned()),
            reviewer_yaml_rejection: None,
            reviewer_workspace_rejection: None,
            review_result_yaml: None,
            feedback_for_executor: None,
        }
    }

    fn sample_values_for_provider(wf: WorkflowType, provider: ProviderKind) -> TemplateValues {
        let mut values = sample_values(wf);
        values.provider = provider;
        values
    }

    #[test]
    fn executor_initial_contains_key_fields() {
        let v = sample_values(WorkflowType::Implement);
        let out = render_template(TemplateId::ExecutorInitial, &v);
        assert!(out.contains("implement"));
        assert!(out.contains("/repo"));
        assert!(out.contains("main"));
        assert!(out.contains("Fix the bug"));
        assert!(out.contains("outbox.txt"));
    }

    #[test]
    fn reviewer_prompt_contains_schema() {
        let v = sample_values(WorkflowType::Implement);
        let out = render_template(TemplateId::ReviewerReview, &v);
        assert!(out.contains("quality_score"));
        assert!(out.contains("checks_performed"));
    }

    #[test]
    fn reviewer_prompt_does_not_include_yaml_rejection_block() {
        let mut v = sample_values(WorkflowType::Implement);
        v.reviewer_yaml_rejection = Some("yaml parse error: bad decision".to_owned());
        let out = render_template(TemplateId::ReviewerReview, &v);
        assert!(!out.contains("YAML_REJECTION"));
    }

    #[test]
    fn reviewer_repair_prompt_contains_rejection_block() {
        let mut v = sample_values(WorkflowType::Implement);
        v.reviewer_yaml_rejection = Some("yaml parse error: bad decision".to_owned());
        let out = render_template(TemplateId::ReviewerRepairYaml, &v);
        assert!(out.contains("Task:"));
        assert!(out.contains("Transport and constraints:"));
        assert!(out.contains("YAML_REJECTION"));
        assert!(out.contains("STRICT_FIELDS_CHECKLIST"));
        assert!(out.contains("Schema:"));
        assert!(out.contains("bad decision"));
    }

    #[test]
    fn reviewer_workspace_repair_prompt_requires_cleanup() {
        let mut v = sample_values(WorkflowType::Implement);
        v.reviewer_workspace_rejection =
            Some("reviewer left dirty worktree entries after review: M src/lib.rs".to_owned());
        let out = render_template(TemplateId::ReviewerRepairWorkspace, &v);

        assert!(out.contains("WORKSPACE_CLEANUP_REQUIRED"));
        assert!(out.contains("Cleanup task:"));
        assert!(out.contains("Allowed:"));
        assert!(out.contains("Forbidden:"));
        assert!(out.contains("Transport and protocols:"));
        assert!(out.contains("Cleanup checklist:"));
        assert!(out.contains("do not rewrite the reviewer YAML verdict"));
        assert!(out.contains("commit the leftover changes locally or revert/delete them"));
        assert!(out.contains("Do not push commits"));
        assert!(out.contains("rewrite_reviewer_yaml_verdict: false"));
        assert!(out.contains("M src/lib.rs"));
        assert!(!out.contains("Use this minimal schema"));
    }

    #[test]
    fn reviewer_prompt_outbox_present_mentions_read() {
        let v = sample_values(WorkflowType::Implement);
        let out = render_template(TemplateId::ReviewerReview, &v);
        assert!(out.contains("Read `/repo/.agent-io/outbox.txt` yourself before review"));
        assert!(!out.contains("soft-success"));
    }

    #[test]
    fn reviewer_prompt_soft_success_no_outbox_read_instruction() {
        let mut v = sample_values(WorkflowType::Implement);
        v.executor_outbox_present = false;
        let out = render_template(TemplateId::ReviewerReview, &v);
        assert!(out.contains("soft-success"));
        assert!(!out.contains("Read `/repo/.agent-io/outbox.txt` yourself before review"));
    }

    #[test]
    fn all_prompts_include_claude_subagent_rule() {
        let v = sample_values(WorkflowType::Implement);

        for template in [
            TemplateId::ExecutorInitial,
            TemplateId::ReviewerReview,
            TemplateId::ReviewerRepairYaml,
            TemplateId::ReviewerRepairWorkspace,
            TemplateId::ExecutorFeedback,
        ] {
            let out = render_template(template, &v);
            assert!(out.contains("If you are Claude: delegate as many independent subtasks"));
            assert!(out.contains("personally validating their outputs"));
        }
    }

    #[test]
    fn provider_specific_prompt_rules_are_conditional() {
        let claude = sample_values_for_provider(WorkflowType::Implement, ProviderKind::Claude);
        let opencode = sample_values_for_provider(WorkflowType::Implement, ProviderKind::Opencode);
        let codex = sample_values_for_provider(WorkflowType::Implement, ProviderKind::Codex);
        let templates = [
            TemplateId::ExecutorInitial,
            TemplateId::ReviewerReview,
            TemplateId::ReviewerRepairYaml,
            TemplateId::ReviewerRepairWorkspace,
            TemplateId::ExecutorFeedback,
        ];

        for template in templates {
            let out = render_template(template, &claude);
            assert!(out.contains("If you are Claude:"));
            assert!(!out.contains("If you are OpenCode:"));

            let out = render_template(template, &opencode);
            assert!(!out.contains("If you are Claude:"));

            let out = render_template(template, &codex);
            assert!(!out.contains("If you are Claude:"));
            assert!(!out.contains("If you are OpenCode:"));
        }

        let claude_initial = render_template(TemplateId::ExecutorInitial, &claude);
        assert!(!claude_initial.contains("If you are OpenCode:"));

        let opencode_initial = render_template(TemplateId::ExecutorInitial, &opencode);
        assert!(opencode_initial.contains("If you are OpenCode:"));

        let opencode_review = render_template(TemplateId::ReviewerReview, &opencode);
        assert!(opencode_review.contains("If you are OpenCode:"));
    }

    #[test]
    fn prompts_use_absolute_transport_paths() {
        let v = sample_values(WorkflowType::Implement);

        for template in [
            TemplateId::ExecutorInitial,
            TemplateId::ReviewerReview,
            TemplateId::ReviewerRepairYaml,
            TemplateId::ExecutorFeedback,
        ] {
            let out = render_template(template, &v);
            assert!(out.contains("Paths for this run:"));
            assert!(out.contains("transport_dir: `/repo/.agent-io`"));
            assert!(out.contains("inbox_path: `/repo/.agent-io/inbox.txt`"));
            assert!(out.contains("outbox_path: `/repo/.agent-io/outbox.txt`"));
            assert!(out.contains("orchestrator_docs_dir: `/orchestrator/docs`"));
            assert!(out
                .contains("do not use it as the workspace and do not look for `.agent-io` there"));
            assert!(out.contains(".agent-io` from other directories"));
            assert!(out.contains("`/repo/.agent-io/outbox.txt` starts with `workspace_root`"));
            assert!(!out.contains("./.agent-io/"));
        }
    }

    #[test]
    fn prompts_enumerate_english_user_facing_artifacts() {
        let v = sample_values(WorkflowType::Implement);

        for template in [
            TemplateId::ExecutorInitial,
            TemplateId::ReviewerReview,
            TemplateId::ReviewerRepairYaml,
            TemplateId::ExecutorFeedback,
        ] {
            let out = render_template(template, &v);
            assert!(out.contains("Write user-facing artifacts in English"));
            assert!(out.contains("`PLAN.md`, `INVESTIGATION.md`, executor outbox"));
            assert!(out.contains("reviewer YAML: `rationale`, `feedback_for_executor`"));
            assert!(out
                .contains("`blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`"));
        }
    }

    #[test]
    fn normal_prompts_include_project_profile_instructions() {
        let v = sample_values(WorkflowType::Implement);

        for template in [
            TemplateId::ExecutorInitial,
            TemplateId::ExecutorFeedback,
            TemplateId::ReviewerReview,
        ] {
            let out = render_template(template, &v);
            assert!(out.contains("Project-specific verification profiles:"));
            assert!(out.contains("`/orchestrator/docs/project_profiles/generic.md`"));
            assert!(out.contains("Independently identify the main workspace stack"));
            assert!(out.contains("`/orchestrator/docs/project_profiles/rust.md`"));
            assert!(out.contains("`/orchestrator/docs/project_profiles/cpp.md`"));
            assert!(out.contains("If the project is mixed-stack"));
            assert!(out.contains("Do not invent verification commands"));
            assert!(!out.contains("Executor must"));
            assert!(!out.contains("Reviewer must"));
        }

        for template in [TemplateId::ExecutorInitial, TemplateId::ExecutorFeedback] {
            let out = render_template(template, &v);
            assert!(out.contains("Profile reporting:"));
            assert!(out.contains("In outbox, state which project profiles you read"));
            assert!(!out.contains("Profile review reporting:"));
            assert!(!out.contains("In `checks_performed`, state"));
        }

        let yaml_repair = render_template(TemplateId::ReviewerRepairYaml, &v);
        let workspace_repair = render_template(TemplateId::ReviewerRepairWorkspace, &v);
        assert!(!yaml_repair.contains("Project-specific verification profiles:"));
        assert!(!workspace_repair.contains("Project-specific verification profiles:"));

        let review = render_template(TemplateId::ReviewerReview, &v);
        assert!(review.contains("Profile review reporting:"));
        assert!(review.contains("In `checks_performed`, state"));
        assert!(review.contains("Verify that the executor stated and applied"));
        assert!(review.contains("the executor did not state reading/applying"));
        assert!(review.contains("this is a finding"));
        assert!(!review.contains("Profile reporting:"));
    }

    #[test]
    fn executor_prompts_group_working_rules_by_concern() {
        let v = sample_values(WorkflowType::Implement);

        for template in [TemplateId::ExecutorInitial, TemplateId::ExecutorFeedback] {
            let out = render_template(template, &v);
            assert!(out.contains("Executor working rules:"));
            assert!(out.contains("1. Inputs and artifacts"));
            assert!(out.contains("2. Transport and outbox"));
            assert!(out.contains("3. Git and workspace safety"));
            assert!(out.contains("4. Language and formatting"));
            assert!(out.contains("5. Checks and unfinished work"));
            assert!(out.contains("Read inbox by absolute path"));
            assert!(out.contains("Do not commit transport files"));
            assert!(out.contains("If you change git-tracked files"));
            assert!(out.contains("Write user-facing artifacts in English"));
            assert!(out.contains("Placeholder tests do not count as valid tests"));
        }
    }

    #[test]
    fn normal_prompts_do_not_make_rust_specific_checks_mandatory_inline() {
        let v = sample_values(WorkflowType::Implement);

        for template in [
            TemplateId::ExecutorInitial,
            TemplateId::ExecutorFeedback,
            TemplateId::ReviewerReview,
        ] {
            let out = render_template(template, &v);
            assert!(!out.contains("If `Cargo.lock` changed"));
            assert!(!out.contains("Do not replace `cargo fmt --all`"));
            assert!(!out.contains("After formatting, run `make clippy`"));
        }
    }

    #[test]
    fn generic_profile_contains_common_verification_rules() {
        let profile = include_str!("../docs/project_profiles/generic.md");

        assert!(profile.contains("repo-approved commands"));
        assert!(profile.contains("Prefer scoped checks before broad checks"));
        assert!(profile.contains("skipped check"));
        assert!(profile.contains("role-specific prompt instructions"));
        assert!(!profile.contains("Reviewer must"));
        assert!(!profile.contains("Executor must"));
    }

    #[test]
    fn rust_profile_contains_rust_specific_rules() {
        let profile = include_str!("../docs/project_profiles/rust.md");

        assert!(profile.contains("`cargo fmt --all`"));
        assert!(profile.contains("Do not replace `cargo fmt --all` with `cargo fmt --all --check`"));
        assert!(profile.contains("`make clippy`"));
        assert!(profile.contains("`Cargo.lock`"));
    }

    #[test]
    fn cpp_profile_contains_cxx_specific_decision_tree() {
        let profile = include_str!("../docs/project_profiles/cpp.md");

        assert!(profile.contains("`CMakePresets.json`"));
        assert!(profile.contains("`compile_commands.json`"));
        assert!(profile.contains("repo-approved workflow"));
        assert!(profile.contains("Do not run broad `clang-format -i`"));
        assert!(profile.contains("`ctest --test-dir <build-dir>`"));
        assert!(profile.contains("`clang-format --dry-run --Werror <changed-files>`"));
        assert!(profile.contains("C/C++ commands are not as universal"));
        assert!(profile.contains("## Build System Decision Tree"));
        assert!(profile.contains("### CMake presets"));
        assert!(profile.contains("### Existing build dir + compile_commands.json"));
        assert!(profile.contains("### Ninja"));
        assert!(profile.contains("### Make"));
        assert!(profile.contains("### Meson"));
        assert!(profile.contains("### Bazel"));
        assert!(profile.contains("### Autotools/configure"));
        assert!(profile.contains("do not run\n  a guessed `cmake -S . -B build`"));
    }

    #[test]
    fn plan_prompt_requires_materialized_implementation_steps() {
        let v = sample_values(WorkflowType::Plan);
        let out = render_template(TemplateId::ExecutorInitial, &v);

        assert!(out.contains("do not defer discovery/assessment to implementation"));
        assert!(out.contains("before writing `PLAN.md`, read the relevant files"));
        assert!(out.contains("`Implementation steps` must be concrete and materializable"));
        assert!(out.contains("Each item must have a clear expected outcome"));
        assert!(out.contains("name concrete files and code anchors"));
        assert!(out.contains("add file:line refs"));
        assert!(out.contains("Add short code snippets or pseudocode"));
        assert!(out.contains("snippets must capture intent and contract"));
        assert!(out.contains("`assess`, `study`, `investigate`, `figure out`"));
        assert!(out.contains("do not hide that in `Implementation steps`"));
        assert!(out.contains("put it in `Open questions` / `Blockers`"));
        assert!(out.contains("Detected stack/profiles"));
        assert!(out.contains("Repo-approved commands found"));
        assert!(out.contains("Verification plan"));
    }

    #[test]
    fn normal_prompts_require_rereading_plan_and_investigation_artifacts() {
        let v = sample_values(WorkflowType::Implement);

        for template in [
            TemplateId::ExecutorInitial,
            TemplateId::ExecutorFeedback,
            TemplateId::ReviewerReview,
        ] {
            let out = render_template(template, &v);
            assert!(out.contains("reread"));
            assert!(out.contains("`PLAN.md`"));
            assert!(out.contains("`INVESTIGATION.md`"));
            assert!(out.contains("right now"));
            assert!(out.contains("already know"));
        }
    }

    #[test]
    fn implement_executor_prompts_do_not_require_full_build_when_profile_checks_suffice() {
        let implement = sample_values(WorkflowType::Implement);
        let plan = sample_values(WorkflowType::Plan);

        for template in [TemplateId::ExecutorInitial, TemplateId::ExecutorFeedback] {
            let out = render_template(template, &implement);
            assert!(out.contains("A full build is not required"));
            assert!(out.contains("project profile or repo-approved checks"));
            assert!(out.contains("explicitly state the selected check in outbox"));

            let plan_out = render_template(template, &plan);
            assert!(!plan_out.contains("A full build is not required"));
        }
    }

    #[test]
    fn prompts_forbid_git_history_rewrite() {
        let v = sample_values(WorkflowType::Implement);

        for template in [
            TemplateId::ExecutorInitial,
            TemplateId::ExecutorFeedback,
            TemplateId::ReviewerReview,
            TemplateId::ReviewerRepairWorkspace,
        ] {
            let out = render_template(template, &v);
            assert!(out.contains("`git commit --amend`"));
            assert!(out.contains("`git rebase`"));
            assert!(out.contains("`git reset`"));
            assert!(out.contains("`git push --force`"));
            assert!(out.contains("If another fix is needed after a previous commit"));
            assert!(out.contains("Do not create commits while in detached HEAD mode"));
        }
    }

    #[test]
    fn reviewer_prompt_forbids_mutating_formatters() {
        let v = sample_values(WorkflowType::Implement);
        let review = render_template(TemplateId::ReviewerReview, &v);

        assert!(review.contains("reviewer must not run mutating formatters/fixers"));
        assert!(review.contains("non-mutating check commands from the relevant project profile"));
        assert!(review.contains("do not fix formatting yourself"));
    }

    #[test]
    fn feedback_prompt_contains_review_yaml() {
        let mut v = sample_values(WorkflowType::Plan);
        v.review_result_yaml = Some("decision: revise".to_owned());
        v.feedback_for_executor = Some("1. Add tests".to_owned());
        let out = render_template(TemplateId::ExecutorFeedback, &v);
        assert!(out.contains("decision: revise"));
        assert!(out.contains("Add tests"));
    }

    #[test]
    fn feedback_prompt_forbids_blind_reviewer_feedback_execution() {
        let mut v = sample_values(WorkflowType::Implement);
        v.feedback_for_executor = Some("1. Fix foo".to_owned());
        let out = render_template(TemplateId::ExecutorFeedback, &v);

        assert!(out.contains("Do not apply reviewer feedback blindly"));
        assert!(out.contains("Fix only feedback items you agree with"));
        assert!(out.contains("do not make that fix"));
        assert!(out.contains("accepted_fixed, accepted_not_done, or rejected_with_reason"));
    }

    #[test]
    fn investigate_prompts_use_general_research_structure() {
        let v = sample_values(WorkflowType::Investigate);
        let executor = render_template(TemplateId::ExecutorInitial, &v);
        let reviewer = render_template(TemplateId::ReviewerReview, &v);

        for out in [executor, reviewer] {
            assert!(out.contains("Research questions"));
            assert!(out.contains("Sources checked"));
            assert!(out.contains("Evidence references"));
            assert!(out.contains("file:line refs for code"));
            assert!(out.contains("Observed symptom"));
            assert!(out.contains("Immediate cause"));
            assert!(out.contains("Causal chain / why chain"));
            assert!(out.contains("Evidence per causal link"));
            assert!(out.contains("Root cause / unresolved boundary"));
            assert!(out.contains("Detected stack/profiles"));
            assert!(out.contains("Repo-approved commands found"));
            assert!(out.contains("Verification/falsification steps for findings"));
            assert!(out.contains("do not format this as an automated test plan"));
            assert!(out.contains("Findings"));
            assert!(out.contains("Conclusions"));
            assert!(!out.contains("- Symptom\n"));
            assert!(!out.contains("Most likely root cause"));
        }
    }

    #[test]
    fn investigate_reviewer_requires_evidence_references() {
        let v = sample_values(WorkflowType::Investigate);
        let out = render_template(TemplateId::ReviewerReview, &v);

        assert!(out.contains("each key claim/conclusion is backed"));
        assert!(out.contains("file:line refs for code"));
        assert!(out.contains("commit hashes for git history"));
        assert!(out.contains("command/log excerpts for logs/CLI"));
        assert!(out.contains("paraphrase without verifiable links to primary sources"));
        assert!(out.contains("top-level/immediate cause"));
        assert!(out.contains("why it became possible"));
        assert!(out.contains("causal chain"));
        assert!(out.contains("unresolved boundary"));
    }

    #[test]
    fn investigate_executor_requires_causal_chain_for_primary_hypotheses() {
        let v = sample_values(WorkflowType::Investigate);
        let executor = render_template(TemplateId::ExecutorInitial, &v);
        let feedback = render_template(TemplateId::ExecutorFeedback, &v);

        for out in [executor, feedback] {
            assert!(out.contains("do not stop at the top-level cause / immediate cause"));
            assert!(out.contains("observed symptom -> immediate technical cause"));
            assert!(out.contains("why that condition was possible"));
            assert!(out.contains("actionable root cause or explicit unresolved boundary"));
            assert!(out.contains("evidence per link"));
            assert!(out.contains("what would falsify this hypothesis"));
            assert!(out.contains("do not force an artificial chain"));
        }
    }

    #[test]
    fn reviewer_prompt_requires_checking_omitted_scope() {
        let v = sample_values(WorkflowType::Plan);
        let out = render_template(TemplateId::ReviewerReview, &v);

        assert!(out.contains("CRITICAL: review purpose"));
        assert!(out.contains("What to check:"));
        assert!(out.contains("1. Scope reconstruction"));
        assert!(out.contains("2. Artifacts and claims review"));
        assert!(out.contains("3. Code and diff review"));
        assert!(out.contains("4. Risks and compatibility"));
        assert!(out.contains("5. Finding requirements"));
        assert!(out.contains("Workflow-specific checks:"));
        assert!(out.contains("Minimum checklist before accept:"));
        assert!(out.contains("1. Workspace and Git"));
        assert!(out.contains("2. Transport and outbox"));
        assert!(out.contains("3. Language and formatting"));
        assert!(out.contains("4. Contract, git facts, and artifacts"));
        assert!(out.contains("5. Remote access violations"));
        assert!(out.contains("Decision policy:"));
        assert!(out.contains("Output format:"));
        assert!(out.contains("perform a full critical review"));
        assert!(out.contains("do not replace reading, analysis, and code/artifact review"));
        assert!(out.contains(
            "decision=accept` is forbidden if you did not independently verify key claims"
        ));
        assert!(out.contains("Do not limit review to what the executor mentioned"));
        assert!(out.contains("independently reconstruct expected scope"));
        assert!(out.contains("important questions"));
        assert!(out.contains("decision=accept` is forbidden if the omission is material"));
        assert!(out.contains("all changed files were checked;"));
        assert!(out.contains("tests are not the only basis for accept;"));
        assert!(out.contains("no high/critical findings exist."));
    }

    #[test]
    fn reviewer_prompt_has_workflow_specific_coverage_rules() {
        let investigate = render_template(
            TemplateId::ReviewerReview,
            &sample_values(WorkflowType::Investigate),
        );
        let plan = render_template(
            TemplateId::ReviewerReview,
            &sample_values(WorkflowType::Plan),
        );
        let implement = render_template(
            TemplateId::ReviewerReview,
            &sample_values(WorkflowType::Implement),
        );

        assert!(investigate.contains("independently formulate research questions"));
        assert!(plan.contains("covers the user prompt and `INVESTIGATION.md`"));
        assert!(implement.contains("check relevant neighboring call sites"));
    }

    #[test]
    fn reviewer_plan_prompt_rejects_deferred_discovery_steps() {
        let v = sample_values(WorkflowType::Plan);
        let out = render_template(TemplateId::ReviewerReview, &v);

        assert!(out.contains("concrete materializable actions"));
        assert!(out.contains("concrete files and code anchors"));
        assert!(out.contains("file:line refs must be present"));
        assert!(out.contains("short code snippets or pseudocode"));
        assert!(out.contains("not replace the full implementation"));
        assert!(out
            .contains("Standalone implementation steps such as `assess`, `study`, `investigate`"));
        assert!(out.contains("defer discovery instead of planning implementation"));
        assert!(out.contains("leaves solution choice to implementation"));
    }

    #[test]
    fn forbidden_remote_network_policy_adds_restrictions_section() {
        let v = sample_values(WorkflowType::Implement);
        let out = render_template(TemplateId::ExecutorInitial, &v);
        assert!(out.contains("Remote access policy:"));
        assert!(out.contains("Do not use SSH to access remote target systems."));
        assert!(out.contains("Do not make HTTP requests to remote target systems."));
        assert!(!out.contains("InternalProduct"));
    }

    #[test]
    fn read_only_remote_network_policy_allows_only_read_actions() {
        let mut v = sample_values(WorkflowType::Implement);
        v.remote_network_policy = RemoteNetworkPolicy::ReadOnly;
        let out = render_template(TemplateId::ExecutorInitial, &v);
        assert!(out.contains("Only read-only access"));
        assert!(out.contains("HTTP is allowed only for read-only requests"));
        assert!(out.contains("Do not unpack log archives"));
        assert!(out.contains("POST/PUT/PATCH/DELETE"));
        assert!(!out.contains("InternalProduct"));
    }

    #[test]
    fn operational_remote_network_policy_allows_limited_mutations() {
        let mut v = sample_values(WorkflowType::Implement);
        v.remote_network_policy = RemoteNetworkPolicy::Operational;
        let out = render_template(TemplateId::ExecutorInitial, &v);
        assert!(out.contains("Operational actions are allowed"));
        assert!(out.contains("DB mutations are forbidden"));
        assert!(out.contains("Do not unpack log archives"));
        assert!(out.contains("List all mutating actions in outbox"));
        assert!(!out.contains("InternalProduct"));
    }

    #[test]
    fn reviewer_prompt_requires_remote_access_policy_check() {
        let v = sample_values(WorkflowType::Implement);
        let out = render_template(TemplateId::ReviewerReview, &v);
        assert!(out.contains("Check remote access policy compliance"));
        assert!(out.contains("return `decision: blocked`"));
        assert!(out.contains("hard_blockers_present: true"));
        assert!(out.contains("do not return `revise`"));
        assert!(out.contains("do not ask the executor to continue investigating"));
    }

    #[test]
    fn remote_access_policy_is_top_level_prompt_section() {
        let v = sample_values(WorkflowType::Implement);
        let executor = render_template(TemplateId::ExecutorInitial, &v);
        let reviewer = render_template(TemplateId::ReviewerReview, &v);
        let feedback = render_template(TemplateId::ExecutorFeedback, &v);

        assert!(executor.contains("Remote access policy:\n"));
        assert!(executor.contains("local CLI tools.\n\nMandatory rules:"));
        assert!(executor.contains("Project-specific verification profiles:"));
        assert!(executor.contains("Executor working rules:"));
        assert!(reviewer.contains("local CLI tools.\n\nCRITICAL: review purpose"));
        assert!(reviewer.contains("material risks.\n\nWhat to check:"));
        assert!(feedback.contains("local CLI tools.\n\nInteraction rules:"));
        assert!(feedback.contains("Executor working rules:"));
    }

    #[test]
    fn prompts_do_not_expose_remote_network_policy_field() {
        let v = sample_values(WorkflowType::Implement);

        for template in [
            TemplateId::ExecutorInitial,
            TemplateId::ReviewerReview,
            TemplateId::ExecutorFeedback,
        ] {
            let out = render_template(template, &v);
            assert!(!out.contains("remote_network_policy:"));
        }
    }

    #[test]
    fn soft_success_plan_requires_plan_md() {
        let mut m = std::collections::HashMap::new();
        m.insert("PLAN.md".to_owned(), false);
        assert!(!soft_success_allowed(WorkflowType::Plan, &m));
        m.insert("PLAN.md".to_owned(), true);
        assert!(soft_success_allowed(WorkflowType::Plan, &m));
    }

    #[test]
    fn soft_success_investigate_requires_investigation_md() {
        let mut m = std::collections::HashMap::new();
        m.insert("INVESTIGATION.md".to_owned(), false);
        assert!(!soft_success_allowed(WorkflowType::Investigate, &m));
        m.insert("INVESTIGATION.md".to_owned(), true);
        assert!(soft_success_allowed(WorkflowType::Investigate, &m));
    }

    #[test]
    fn soft_success_implement_always_allowed() {
        // Implement delegates commit/worktree/test validation to the reviewer.
        // soft_success_allowed itself always returns true regardless of artifact map.
        assert!(soft_success_allowed(
            WorkflowType::Implement,
            &std::collections::HashMap::new()
        ));
        let mut m = std::collections::HashMap::new();
        m.insert("PLAN.md".to_owned(), false);
        assert!(soft_success_allowed(WorkflowType::Implement, &m));
    }
}
