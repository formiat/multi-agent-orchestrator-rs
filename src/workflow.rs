use crate::state::{RemoteNetworkPolicy, TemplateId, TemplateValues, WorkflowType};

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

const PROPTEST_TEST_RULE: &str = "- When running `cargo test`, always disable proptest failure persistence: `PROPTEST_DISABLE_FAILURE_PERSISTENCE=1 cargo test ...`. If you use `runlim`: `PROPTEST_DISABLE_FAILURE_PERSISTENCE=1 runlim cargo test ...`. Do not commit or leave `*.proptest-regressions`.\n";

const EXECUTOR_RUST_FORMAT_RULE: &str = "- For Rust changes before commit, always run the mutating formatter: `cargo fmt --all`, then `make clippy` or a repo-approved equivalent. Do not replace `cargo fmt --all` with `cargo fmt --all --check`: the executor must format its own changes before commit.\n";

const REVIEWER_FORMAT_CHECK_RULE: &str = "- The reviewer must not run mutating formatters/fixers: `cargo fmt`, `cargo fmt --all`, `cargo fix`, auto-fix linters, or similar commands. For formatting verification, use only the non-mutating command: `cargo fmt --all --check`. If formatting fails, return a finding/feedback; do not fix formatting yourself.\n";

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

fn run_paths_section(v: &TemplateValues) -> String {
    format!(
        "Run paths:\n\
         - workspace_root: `{workspace_root}`\n\
         - transport_dir: `{transport_dir}`\n\
         - inbox_path: `{inbox_path}`\n\
         - outbox_path: `{outbox_path}`\n\
         - orchestrator_docs_dir: `{orchestrator_docs_dir}`\n\
         \n\
         All project files, `PLAN.md`, `INVESTIGATION.md`, `.agent-io/inbox.txt`, and `.agent-io/outbox.txt` belong only to `workspace_root`.\n\
         The orchestrator docs directory is only for reading instructions; do not use it as a workspace and do not look for `.agent-io` there.\n\
         Protocol files live in the orchestrator repository and are read only as instructions. This is not the task workspace. After reading protocols, perform all actions in `workspace_root`.\n\
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
        "- If `INVESTIGATION.md` exists in workspace_root, read it and use it as planning input.\n"
    } else {
        ""
    }
}

fn implement_plan_rule(wf: WorkflowType) -> &'static str {
    if wf == WorkflowType::Implement {
        "- If `PLAN.md` exists in workspace_root, read it and use it as implementation input.\n"
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
            "- If `INVESTIGATION.md` already exists, read it first and refine/fix it as the current artifact instead of ignoring it.\n"
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
             - Affected components\n\
             - Implementation steps (numbered, with file paths)\n\
             - Testing strategy (categories 1/2/3: no refactoring / light refactoring / heavy refactoring)\n\
             - Risks and tradeoffs\n\
             - Open questions\n"
        }
        WorkflowType::Investigate => {
            "- Context/Task\n\
             - Research questions\n\
             - Scope and constraints\n\
             - Sources checked\n\
             - Evidence\n\
             - Findings\n\
             - Relevant code paths\n\
             - Timeline/history (if applicable)\n\
             - Hypotheses/alternatives (if applicable)\n\
             - Risk/impact\n\
             - Conclusions\n\
             - Recommendations/next steps\n\
             - Testing/verification implications (if applicable)\n\
             - Open questions\n"
        }
        WorkflowType::Implement => "",
    }
}

fn reviewer_artifact_compliance_requirements(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Plan => {
            "- For workflow `plan` (loop A), verify the required `PLAN.md` structure:\n\
             - Verify that all required sections are present, substantively complete (not placeholders), and coherent: Affected components ↔ Implementation steps ↔ Testing strategy.\n\
             - `Testing strategy` must plan automated testing (unit/integration/e2e where applicable), not manual checks.\n\
             - Manual checks are allowed only as fallback and only with an explicit explanation of why an automated test is objectively impossible in the current context (explicit gap + reason).\n"
        }
        WorkflowType::Investigate => {
            "- For workflow `investigate` (loop A), verify the required `INVESTIGATION.md` structure:\n\
             - Verify that all required sections are present, substantively complete (not placeholders), and coherent: Research questions ↔ Evidence ↔ Findings ↔ Conclusions.\n\
             - Every research question from the user prompt must be answered by a conclusion or explicitly marked unresolved with a verifiable reason.\n"
        }
        WorkflowType::Implement => "",
    }
}

