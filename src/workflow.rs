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
            "- If `INVESTIGATION.md` already exists, read it first and refine or correct it as the current artifact instead of ignoring it.\n"
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
             - Symptom\n\
             - Expected behavior\n\
             - Evidence\n\
             - Timeline\n\
             - Ruled out hypotheses\n\
             - Main hypotheses\n\
             - Most likely root cause\n\
             - Suspect commits (if any)\n\
             - Testing implications\n\
             - Fix directions\n\
             - Open questions\n"
        }
        WorkflowType::Implement => "",
    }
}

fn reviewer_artifact_compliance_requirements(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Plan => {
            "- For workflow `plan` (track A), verify the required `PLAN.md` structure:\n\
             - Check that all required sections exist, are substantive (not placeholders), and are coherent: Affected components -> Implementation steps -> Testing strategy.\n\
             - `Testing strategy` must plan automated testing (unit/integration/e2e where applicable), not only manual checks.\n\
             - Manual checks are allowed only as fallback and only with explicit justification for why automated testing is objectively impractical in the current context (clear gap + reason).\n"
        }
        WorkflowType::Investigate => {
            "- For workflow `investigate` (track A), verify the required `INVESTIGATION.md` structure:\n\
             - Check that all required sections exist, are substantive (not placeholders), and are coherent: Evidence -> Main hypotheses -> Most likely root cause.\n"
        }
        WorkflowType::Implement => "",
    }
}

fn reviewer_investigate_independent_validation_rule(wf: WorkflowType) -> &'static str {
    if wf == WorkflowType::Investigate {
        "- For workflow `investigate`, independently verify every key hypothesis and evidence item from `INVESTIGATION.md` against primary sources (code, git history, logs, commands), not only against the artifact text.\n\
         - Do not confirm a hypothesis without verifiable evidence; if it could not be verified, mark that explicitly as a gap/uncertainty in findings.\n"
    } else {
        ""
    }
}

fn executor_breakage_block_rule(wf: WorkflowType) -> &'static str {
    match wf {
        WorkflowType::Plan | WorkflowType::Implement => {
            "- A `What Could Break` section is required: list potential regressions/risks after the changes (behavior, API/contracts, data/DB, integrations, performance/resources) and how to verify them.\n"
        }
        WorkflowType::Investigate => "",
    }
}

fn executor_plan_test_coverage_rule(wf: WorkflowType) -> &'static str {
    if wf == WorkflowType::Plan {
        "- For workflow `plan`, plan automated tests (unit/integration/e2e where applicable), not only manual checks.\n\
         - Manual checks are allowed only as fallback when automated testing is objectively impractical in the current context.\n\
         - For the automated-test plan, cover happy-path + negative-path + edge cases within reason; if something cannot be covered by automated tests, explicitly record the gap and reason.\n"
    } else {
        ""
    }
}

fn remote_network_restrictions_line(policy: RemoteNetworkPolicy) -> &'static str {
    if policy == RemoteNetworkPolicy::Forbidden {
        "- Do not access remote production systems over SSH. Do not make HTTP requests to remote production systems."
    } else {
        ""
    }
}

