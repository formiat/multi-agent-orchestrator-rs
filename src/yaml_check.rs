use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::errors::{OrchestratorError, OrchestratorResult};

// ---------------------------------------------------------------------------
// Reviewer YAML schema types
// ---------------------------------------------------------------------------

/// Root YAML document produced by the reviewer agent.
///
/// The top-level field set is strict. Some known payload fields accept free-form YAML values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerYaml {
    pub quality_score: f64,
    pub decision: ReviewDecision,
    pub rationale: String,
    pub contract_satisfied: bool,
    pub hard_blockers_present: bool,
    #[serde(default)]
    pub notion_requirements_satisfied: Option<bool>,
    pub feedback_for_executor: Vec<String>,
    #[serde(default)]
    pub checks_performed: Option<serde_yaml::Value>,
    #[serde(default)]
    pub findings: Option<serde_yaml::Value>,
    #[serde(default)]
    pub verification_commands: Option<serde_yaml::Value>,
    #[serde(default)]
    pub blocking_reason: Option<String>,
    #[serde(default)]
    pub irreconcilable_reason: Option<String>,
    #[serde(default)]
    pub poisoned_session_reason: Option<String>,
}

/// Reviewer decision returned in the YAML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Accept,
    Revise,
    Blocked,
    IrreconcilableDisagreement,
    PoisonedSession,
}

impl ReviewDecision {
    /// Stable snake_case string for use in reports and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Revise => "revise",
            Self::Blocked => "blocked",
            Self::IrreconcilableDisagreement => "irreconcilable_disagreement",
            Self::PoisonedSession => "poisoned_session",
        }
    }
}

// ---------------------------------------------------------------------------
// Pre-scan and parse
// ---------------------------------------------------------------------------

/// Perform structural pre-scan of raw reviewer YAML before deserialization.
///
/// Rejects: Markdown fences, YAML anchors, aliases, tags, multiple documents.
///
/// Note: yaml-rust2's scanner module is not public in 0.9.x, so detection uses regex and
/// line-level checks. `serde_yaml::from_str` performs the final parse and schema validation.
pub fn prescan_reviewer_yaml(raw: &str) -> OrchestratorResult<()> {
    fn err(msg: &str) -> OrchestratorError {
        OrchestratorError::ArtifactContract {
            contract: msg.to_owned(),
        }
    }

    if raw.contains("```") {
        return Err(err("markdown fence in reviewer YAML"));
    }

    // Multiple document separator (second --- after initial content).
    if raw.contains("\n---") {
        return Err(err("multiple YAML documents in reviewer output"));
    }

    // Detect anchors, aliases, and tags using targeted regexes.
    static ANCHOR_RE: OnceLock<Regex> = OnceLock::new();
    static ALIAS_RE: OnceLock<Regex> = OnceLock::new();
    static TAG_RE: OnceLock<Regex> = OnceLock::new();

    // Anchor: & at value position (after leading whitespace / list marker / mapping value)
    let anchor = ANCHOR_RE.get_or_init(|| {
        Regex::new(r"(?m)(?:^[ \t]*(?:-[ \t]+)?|:[ \t]+|,[ \t]*)&[a-zA-Z_][a-zA-Z0-9_-]*")
            .expect("anchor regex")
    });
    // Alias: * at value position
    let alias = ALIAS_RE.get_or_init(|| {
        Regex::new(r"(?m)(?:^[ \t]*(?:-[ \t]+)?|:[ \t]+|,[ \t]*)\*[a-zA-Z_][a-zA-Z0-9_-]*")
            .expect("alias regex")
    });
    // Tag: ! at value position
    let tag = TAG_RE.get_or_init(|| {
        Regex::new(r"(?m)(?:^[ \t]*(?:-[ \t]+)?|:[ \t]+|,[ \t]*)!!?[a-zA-Z]").expect("tag regex")
    });

    if anchor.is_match(raw) {
        return Err(err("YAML anchor in reviewer output"));
    }
    if alias.is_match(raw) {
        return Err(err("YAML alias in reviewer output"));
    }
    if tag.is_match(raw) {
        return Err(err("YAML tag in reviewer output"));
    }

    // Use yaml-rust2 YamlLoader to verify exactly one document can be loaded.
    let docs = yaml_rust2::YamlLoader::load_from_str(raw).map_err(|e| {
        OrchestratorError::ArtifactContract {
            contract: format!("YAML parse error in pre-scan: {e}"),
        }
    })?;
    if docs.len() != 1 {
        return Err(err("reviewer output must be exactly one YAML document"));
    }

    Ok(())
}

/// Parse raw reviewer YAML: pre-scan → typed deserialization → semantic validation.
pub fn parse_reviewer_yaml(raw: &str) -> OrchestratorResult<ReviewerYaml> {
    prescan_reviewer_yaml(raw)?;

    let yaml: ReviewerYaml = serde_yaml::from_str(raw)?;

    validate_reviewer_yaml(&yaml)?;

    Ok(yaml)
}