fn reviewer_investigate_independent_validation_rule(wf: WorkflowType) -> &'static str {
    if wf == WorkflowType::Investigate {
        "- For workflow `investigate`, independently verify every key claim/conclusion and every key piece of evidence from `INVESTIGATION.md` against primary sources (code, git history, logs, commands), not only against the artifact text.\n\
         - Do not confirm a conclusion without verifiable evidence; if verification was not possible, explicitly mark it as a gap/uncertainty in findings.\n"
    } else {
        ""
    }
}

fn reviewer_workflow_coverage_rule(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Investigate => {
            "- For workflow `investigate`: independently derive research questions from the user prompt and compare them with `INVESTIGATION.md`.\n\
             - Check beyond what the executor mentioned: assess whether important questions, code paths, sources, alternative explanations, risks, or limitations were missed.\n"
        }
        WorkflowType::Plan => {
            "- For workflow `plan`: independently verify that `PLAN.md` covers the user prompt and `INVESTIGATION.md` (if present), including affected components, implementation steps, risks/tradeoffs, and automated tests.\n\
             - Check beyond what the executor mentioned: assess whether important components, edge cases, migrations/data, integrations, risks, or test scenarios were missed.\n"
        }
        WorkflowType::Implement => {
            "- For workflow `implement`: in addition to every changed file, inspect relevant adjacent call sites, contracts, integrations, and invariants that may have been broken by the changes.\n\
             - Check beyond what the executor mentioned: assess whether related files, edge cases, tests, migrations/data, runtime paths, or backward compatibility were missed.\n"
        }
    }
}

fn executor_breakage_block_rule(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Plan | WorkflowType::Implement => {
            "- A `What could break` section is required: list potential regressions/risks after the changes (behavior, API/contracts, data/DB, integrations, performance/resources) and how to verify them.\n"
        }
        WorkflowType::Investigate => "",
    }
}

fn executor_plan_test_coverage_rule(wf: WorkflowType) -> &'static str {
    if wf == WorkflowType::Plan {
        "- For workflow `plan`, the plan must include automated tests (unit/integration/e2e where applicable), not manual checks.\n\
         - Manual checks are allowed only as fallback when automated testing is objectively impossible in the current context.\n\
         - For the automated test plan: cover happy path + negative path + edge cases within practical limits; if something cannot be covered by automated tests, explicitly record the gap and reason.\n"
    } else {
        ""
    }
}

fn remote_network_policy_section(policy: RemoteNetworkPolicy) -> &'static str {
    match policy {
        RemoteNetworkPolicy::Forbidden => {
            "Remote access policy:\n\
             - Do not access remote target systems over SSH.\n\
             - Do not make HTTP requests to remote target systems.\n\
             - Investigate only local code, local artifacts, and explicitly allowed local CLIs.\n\
             \n"
        }
        RemoteNetworkPolicy::ReadOnly => {
            "Remote access policy:\n\
             - Only read-only access to remote target systems is allowed for investigation.\n\
             - SSH is allowed only for reading: viewing logs, configs, statuses, metrics, versions, time, environment, and other diagnostic information.\n\
             - HTTP is allowed only for read-only requests: GET/HEAD/OPTIONS and necessary auth requests for read-only access.\n\
             - Any changes to files, configs, processes, services, DBs, queues, caches, or runtime state are forbidden.\n\
             - POST/PUT/PATCH/DELETE and any HTTP requests with side effects are forbidden, except explicitly necessary auth requests.\n\
             - Service stop/restart/reload and any commands that change system state are forbidden.\n\
             - Do not unpack log archives on a remote system; if compressed logs must be read, use streaming read/grep without creating files.\n\
             - If a command is potentially mutating or ambiguous, do not run it; first record the concern in outbox.\n\
             - If you performed remote SSH/HTTP actions, list them in outbox with classification: read-only or mutating.\n\
             \n"
        }
        RemoteNetworkPolicy::Operational => {
            "Remote access policy:\n\
             - Operational actions are allowed only on explicitly user-specified remote target systems.\n\
             - Application config changes, read/write HTTP requests, diagnostic setting toggles, and application stop/restart/reload are allowed only when needed for the task.\n\
             - Reading DBs and executing read-only SQL queries is allowed.\n\
             - DB mutations are forbidden: INSERT/UPDATE/DELETE/TRUNCATE/ALTER/DROP/CREATE and any SQL/CLI action with side effects.\n\
             - Do not unpack log archives on a remote system; if compressed logs must be read, use streaming read/grep without creating files.\n\
             - Use the minimum necessary changes: before any mutating action, understand the goal, expected effect, and how to roll back or verify the result.\n\
             - Non-operational and system-destructive actions are forbidden: installing/removing OS packages or utilities, changing OS settings, users, firewall/network/systemd outside the application, clearing data without an explicit command, and destructive shell/git operations.\n\
             - Actions outside the explicitly user-specified target system are forbidden.\n\
             - List all mutating actions in outbox: command/request, target, time, result, rollback, or reason rollback is unnecessary.\n\
             \n"
        }
    }
}