/// Render the executor initial prompt template.
fn render_executor_initial(v: &TemplateValues) -> String {
    let notion_protocol_path = notion_protocol_path();
    let gitlab_protocol_path = gitlab_protocol_path();
    let plan_investigation_rule = plan_investigation_rule(v.workflow_type);
    let implement_plan_rule = implement_plan_rule(v.workflow_type);
    let existing_artifact_refine_rule = existing_artifact_refine_rule(v.workflow_type);
    let executor_breakage_block_rule = executor_breakage_block_rule(v.workflow_type);
    let executor_plan_test_coverage_rule = executor_plan_test_coverage_rule(v.workflow_type);
    let artifact_structure_name = artifact_structure_name(v.workflow_type);
    let artifact_structure_items = artifact_structure_items(v.workflow_type);
    let remote_network_restrictions = remote_network_restrictions_line(v.remote_network_policy);
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
         User prompt:\n\
         {prompt}\n\
         \n\
         Workflow contract:\n\
         {contract}\n\
         \n\
         {artifact_structure_section}\
         Mandatory rules:\n\
         - Read and follow `{notion_protocol_path}` as the mandatory Notion protocol.\n\
         - Read and follow `{gitlab_protocol_path}` as the mandatory GitLab protocol.\n\
         - Read `./.agent-io/inbox.txt` as the task source.\n\
         - Before the final response, clear `./.agent-io/outbox.txt` in place via truncate and write the current round result there.\n\
         - If you are OpenCode: first read `./.agent-io/outbox.txt` with the Read tool, then clear it in place via truncate, then write the result; do not delete outbox.\n\
         - Do not commit `./.agent-io/inbox.txt` or `./.agent-io/outbox.txt`.\n\
         - Do not push commits without a direct user command.\n\
         - Write user-facing artifacts and outbox in English.\n\
         - Add/edit project code comments only in English.\n\
         - If you change git-tracked files in workspace_root (including `PLAN.md`/`INVESTIGATION.md`), create a local commit.\n\
         - If `Cargo.lock` changed, it must be included in the local commit.\n\
         - Report the commit hash and executed verification commands in outbox.\n\
         - For Rust changes, run `cargo fmt` and `make clippy` or a repo-approved equivalent before commit.\n\
         - If you add/change tests, run the new/changed tests and the relevant module/crate-level scope.\n\
         - Placeholder tests are not valid tests.\n\
         - Report any incomplete work, skipped checks, or failures in outbox.\n\
         - Do not run destructive git operations without a direct user command.\n\
         {remote_network_restrictions}\n\
         {existing_artifact_refine_rule}\
         {plan_investigation_rule}\
         {implement_plan_rule}\
         {executor_breakage_block_rule}\
         {executor_plan_test_coverage_rule}\
         \n\
         Outbox format:\n\
         - For this role, outbox is free-form.\n\
         - Write enough detail for the reviewer to verify the result without guessing.\n\
         \n\
         When done, write the result to `./.agent-io/outbox.txt` and stop.",
        wf = v.workflow_type,
        repo = v.workspace_root,
        branch = v.branch,
        notion_policy = v.notion_policy,
        notion_protocol_path = notion_protocol_path,
        gitlab_protocol_path = gitlab_protocol_path,
        remote_network_restrictions = remote_network_restrictions,
        artifact_structure_section = artifact_structure_section,
        existing_artifact_refine_rule = existing_artifact_refine_rule,
        plan_investigation_rule = plan_investigation_rule,
        implement_plan_rule = implement_plan_rule,
        executor_breakage_block_rule = executor_breakage_block_rule,
        executor_plan_test_coverage_rule = executor_plan_test_coverage_rule,
        prompt = v.user_prompt,
        contract = v.workflow_contract,
    )
}