/// Semantic validation of a parsed `ReviewerYaml`.
fn validate_reviewer_yaml(yaml: &ReviewerYaml) -> OrchestratorResult<()> {
    fn protocol(msg: &str) -> OrchestratorError {
        OrchestratorError::ArtifactContract {
            contract: msg.to_owned(),
        }
    }

    if !(0.0..=10.0).contains(&yaml.quality_score) {
        return Err(protocol(&format!(
            "quality_score must be in [0, 10], got {}",
            yaml.quality_score
        )));
    }

    if yaml.rationale.trim().is_empty() {
        return Err(protocol("rationale must not be empty"));
    }

    // Decision-specific rules
    match yaml.decision {
        ReviewDecision::Accept => {
            if yaml.quality_score < crate::constants::MIN_ACCEPT_SCORE {
                return Err(protocol(&format!(
                    "decision=accept requires quality_score >= {}, got {}",
                    crate::constants::MIN_ACCEPT_SCORE,
                    yaml.quality_score
                )));
            }
            if !yaml.contract_satisfied {
                return Err(protocol("decision=accept requires contract_satisfied=true"));
            }
            if yaml.hard_blockers_present {
                return Err(protocol(
                    "decision=accept requires hard_blockers_present=false",
                ));
            }
        }
        ReviewDecision::Revise => {
            if yaml.feedback_for_executor.is_empty() {
                return Err(protocol(
                    "decision=revise requires non-empty feedback_for_executor",
                ));
            }
        }
        ReviewDecision::Blocked => {
            if yaml
                .blocking_reason
                .as_ref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
            {
                return Err(protocol(
                    "decision=blocked requires non-empty blocking_reason",
                ));
            }
        }
        ReviewDecision::IrreconcilableDisagreement => {
            if yaml
                .irreconcilable_reason
                .as_ref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
            {
                return Err(protocol(
                    "decision=irreconcilable_disagreement requires non-empty irreconcilable_reason",
                ));
            }
        }
        ReviewDecision::PoisonedSession => {
            if yaml
                .poisoned_session_reason
                .as_ref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
            {
                return Err(protocol(
                    "decision=poisoned_session requires non-empty poisoned_session_reason",
                ));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_ACCEPT: &str = r#"
quality_score: 9.0
decision: accept
rationale: Everything is good
contract_satisfied: true
hard_blockers_present: false
notion_requirements_satisfied: true
checks_performed: "any string payload"
findings:
  category: "whatever_snake_case"
feedback_for_executor: []
verification_commands:
  skipped: true
blocking_reason: null
irreconcilable_reason: null
poisoned_session_reason: null
"#;

    const MINIMAL_REVISE: &str = r#"
quality_score: 5.0
decision: revise
rationale: Changes are needed
contract_satisfied: false
hard_blockers_present: false
notion_requirements_satisfied: true
checks_performed: "free form"
findings: []
feedback_for_executor:
  - Add tests
verification_commands: []
blocking_reason: null
irreconcilable_reason: null
poisoned_session_reason: null
"#;

    #[test]
    fn parse_valid_accept_yaml() {
        let result = parse_reviewer_yaml(MINIMAL_ACCEPT);
        assert!(result.is_ok(), "{result:?}");
        let y = result.expect("accept yaml must parse");
        assert_eq!(y.decision, ReviewDecision::Accept);
        assert_eq!(y.quality_score, 9.0);
    }

    #[test]
    fn parse_valid_revise_yaml() {
        let result = parse_reviewer_yaml(MINIMAL_REVISE);
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn reject_markdown_fence() {
        let raw = "```yaml\nfoo: bar\n```";
        assert!(prescan_reviewer_yaml(raw).is_err());
    }

    #[test]
    fn reject_multiple_documents() {
        let raw = "foo: bar\n---\nbaz: qux";
        assert!(prescan_reviewer_yaml(raw).is_err());
    }

    #[test]
    fn reject_yaml_anchor() {
        let raw = "foo: &anchor bar\nbaz: *anchor";
        assert!(prescan_reviewer_yaml(raw).is_err());
    }

    #[test]
    fn reject_yaml_alias() {
        let raw = "foo: bar\nbaz: *foo_alias";
        assert!(prescan_reviewer_yaml(raw).is_err());
    }

    #[test]
    fn reject_accept_with_low_score() {
        let raw = MINIMAL_ACCEPT.replace("9.0", "7.0");
        let result = parse_reviewer_yaml(&raw);
        assert!(result.is_err());
    }

    #[test]
    fn reject_revise_without_feedback() {
        let raw = MINIMAL_REVISE.replace(
            "feedback_for_executor:\n  - Add tests",
            "feedback_for_executor: []",
        );
        let result = parse_reviewer_yaml(&raw);
        assert!(result.is_err());
    }

    #[test]
    fn reject_accept_when_contract_not_satisfied() {
        let raw = MINIMAL_ACCEPT.replace("contract_satisfied: true", "contract_satisfied: false");
        let result = parse_reviewer_yaml(&raw);
        assert!(result.is_err());
    }

    #[test]
    fn reject_accept_when_hard_blockers_present() {
        let raw = MINIMAL_ACCEPT.replace(
            "hard_blockers_present: false",
            "hard_blockers_present: true",
        );
        let result = parse_reviewer_yaml(&raw);
        assert!(result.is_err());
    }

    #[test]
    fn reject_unknown_fields() {
        let raw = MINIMAL_ACCEPT.to_owned() + "unknown_field: oops\n";
        let result = parse_reviewer_yaml(&raw);
        assert!(result.is_err());
    }

    #[test]
    fn reject_empty_rationale() {
        let raw = MINIMAL_ACCEPT.replace("rationale: Everything is good", "rationale: \"   \"");
        let result = parse_reviewer_yaml(&raw);
        assert!(result.is_err());
    }
}