/// Render the executor initial prompt template.
fn render_executor_initial(v: &TemplateValues) -> String {
    let notion_protocol_path = notion_protocol_path();
    let gitlab_protocol_path = gitlab_protocol_path();
    let run_paths_section = run_paths_section(v);
    let plan_investigation_rule = plan_investigation_rule(v.workflow_type);
    let implement_plan_rule = implement_plan_rule(v.workflow_type);
    let existing_artifact_refine_rule = existing_artifact_refine_rule(v.workflow_type);
    let executor_breakage_block_rule = executor_breakage_block_rule(v.workflow_type);
    let executor_plan_test_coverage_rule = executor_plan_test_coverage_rule(v.workflow_type);
    let runlim_rule = v.runlim_rule.as_deref().unwrap_or("");
    let artifact_structure_name = artifact_structure_name(v.workflow_type);
    let artifact_structure_items = artifact_structure_items(v.workflow_type);
    let remote_network_policy_section = remote_network_policy_section(v.remote_network_policy);
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
         Required rules:\n\
         - Read and follow `{notion_protocol_path}` as the mandatory Notion protocol.\n\
         - Read and follow `{gitlab_protocol_path}` as the mandatory GitLab protocol.\n\
         - Read inbox by absolute path: `{inbox_path}`.\n\
         - Before the final response, clear outbox at absolute path `{outbox_path}` in-place via truncate and write the current round summary there.\n\
         - If you are OpenCode: first read `{outbox_path}` with the Read tool, then clear the file in-place via truncate, then write the summary; do not delete `outbox.txt`.\n\
         - Do not use `.agent-io` from other directories.\n\
         - Before writing outbox, verify that `{outbox_path}` starts with `workspace_root` (`{repo}`). If the outbox path is not inside `workspace_root`, stop and write an error to the correct `{outbox_path}`.\n\
         - Do not commit transport files: `{inbox_path}` and `{outbox_path}`.\n\
         - Do not push commits without a direct user command.\n\
         - Write user-facing artifacts in English (`PLAN.md`, `INVESTIGATION.md`, executor outbox, reviewer YAML: `rationale`, `feedback_for_executor`, `checks_performed`, `findings`, `verification_commands`, `blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`). Technical terms, commands, API names, errors, and tool quotes may remain in their original language when appropriate.\n\
         - Add/edit project code comments only in English.\n\
         - If you change git-tracked files in workspace_root (including `PLAN.md`/`INVESTIGATION.md`), create a local commit.\n\
         - If `Cargo.lock` changed, it must be included in the local commit.\n\
         - In outbox, include the commit hash and verification commands run.\n\
         {executor_rust_format_rule}\
         - If you add/change tests, run the new/changed tests and the relevant module/crate-level scope.\n\
         - Placeholder tests are not valid tests.\n\
         - Report any unfinished work, skipped checks, or failures in outbox.\n\
         - Do not run destructive git operations without a direct user command.\n\
         {existing_artifact_refine_rule}\
         {plan_investigation_rule}\
         {implement_plan_rule}\
        {executor_breakage_block_rule}\
        {executor_plan_test_coverage_rule}\
        {proptest_test_rule}\
        {runlim_rule}\
         \n\
         Outbox format:\n\
         - For this role, outbox is free-form.\n\
         - Write enough detail for the reviewer to verify the result without guessing.\n\
         \n\
         When finished, write the summary to outbox at absolute path `{outbox_path}` and stop.",
        wf = v.workflow_type,
        repo = v.workspace_root,
        branch = v.branch,
        notion_policy = v.notion_policy,
        run_paths_section = run_paths_section,
        inbox_path = v.inbox_path,
        outbox_path = v.outbox_path,
        notion_protocol_path = notion_protocol_path,
        gitlab_protocol_path = gitlab_protocol_path,
        remote_network_policy_section = remote_network_policy_section,
        artifact_structure_section = artifact_structure_section,
        existing_artifact_refine_rule = existing_artifact_refine_rule,
        plan_investigation_rule = plan_investigation_rule,
        implement_plan_rule = implement_plan_rule,
        executor_breakage_block_rule = executor_breakage_block_rule,
        executor_plan_test_coverage_rule = executor_plan_test_coverage_rule,
        executor_rust_format_rule = EXECUTOR_RUST_FORMAT_RULE,
        proptest_test_rule = PROPTEST_TEST_RULE,
        runlim_rule = runlim_rule,
        prompt = v.user_prompt,
        contract = v.workflow_contract,
    )
}