/// Render the reviewer review prompt template.
fn render_reviewer_review(v: &TemplateValues) -> String {
    let notion_protocol_path = notion_protocol_path();
    let gitlab_protocol_path = gitlab_protocol_path();
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
    let remote_network_restrictions = remote_network_restrictions_line(v.remote_network_policy);

    let outbox_section = if v.executor_outbox_present {
        "Executor result:\n\
         - The executor completed the round and wrote a free-form report to `./.agent-io/outbox.txt`.\n\
         - Read `./.agent-io/outbox.txt` yourself before verification.\n\
         - If you are OpenCode: read `./.agent-io/outbox.txt` with the Read tool before any truncate/write.\n\
         - Do not expect strict format in executor outbox.\n\
         - After reading executor outbox and completing verification, clear `./.agent-io/outbox.txt` in place and write only your YAML there."
    } else {
        "Executor result:\n\
         - The executor completed the round in soft-success mode: outbox.txt is missing or empty.\n\
         - Do not try to read outbox.txt as the executor report source; it has no data.\n\
         - Verify the result only from git facts and required artifact presence in the workspace.\n\
         - Write your YAML directly to `./.agent-io/outbox.txt` (clear in place via truncate, then write)."
    };

    format!(
        "You are the reviewer agent in workflow `{wf}`.\n\
         \n\
         Run context:\n\
         - workspace_root: `{repo}`\n\
         - branch: `{branch}`\n\
         - notion_policy: `{notion_policy}`\n\
         \n\
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
         Your task:\n\
         - Independently verify the executor result.\n\
         - Do not trust the executor self-report without verification.\n\
         - Verify code and commit changes (diff/files/content), not only outbox and test runs.\n\
         - Verify every changed file from executor commit(s); selective file review is not allowed.\n\
         - If `Cargo.lock` changed in the workspace, verify that it is included in the local commit; uncommitted `Cargo.lock` forbids `decision=accept`.\n\
         - Run review in two independent tracks:\n\
         - Track A (compliance): for workflow `implement`, if `PLAN.md` is present, verify implementation against PLAN.md; if `PLAN.md` is absent, verify implementation against user prompt and workflow contract.\n\
         - Track B (independent code review): separately from PLAN.md/user prompt, verify the quality and correctness of code changes: logic, regressions, architecture risks, and out-of-scope changes.\n\
         - Passing tests are required but are not sufficient for `decision=accept` without confirmed correctness of code changes.\n\
         - Do not modify git-tracked files in workspace_root, workflow artifacts, git index, commits, branches, or provider metadata.\n\
         {remote_network_restrictions}\n\
         - The only file you must change is `./.agent-io/outbox.txt`, where you write the final YAML.\n\
         - If executor outbox clearly shows that the executor is answering a different repo/request/thread or using the wrong transport path, return `decision: poisoned_session` and fill `poisoned_session_reason`.\n\
         - Verify workflow contract compliance.\n\
         - Verify git facts and required artifact presence in the workspace.\n\
         - Separately verify compatibility and regression risks: API/contracts, behavior, data/DB, serialization/deserialization, and integration assumptions.\n\
         - A `What Could Break` section is required: list potential regressions and their impact; if no risks were found, explicitly state what was checked and why risk is low.\n\
         - Separately state what was done, what was not done, what is partial/disputed, and what was changed unnecessarily out of scope.\n\
         {reviewer_investigate_independent_validation_rule}\
         - If the user prompt mentions a Notion task, separately state in the review which Notion commands/evidence you independently checked (not only executor claims).\n\
         - First independently assign quality_score from 0 to 10 and set contract_satisfied/hard_blockers_present based on facts.\n\
         - Set `contract_satisfied=true` only if mandatory workflow contract conditions are satisfied by verifiable facts (artifact/git/notion policy).\n\
         - Set `hard_blockers_present=true` if there is at least one external/critical blocker that prevents successful round completion.\n\
         - `decision=accept` is forbidden if mandatory requirements are unmet, key claims are unverified, or any serious finding exists (`major|high|serious|critical`).\n\
         - If at least one serious/critical issue exists (`major|high|serious|critical`), `hard_blockers_present` must be `true`.\n\
         {reviewer_artifact_compliance_requirements}\
         - Then choose decision based on the already assigned score and facts.\n\
         - Return decision: `accept`, `revise`, `blocked`, `irreconcilable_disagreement`, or `poisoned_session`.\n\
         - The orchestrator applies thresholds and gate rules automatically; do not tune score to fit a desired decision.\n\
         - Every substantial finding must be evidence-based and actionable: where (file/line), what is wrong, why it is risky, and how to fix it minimally.\n\
         - If there are no findings, explicitly state `no findings` and briefly list what was checked.\n\
         - Vague claims (for example, `looks OK`, `seems fine`, `probably correct`) are forbidden without verifiable support.\n\
         - If fixes are needed, fill `feedback_for_executor` with concrete items.\n\
         \n\
         Strict `./.agent-io/outbox.txt` format:\n\
         - Write exactly one YAML document to `./.agent-io/outbox.txt`.\n\
         - Do not add Markdown fences.\n\
         - Do not add text before or after YAML.\n\
         - Do not add YAML comments: the parser ignores comments, they are not part of the protocol, and they will not enter the orchestrator report.\n\
         - Do not add anchors, aliases, custom tags, or a second YAML document.\n\
         - Mandatory strict fields: `decision`, `quality_score`, `rationale`, `contract_satisfied`, `hard_blockers_present`, `notion_requirements_satisfied`, `feedback_for_executor`, `blocking_reason`, `irreconcilable_reason`, `poisoned_session_reason`.\n\
         - Fields `checks_performed`, `findings`, and `verification_commands` are free-form structurally.\n\
         - Use snake_case ASCII for decision/status/category/name values.\n\
         - Use this minimal schema:\n\
         \n\
         {schema}\n\
         \n\
         When done, write YAML to `./.agent-io/outbox.txt` and stop.",
        wf = v.workflow_type,
        repo = v.workspace_root,
        branch = v.branch,
        notion_policy = v.notion_policy,
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
        remote_network_restrictions = remote_network_restrictions,
    )
}

/// Render reviewer repair prompt after a rejected YAML attempt.
fn render_reviewer_repair_yaml(v: &TemplateValues) -> String {
    let notion_protocol_path = notion_protocol_path();
    let gitlab_protocol_path = gitlab_protocol_path();
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
         Your only task now: fix only the YAML protocol and overwrite `./.agent-io/outbox.txt`.\n\
         Do not reconsider the review content unless required to fix the schema.\n\
         Do not change the Notion protocol: it is defined in `{notion_protocol_path}`.\n\
         Do not change the GitLab protocol: it is defined in `{gitlab_protocol_path}`.\n\
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
         Use this minimal schema:\n\
         \n\
         {schema}\n\
         \n\
         Stop immediately after fixing it.",
        rejection = rejection,
        notion_policy = v.notion_policy,
        notion_protocol_path = notion_protocol_path,
        gitlab_protocol_path = gitlab_protocol_path,
        schema = schema,
    )
}