/// Render the reviewer review prompt template.
fn render_reviewer_review(v: &TemplateValues) -> String {
    let notion_protocol_path = notion_protocol_path();
    let gitlab_protocol_path = gitlab_protocol_path();
    let run_paths_section = run_paths_section(v);
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
            "Artifact structure (for verification):\n\
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
    let runlim_rule = v.runlim_rule.as_deref().unwrap_or("");

    let outbox_section = if v.executor_outbox_present {
        format!(
            "Executor result:\n\
             - The executor completed the round and wrote a free-form text report to `{outbox_path}`.\n\
             - Read `{outbox_path}` yourself before verification.\n\
             - If you are OpenCode: read `{outbox_path}` with the Read tool before any truncate/write.\n\
             - Do not expect a strict format in executor outbox.\n\
             - After reading executor outbox and completing verification, clear `{outbox_path}` in-place and write only your YAML there.",
            outbox_path = v.outbox_path,
        )
    } else {
        format!(
            "Executor result:\n\
             - The executor completed the round in soft-success mode: outbox.txt is missing or empty.\n\
             - Do not try to read outbox.txt as an executor report source; it has no data.\n\
             - Verify the result only from git facts and required artifact presence in the workspace.\n\
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
         \n\
         Executor data:\n\
         {outbox_section}\n\
         \n\
         Git facts:\n\
         {git_facts}\n\
         \n\
         {remote_network_policy_section}\
         Your task:\n\
         - Independently verify the executor result.\n\
         - Do not trust executor self-report without verification.\n\
         - Do not limit yourself to what the executor mentioned: independently reconstruct the expected scope from the user prompt, workflow contract, and required input artifacts.\n\
         - Check whether important questions, code paths, files, sources, edge cases, risks, tests, or alternative explanations/solutions were missed.\n\
         - If the executor omitted an important area that should have been checked from prompt/context, that is a finding; `decision=accept` is forbidden if the omission is material.\n\
         {reviewer_workflow_coverage_rule}\
         - Always verify code and commit changes (diff/files/content), not only outbox and test runs.\n\
         - Always inspect every changed file from the executor commit(s); selective file review is not allowed.\n\
         - If `Cargo.lock` changed in the workspace, verify that it is included in the local commit; uncommitted `Cargo.lock` forbids `decision=accept`.\n\
         - Perform review in two independent loops:\n\
         - Loop A (compliance): for workflow `implement`, if `PLAN.md` is present, verifying implementation against PLAN.md items is mandatory; if `PLAN.md` is absent, verifying implementation against the user prompt and workflow contract is mandatory.\n\
         - Loop B (independent code review): separately from PLAN.md/user prompt, verify the quality and correctness of the code changes themselves: logic, regressions, architectural risks, extra out-of-scope changes.\n\
         - Passing tests are required, but not sufficient by themselves for `decision=accept` without confirmed correctness of the code changes.\n\
         {reviewer_format_check_rule}\
         - Do not modify git-tracked workspace_root files, workflow artifacts, git index, commits, branches, or provider metadata.\n\
         - The only file you must modify is `{outbox_path}`, where you write the final YAML.\n\
         - Do not use `.agent-io` from other directories.\n\
         - Before writing outbox, verify that `{outbox_path}` starts with `workspace_root` (`{repo}`). If the outbox path is not inside `workspace_root`, stop and write an error to the correct `{outbox_path}`.\n\
         - Write user-facing artifacts in English (`PLAN.md`, `INVESTIGATION.md`, executor outbox, reviewer YAML: `rationale`, `feedback_for_executor`, `checks_performed`, `findings`, `verification_commands`, `blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`). Technical terms, commands, API names, errors, and tool quotes may remain in their original language when appropriate.\n\
         {proptest_test_rule}\
         {runlim_rule}\
         - If executor outbox clearly shows that the executor is answering the wrong repo/request/thread or using the wrong transport path, return `decision: poisoned_session` and fill `poisoned_session_reason`.\n\
         - Verify workflow contract compliance.\n\
         - Verify git facts and required artifact presence in the workspace.\n\
         - Verify remote access policy compliance. If the executor performed a forbidden remote action, return `decision: blocked`, `hard_blockers_present: true`, fill `blocking_reason` with the violation description; do not return `revise`.\n\
         - For remote access policy violations, include in `findings`: what action was performed, why it is forbidden by the current policy, and what traces/risks a human should check.\n\
         - If a remote access policy violation occurred, do not ask the executor to continue investigation or perform compensating actions without a direct new user command.\n\
         - Separately check compatibility and regression risks: API/contracts, behavior, data/DB, serialization/deserialization, integration assumptions.\n\
         - A `What could break` section is required: list potential regressions and impact; if no risks were found, explicitly state what was checked and why risk is low.\n\
         - Separately state what was done, not done, partially/controversially done, and what extra out-of-scope changes were made.\n\
         {reviewer_investigate_independent_validation_rule}\
         - If the user prompt mentions a Notion task, separately state in the review which Notion commands/evidence you independently checked (not only executor claims).\n\
         - First independently assign quality_score from 0 to 10 and set contract_satisfied/hard_blockers_present based on facts.\n\
         - Set `contract_satisfied=true` only if mandatory workflow contract conditions are satisfied by verifiable facts (artifact/git/notion policy).\n\
         - Set `hard_blockers_present=true` if there is at least one external/critical blocker that prevents successful completion of the round.\n\
         - `decision=accept` is forbidden if there are unmet mandatory requirements, unverified key claims, or any serious finding (`major|high|serious|critical`).\n\
         - If there is at least one serious/critical issue (`major|high|serious|critical`), `hard_blockers_present` must be `true`.\n\
         {reviewer_artifact_compliance_requirements}\
         - Then choose decision based on the already assigned score and facts.\n\
         - Return decision: `accept`, `revise`, `blocked`, `irreconcilable_disagreement`, or `poisoned_session`.\n\
         - The orchestrator applies thresholds and gate rules automatically; do not tune score to the desired decision.\n\
         - Every material finding must be evidence-based and actionable: where (file/line), what is wrong, why it is a risk, and the minimal fix.\n\
         - If there are no findings, explicitly state `no findings` and briefly list what exactly was checked.\n\
         - Vague wording without verifiable support is forbidden.\n\
         - If fixes are needed, fill `feedback_for_executor` with concrete items.\n\
         \n\
         Strict outbox format (`{outbox_path}`):\n\
         - Write exactly one YAML document to `{outbox_path}`.\n\
         - Do not add Markdown fences.\n\
         - Do not add text before or after YAML.\n\
         - Do not add YAML comments: the parser ignores comments, they are not part of the protocol and will not reach the orchestrator report.\n\
         - Do not add anchors, aliases, custom tags, or a second YAML document.\n\
         - Mandatory strict fields: `decision`, `quality_score`, `rationale`, `contract_satisfied`, `hard_blockers_present`, `notion_requirements_satisfied`, `feedback_for_executor`, `blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`.\n\
         - Fields `checks_performed`, `findings`, `verification_commands` are free-form in structure.\n\
         - For decision/status/category/name values, use snake_case ASCII.\n\
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
        proptest_test_rule = PROPTEST_TEST_RULE,
        runlim_rule = runlim_rule,
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
         Your only task now is to fix only the YAML protocol and overwrite `{outbox_path}`.\n\
         Do not reconsider the review content unless required to fix the schema.\n\
         Do not change the Notion protocol: it is defined in `{notion_protocol_path}`.\n\
         Do not change the GitLab protocol: it is defined in `{gitlab_protocol_path}`.\n\
         Do not use `.agent-io` from other directories.\n\
         Before writing outbox, verify that `{outbox_path}` starts with `workspace_root` (`{repo}`). If the outbox path is not inside `workspace_root`, stop and write an error to the correct `{outbox_path}`.\n\
         - Write user-facing artifacts in English (`PLAN.md`, `INVESTIGATION.md`, executor outbox, reviewer YAML: `rationale`, `feedback_for_executor`, `checks_performed`, `findings`, `verification_commands`, `blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`). Technical terms, commands, API names, errors, and tool quotes may remain in their original language when appropriate.\n\
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
         notion_policy for this run: `{notion_policy}`\n\
         \n\
         Use this minimal schema:\n\
         \n\
         {schema}\n\
         \n\
         After fixing it, stop immediately.",
        rejection = rejection,
        notion_policy = v.notion_policy,
        run_paths_section = run_paths_section,
        outbox_path = v.outbox_path,
        repo = v.workspace_root,
        notion_protocol_path = notion_protocol_path,
        gitlab_protocol_path = gitlab_protocol_path,
        schema = schema,
    )
}

/// Render the executor feedback prompt template.
fn render_executor_feedback(v: &TemplateValues) -> String {
    let run_paths_section = run_paths_section(v);
    let executor_breakage_block_rule = executor_breakage_block_rule(v.workflow_type);
    let executor_plan_test_coverage_rule = executor_plan_test_coverage_rule(v.workflow_type);
    let runlim_rule = v.runlim_rule.as_deref().unwrap_or("");
    let artifact_structure_name = artifact_structure_name(v.workflow_type);
    let artifact_structure_items = artifact_structure_items(v.workflow_type);
    let remote_network_policy_section = remote_network_policy_section(v.remote_network_policy);
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
         Reviewer YAML result:\n\
         {review_result_yaml}\n\
         \n\
         Feedback for you:\n\
         {feedback_for_executor}\n\
         \n\
         {remote_network_policy_section}\
         Interaction rules:\n\
         - Treat feedback as peer review.\n\
         - Do not execute reviewer feedback blindly: first independently evaluate each item, verify facts, and check applicability to the current repo/request.\n\
         - Fix only the feedback items you agree with after verification.\n\
         - If you agree with a feedback item, update artifacts/code/plan according to the feedback.\n\
         - If you disagree with a feedback item, do not make that fix; explicitly explain why in outbox and support it with facts.\n\
         - In outbox, provide status for every feedback item: accepted_fixed, accepted_not_done, or rejected_with_reason.\n\
         - Do not ignore major/critical findings.\n\
         \n\
         Required rules:\n\
         - Read inbox by absolute path: `{inbox_path}`.\n\
         - Before the final response, clear outbox at absolute path `{outbox_path}` in-place via truncate and write the current round summary there.\n\
         - If you are OpenCode: first read `{outbox_path}` with the Read tool, then clear the file in-place via truncate, then write the summary; do not delete `outbox.txt`.\n\
         - Do not use `.agent-io` from other directories.\n\
         - Before writing outbox, verify that `{outbox_path}` starts with `workspace_root` (`{repo}`). If the outbox path is not inside `workspace_root`, stop and write an error to the correct `{outbox_path}`.\n\
         - Do not commit transport files: `{inbox_path}` and `{outbox_path}`.\n\
         - Do not push commits without a direct user command.\n\
         - Write user-facing artifacts in English (`PLAN.md`, `INVESTIGATION.md`, executor outbox, reviewer YAML: `rationale`, `feedback_for_executor`, `checks_performed`, `findings`, `verification_commands`, `blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`). Technical terms, commands, API names, errors, and tool quotes may remain in their original language when appropriate.\n\
         - Add/edit project code comments only in English.\n\
         - If you change git-tracked files in workspace_root (including `PLAN.md`/`INVESTIGATION.md`), create a local commit.\n\
         - If `Cargo.lock` changed, it must be included in the local commit.\n\
         - In outbox, include the commit hash and verification commands run.\n\
         {executor_rust_format_rule}\
         - If you add/change tests, run the new/changed tests and the relevant module/crate-level scope.\n\
         - Placeholder tests are not valid tests.\n\
         - Report any unfinished work, skipped checks, or failures in outbox.\n\
         - Do not run destructive git operations without a direct user command.\n\
        {executor_breakage_block_rule}\
        {executor_plan_test_coverage_rule}\
        {proptest_test_rule}\
        {runlim_rule}\
         \n\
         Outbox format:\n\
         - For this role, outbox is free-form.\n\
         - List every feedback item and its status: what was fixed, what was accepted but not completed, and what you disagree with and why.\n\
         \n\
         When finished, write the summary to outbox at absolute path `{outbox_path}` and stop.",
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
        executor_breakage_block_rule = executor_breakage_block_rule,
        executor_plan_test_coverage_rule = executor_plan_test_coverage_rule,
        executor_rust_format_rule = EXECUTOR_RUST_FORMAT_RULE,
        proptest_test_rule = PROPTEST_TEST_RULE,
        runlim_rule = runlim_rule,
    )
}