/// Render the executor feedback prompt template.
fn render_executor_feedback(v: &TemplateValues) -> String {
    let executor_breakage_block_rule = executor_breakage_block_rule(v.workflow_type);
    let executor_plan_test_coverage_rule = executor_plan_test_coverage_rule(v.workflow_type);
    let artifact_structure_name = artifact_structure_name(v.workflow_type);
    let artifact_structure_items = artifact_structure_items(v.workflow_type);
    let remote_network_restrictions = remote_network_restrictions_line(v.remote_network_policy);
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
         Interaction rules:\n\
         - Treat feedback as peer review.\n\
         - If you agree, update artifacts/code/plan according to the feedback.\n\
         - If you disagree with a feedback item, explain why explicitly in outbox and support it with facts.\n\
         - Do not ignore major/critical findings.\n\
         \n\
         Mandatory rules:\n\
         - Read `./.agent-io/inbox.txt` as the task source.\n\
         - Before the final response, clear `./.agent-io/outbox.txt` in place via truncate and write the current round result there.\n\
         - If you are OpenCode: first read `./.agent-io/outbox.txt` with the Read tool, then clear it in place via truncate, then write the result; do not delete outbox.\n\
         - Do not commit `./.agent-io/inbox.txt` or `./.agent-io/outbox.txt`.\n\
         - Do not push commits without a direct user command.\n\
         - Write user-facing artifacts and outbox in English.\n\
         - Add/edit project code comments only in English.\n\
         - If you change git-tracked files in workspace_root (including `PLAN.md`/`INVESTIGATION.md`), create a local commit.\n\
         - If `Cargo.lock` changed, it must be included in the local commit.\n\
         - Report the commit hash and executed verification commands in outbox.\n\
         - For Rust changes, run `cargo fmt` and `make clippy` or a repo-approved equivalent before commit.\n\
         - If you add/change tests, run the new/changed tests and the relevant module/crate-level scope.\n\
         - Placeholder tests are not valid tests.\n\
         - Report any incomplete work, skipped checks, or failures in outbox.\n\
         - Do not run destructive git operations without a direct user command.\n\
         {remote_network_restrictions}\n\
         {executor_breakage_block_rule}\
         {executor_plan_test_coverage_rule}\
         \n\
         Outbox format:\n\
         - For this role, outbox is free-form.\n\
         - List which feedback items were addressed and which ones you disagree with, with reasoning.\n\
         \n\
         When done, write the result to `./.agent-io/outbox.txt` and stop.",
        wf = v.workflow_type,
        repo = v.workspace_root,
        branch = v.branch,
        notion_policy = v.notion_policy,
        prompt = v.user_prompt,
        contract = v.workflow_contract,
        artifact_structure_section = artifact_structure_section,
        git_facts = v.git_facts,
        review_result_yaml = review_result_yaml,
        feedback_for_executor = feedback_for_executor,
        remote_network_restrictions = remote_network_restrictions,
        executor_breakage_block_rule = executor_breakage_block_rule,
        executor_plan_test_coverage_rule = executor_plan_test_coverage_rule,
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
        assert!(out.contains("Read `./.agent-io/outbox.txt`"));
        assert!(!out.contains("soft-success"));
    }

    #[test]
    fn reviewer_prompt_soft_success_no_outbox_read_instruction() {
        let mut v = sample_values(WorkflowType::Implement);
        v.executor_outbox_present = false;
        let out = render_template(TemplateId::ReviewerReview, &v);
        assert!(out.contains("soft-success"));
        assert!(!out.contains("Read `./.agent-io/outbox.txt` yourself before verification"));
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
    fn forbidden_remote_network_policy_adds_restrictions_line() {
        let v = sample_values(WorkflowType::Implement);
        let out = render_template(TemplateId::ExecutorInitial, &v);
        assert!(out.contains("- Do not access remote production systems over SSH."));
        assert!(out.contains("Do not make HTTP requests to remote production systems."));
    }

    #[test]
    fn allowed_remote_network_policy_omits_restrictions_line() {
        let mut v = sample_values(WorkflowType::Implement);
        v.remote_network_policy = RemoteNetworkPolicy::Allowed;
        let out = render_template(TemplateId::ExecutorInitial, &v);
        assert!(!out.contains("Do not access remote production systems over SSH."));
        assert!(!out.contains("Do not make HTTP requests to remote production systems."));
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