/// Render one of the three fixed prompt templates into a String.
pub fn render_template(id: TemplateId, values: &TemplateValues) -> String {
    match id {
        TemplateId::ExecutorInitial => render_executor_initial(values),
        TemplateId::ReviewerReview => render_reviewer_review(values),
        TemplateId::ReviewerRepairYaml => render_reviewer_repair_yaml(values),
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
            review_result_yaml: None,
            feedback_for_executor: None,
            runlim_rule: None,
        }
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
        assert!(out.contains("YAML_REJECTION"));
        assert!(out.contains("STRICT_FIELDS_CHECKLIST"));
        assert!(out.contains("bad decision"));
    }

    #[test]
    fn reviewer_prompt_outbox_present_mentions_read() {
        let v = sample_values(WorkflowType::Implement);
        let out = render_template(TemplateId::ReviewerReview, &v);
        assert!(out.contains("Read `/repo/.agent-io/outbox.txt` yourself before verification"));
        assert!(!out.contains("soft-success"));
    }

    #[test]
    fn reviewer_prompt_soft_success_no_outbox_read_instruction() {
        let mut v = sample_values(WorkflowType::Implement);
        v.executor_outbox_present = false;
        let out = render_template(TemplateId::ReviewerReview, &v);
        assert!(out.contains("soft-success"));
        assert!(!out.contains("Read `/repo/.agent-io/outbox.txt` yourself before verification"));
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
            assert!(out.contains("Run paths:"));
            assert!(out.contains("transport_dir: `/repo/.agent-io`"));
            assert!(out.contains("inbox_path: `/repo/.agent-io/inbox.txt`"));
            assert!(out.contains("outbox_path: `/repo/.agent-io/outbox.txt`"));
            assert!(out.contains("orchestrator_docs_dir: `/orchestrator/docs`"));
            assert!(
                out.contains("do not use it as a workspace and do not look for `.agent-io` there")
            );
            assert!(out.contains("Do not use `.agent-io` from other directories"));
            assert!(out
                .contains("verify that `/repo/.agent-io/outbox.txt` starts with `workspace_root`"));
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
            assert!(out.contains("Technical terms, commands, API names, errors"));
            assert!(out.contains("may remain in their original language when appropriate"));
        }
    }

    #[test]
    fn normal_prompts_include_runlim_rule_when_available() {
        let mut v = sample_values(WorkflowType::Implement);
        v.runlim_rule = Some(
            "- If you run `cargo run` (running the built project / binary) or `cargo test`, use `runlim`, for example: `runlim cargo run ...` or `runlim cargo test ...`. If `runlim` is unavailable in the current shell, run normally.\n"
                .to_owned(),
        );

        let initial = render_template(TemplateId::ExecutorInitial, &v);
        let feedback = render_template(TemplateId::ExecutorFeedback, &v);
        let review = render_template(TemplateId::ReviewerReview, &v);
        assert!(initial.contains("use `runlim`"));
        assert!(initial.contains("`runlim cargo run ...`"));
        assert!(initial.contains("`runlim cargo test ...`"));
        assert!(initial.contains("run normally"));
        assert!(!initial.contains("bash -ic"));
        assert!(feedback.contains("use `runlim`"));
        assert!(feedback.contains("`runlim cargo run ...`"));
        assert!(feedback.contains("`runlim cargo test ...`"));
        assert!(feedback.contains("run normally"));
        assert!(!feedback.contains("bash -ic"));
        assert!(review.contains("use `runlim`"));
        assert!(review.contains("`runlim cargo run ...`"));
        assert!(review.contains("`runlim cargo test ...`"));
        assert!(review.contains("run normally"));
        assert!(!review.contains("bash -ic"));
    }

    #[test]
    fn normal_prompts_include_proptest_persistence_rule() {
        let v = sample_values(WorkflowType::Implement);

        for template in [
            TemplateId::ExecutorInitial,
            TemplateId::ExecutorFeedback,
            TemplateId::ReviewerReview,
        ] {
            let out = render_template(template, &v);
            assert!(out.contains("PROPTEST_DISABLE_FAILURE_PERSISTENCE=1 cargo test"));
            assert!(out.contains("PROPTEST_DISABLE_FAILURE_PERSISTENCE=1 runlim cargo test"));
            assert!(out.contains("`*.proptest-regressions`"));
        }

        let repair = render_template(TemplateId::ReviewerRepairYaml, &v);
        assert!(!repair.contains("PROPTEST_DISABLE_FAILURE_PERSISTENCE"));
    }

    #[test]
    fn executor_prompts_require_mutating_format_before_commit() {
        let v = sample_values(WorkflowType::Implement);

        for template in [TemplateId::ExecutorInitial, TemplateId::ExecutorFeedback] {
            let out = render_template(template, &v);
            assert!(out.contains("mutating formatter: `cargo fmt --all`"));
            assert!(out.contains("Do not replace `cargo fmt --all` with `cargo fmt --all --check`"));
            assert!(out.contains("format its own changes before commit"));
        }
    }

    #[test]
    fn reviewer_prompt_forbids_mutating_formatters() {
        let v = sample_values(WorkflowType::Implement);
        let review = render_template(TemplateId::ReviewerReview, &v);

        assert!(review.contains("The reviewer must not run mutating formatters/fixers"));
        assert!(review.contains("`cargo fmt`, `cargo fmt --all`, `cargo fix`"));
        assert!(review.contains("`cargo fmt --all --check`"));
        assert!(review.contains("do not fix formatting yourself"));
    }

    #[test]
    fn reviewer_repair_prompt_does_not_include_runlim_rule() {
        let mut v = sample_values(WorkflowType::Implement);
        v.runlim_rule = Some("- If you run `cargo run`, use `runlim`.\n".to_owned());

        let repair = render_template(TemplateId::ReviewerRepairYaml, &v);
        assert!(!repair.contains("runlim"));
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

        assert!(out.contains("Do not execute reviewer feedback blindly"));
        assert!(out.contains("Fix only the feedback items you agree with after verification"));
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
            assert!(out.contains("Findings"));
            assert!(out.contains("Conclusions"));
            assert!(!out.contains("- Symptom\n"));
            assert!(!out.contains("Most likely root cause"));
        }
    }

    #[test]
    fn reviewer_prompt_requires_checking_omitted_scope() {
        let v = sample_values(WorkflowType::Plan);
        let out = render_template(TemplateId::ReviewerReview, &v);

        assert!(out.contains("Do not limit yourself to what the executor mentioned"));
        assert!(out.contains("independently reconstruct the expected scope"));
        assert!(out.contains("important questions"));
        assert!(out.contains("`decision=accept` is forbidden if the omission is material"));
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

        assert!(investigate.contains("independently derive research questions"));
        assert!(plan.contains("covers the user prompt and `INVESTIGATION.md`"));
        assert!(implement.contains("inspect relevant adjacent call sites"));
    }

    #[test]
    fn forbidden_remote_network_policy_adds_restrictions_section() {
        let v = sample_values(WorkflowType::Implement);
        let out = render_template(TemplateId::ExecutorInitial, &v);
        assert!(out.contains("Remote access policy:"));
        assert!(out.contains("Do not access remote target systems over SSH."));
        assert!(out.contains("Do not make HTTP requests to remote target systems."));
        assert!(!out.contains("internal product name"));
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
        assert!(!out.contains("internal product name"));
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
        assert!(!out.contains("internal product name"));
    }

    #[test]
    fn reviewer_prompt_requires_remote_access_policy_check() {
        let v = sample_values(WorkflowType::Implement);
        let out = render_template(TemplateId::ReviewerReview, &v);
        assert!(out.contains("Verify remote access policy compliance"));
        assert!(out.contains("return `decision: blocked`"));
        assert!(out.contains("hard_blockers_present: true"));
        assert!(out.contains("do not return `revise`"));
        assert!(out.contains("do not ask the executor to continue investigation"));
    }

    #[test]
    fn remote_access_policy_is_top_level_prompt_section() {
        let v = sample_values(WorkflowType::Implement);
        let executor = render_template(TemplateId::ExecutorInitial, &v);
        let reviewer = render_template(TemplateId::ReviewerReview, &v);
        let feedback = render_template(TemplateId::ExecutorFeedback, &v);

        assert!(executor.contains("Remote access policy:\n"));
        assert!(executor.contains("local CLIs.\n\nRequired rules:"));
        assert!(reviewer.contains("local CLIs.\n\nYour task:"));
        assert!(feedback.contains("local CLIs.\n\nInteraction rules:"));
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
