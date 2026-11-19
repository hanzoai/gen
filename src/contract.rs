//! Conformance contract and telemetry schema for Hanzo Gen agent runs.
//!
//! Defines the wire-stable `AgentRun` telemetry record emitted by routed and
//! managed agent executions, the `SpecManifest` describing the supported
//! conformance surface, and `ConformanceCheck` / JSON schema validation helpers.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::Usage;

/// Conformance contract specification version implemented by this crate.
pub const SPEC_VERSION: &str = "1.0.0";

/// Harness version stamped onto emitted `AgentRun` records.
pub const HARNESS_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Canonical JSON schema URI for `AgentRun` records.
pub const AGENT_RUN_SCHEMA_ID: &str = "https://hanzo.ai/schemas/agent-run/v1";

/// Allowed values for [`ExecutionTelemetry::mode`].
pub const VALID_EXECUTION_MODES: &[&str] = &["launch", "observe", "managed"];

/// Required top-level camelCase keys in a wire `AgentRun` JSON object.
pub const REQUIRED_AGENT_RUN_FIELDS: &[&str] = &[
    "runId",
    "taskId",
    "harnessVersion",
    "agentVersion",
    "modelId",
    "modelLocked",
    "execution",
    "usage",
    "verification",
    "traceUri",
];

const REQUIRED_EXECUTION_FIELDS: &[&str] = &[
    "mode",
    "steps",
    "toolCalls",
    "elapsedMs",
    "exitReason",
];

const REQUIRED_USAGE_FIELDS: &[&str] = &[
    "inputTokens",
    "cachedInputTokens",
    "outputTokens",
    "costUsd",
];

const REQUIRED_VERIFICATION_FIELDS: &[&str] = &[
    "checksExecuted",
    "checksPassed",
];

/// Complete telemetry record for a single agent run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRun {
    pub run_id: String,
    pub task_id: String,
    pub harness_version: String,
    pub agent_version: String,
    pub model_id: String,
    pub model_locked: bool,
    pub execution: ExecutionTelemetry,
    pub usage: UsageTelemetry,
    pub verification: VerificationTelemetry,
    pub trace_uri: String,
}

impl AgentRun {
    /// Construct a new `AgentRun` pre-populated with this crate's harness version.
    pub fn new(
        run_id: impl Into<String>,
        task_id: impl Into<String>,
        agent_version: impl Into<String>,
        model_id: impl Into<String>,
        model_locked: bool,
        trace_uri: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            task_id: task_id.into(),
            harness_version: HARNESS_VERSION.to_string(),
            agent_version: agent_version.into(),
            model_id: model_id.into(),
            model_locked,
            execution: ExecutionTelemetry::default(),
            usage: UsageTelemetry::default(),
            verification: VerificationTelemetry::default(),
            trace_uri: trace_uri.into(),
        }
    }

    /// Validate semantic conformance of this `AgentRun` record.
    pub fn validate(&self) -> ConformanceCheck {
        let mut violations = Vec::new();

        if self.run_id.trim().is_empty() {
            violations.push("runId must be non-empty".to_string());
        }
        if self.task_id.trim().is_empty() {
            violations.push("taskId must be non-empty".to_string());
        }
        if self.harness_version.trim().is_empty() {
            violations.push("harnessVersion must be non-empty".to_string());
        }
        if self.model_id.trim().is_empty() {
            violations.push("modelId must be non-empty".to_string());
        }
        if self.trace_uri.trim().is_empty() {
            violations.push("traceUri must be non-empty".to_string());
        }
        if !VALID_EXECUTION_MODES.contains(&self.execution.mode.as_str()) {
            violations.push(format!(
                "execution.mode must be one of {:?}, got {:?}",
                VALID_EXECUTION_MODES, self.execution.mode
            ));
        }
        if self.execution.exit_reason.trim().is_empty() {
            violations.push("execution.exitReason must be non-empty".to_string());
        }
        if !self.usage.cost_usd.is_finite() || self.usage.cost_usd < 0.0 {
            violations.push(format!(
                "usage.costUsd must be finite and non-negative, got {}",
                self.usage.cost_usd
            ));
        }

        ConformanceCheck::from_violations("agent_run.semantic", violations)
    }

    /// Serialize this `AgentRun` to JSON and validate both wire schema and semantics.
    pub fn to_validated_json(&self) -> Result<Value, ConformanceCheck> {
        let value = serde_json::to_value(self).map_err(|e| {
            ConformanceCheck::from_violations(
                "agent_run.schema",
                vec![format!("failed to serialize AgentRun: {e}")],
            )
        })?;
        let check = validate_agent_run_json(&value);
        if check.passed {
            Ok(value)
        } else {
            Err(check)
        }
    }
}

/// Execution lifecycle telemetry for an agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionTelemetry {
    /// Execution mode: `"launch"`, `"observe"`, or `"managed"`.
    pub mode: String,
    pub steps: u64,
    pub tool_calls: u64,
    pub elapsed_ms: u64,
    pub exit_reason: String,
}

impl Default for ExecutionTelemetry {
    fn default() -> Self {
        Self {
            mode: "managed".to_string(),
            steps: 0,
            tool_calls: 0,
            elapsed_ms: 0,
            exit_reason: "completed".to_string(),
        }
    }
}

/// Token and cost accounting telemetry for an agent run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTelemetry {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl UsageTelemetry {
    /// Convert a session stream [`Usage`] record into contract [`UsageTelemetry`].
    pub fn from_event_usage(usage: &Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens.unwrap_or(0),
            cached_input_tokens: usage.cache_read_tokens.unwrap_or(0),
            output_tokens: usage.output_tokens.unwrap_or(0),
            cost_usd: usage.total_cost_usd.unwrap_or(0.0),
        }
    }
}

/// Post-execution verification and grading telemetry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationTelemetry {
    pub checks_executed: Vec<String>,
    pub checks_passed: bool,
    pub critic_verdict: Option<String>,
    pub grader_result: Option<String>,
}

/// Machine-readable manifest describing this harness's conformance contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecManifest {
    pub spec_version: String,
    pub harness_name: String,
    pub harness_version: String,
    pub schema_id: String,
    pub supported_modes: Vec<String>,
    pub required_fields: Vec<String>,
}

impl SpecManifest {
    /// Return the current conformance manifest for `hanzo-gen`.
    pub fn current() -> Self {
        Self {
            spec_version: SPEC_VERSION.to_string(),
            harness_name: env!("CARGO_PKG_NAME").to_string(),
            harness_version: HARNESS_VERSION.to_string(),
            schema_id: AGENT_RUN_SCHEMA_ID.to_string(),
            supported_modes: VALID_EXECUTION_MODES
                .iter()
                .map(|m| (*m).to_string())
                .collect(),
            required_fields: REQUIRED_AGENT_RUN_FIELDS
                .iter()
                .map(|f| (*f).to_string())
                .collect(),
        }
    }
}

/// Result of a single conformance or schema check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConformanceCheck {
    pub name: String,
    pub passed: bool,
    pub violations: Vec<String>,
}

impl ConformanceCheck {
    /// Create a passing check with no violations.
    pub fn pass(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: true,
            violations: Vec::new(),
        }
    }

    /// Create a check whose pass/fail status is determined by whether `violations` is empty.
    pub fn from_violations(name: impl Into<String>, violations: Vec<String>) -> Self {
        Self {
            name: name.into(),
            passed: violations.is_empty(),
            violations,
        }
    }
}

/// Validate a raw JSON [`Value`] against the `AgentRun` wire schema and semantic rules.
pub fn validate_agent_run_json(value: &Value) -> ConformanceCheck {
    let mut violations = Vec::new();

    let Some(obj) = value.as_object() else {
        violations.push("root value must be a JSON object".to_string());
        return ConformanceCheck::from_violations("agent_run.schema", violations);
    };

    for &key in REQUIRED_AGENT_RUN_FIELDS {
        if !obj.contains_key(key) {
            violations.push(format!("missing required field '{key}'"));
        }
    }

    check_nested_object(obj, "execution", REQUIRED_EXECUTION_FIELDS, &mut violations);
    check_nested_object(obj, "usage", REQUIRED_USAGE_FIELDS, &mut violations);
    check_nested_object(
        obj,
        "verification",
        REQUIRED_VERIFICATION_FIELDS,
        &mut violations,
    );

    if !violations.is_empty() {
        return ConformanceCheck::from_violations("agent_run.schema", violations);
    }

    match serde_json::from_value::<AgentRun>(value.clone()) {
        Ok(run) => {
            let semantic = run.validate();
            ConformanceCheck::from_violations("agent_run.schema", semantic.violations)
        }
        Err(e) => {
            violations.push(format!("type mismatch while decoding AgentRun: {e}"));
            ConformanceCheck::from_violations("agent_run.schema", violations)
        }
    }
}

fn check_nested_object(
    parent: &serde_json::Map<String, Value>,
    field: &str,
    required_keys: &[&str],
    violations: &mut Vec<String>,
) {
    let Some(child) = parent.get(field) else {
        return;
    };
    let Some(child_obj) = child.as_object() else {
        violations.push(format!("field '{field}' must be a JSON object"));
        return;
    };
    for &key in required_keys {
        if !child_obj.contains_key(key) {
            violations.push(format!("missing required field '{field}.{key}'"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_run() -> AgentRun {
        let mut run = AgentRun::new(
            "run_01",
            "task_01",
            "0.4.2",
            "enso-auto",
            true,
            "https://hanzo.bot/sessions/run_01",
        );
        run.execution = ExecutionTelemetry {
            mode: "managed".to_string(),
            steps: 5,
            tool_calls: 12,
            elapsed_ms: 4200,
            exit_reason: "completed".to_string(),
        };
        run.usage = UsageTelemetry {
            input_tokens: 1500,
            cached_input_tokens: 500,
            output_tokens: 300,
            cost_usd: 0.0125,
        };
        run.verification = VerificationTelemetry {
            checks_executed: vec!["cargo test".to_string(), "cargo clippy".to_string()],
            checks_passed: true,
            critic_verdict: Some("approved".to_string()),
            grader_result: None,
        };
        run
    }

    #[test]
    fn agent_run_round_trips_with_exact_camel_case_wire_keys() {
        let run = sample_run();
        let val = run.to_validated_json().expect("sample run must validate");
        let obj = val.as_object().expect("must serialize as JSON object");

        for &key in REQUIRED_AGENT_RUN_FIELDS {
            assert!(obj.contains_key(key), "missing wire key {key}");
        }
        assert!(obj.get("run_id").is_none(), "must not emit snake_case keys");
        assert_eq!(obj["runId"], "run_01");
        assert_eq!(obj["taskId"], "task_01");
        assert_eq!(obj["harnessVersion"], HARNESS_VERSION);
        assert_eq!(obj["agentVersion"], "0.4.2");
        assert_eq!(obj["modelId"], "enso-auto");
        assert_eq!(obj["modelLocked"], true);
        assert_eq!(obj["traceUri"], "https://hanzo.bot/sessions/run_01");

        assert_eq!(obj["execution"]["toolCalls"], 12);
        assert_eq!(obj["execution"]["elapsedMs"], 4200);
        assert_eq!(obj["execution"]["exitReason"], "completed");

        assert_eq!(obj["usage"]["inputTokens"], 1500);
        assert_eq!(obj["usage"]["cachedInputTokens"], 500);
        assert_eq!(obj["usage"]["outputTokens"], 300);
        assert_eq!(obj["usage"]["costUsd"], 0.0125);

        assert_eq!(obj["verification"]["checksPassed"], true);
        assert_eq!(obj["verification"]["criticVerdict"], "approved");
        assert!(obj["verification"]["graderResult"].is_null());

        let decoded: AgentRun = serde_json::from_value(val).expect("round-trip decode");
        assert_eq!(decoded, run);
    }

    #[test]
    fn schema_validation_rejects_missing_required_fields() {
        let incomplete = json!({
            "runId": "run_02",
            "taskId": "task_02"
        });
        let check = validate_agent_run_json(&incomplete);
        assert!(!check.passed);
        assert!(check.violations.iter().any(|v| v.contains("harnessVersion")));
        assert!(check.violations.iter().any(|v| v.contains("execution")));
        assert!(check.violations.iter().any(|v| v.contains("traceUri")));
    }

    #[test]
    fn schema_validation_rejects_missing_nested_fields() {
        let mut val = serde_json::to_value(sample_run()).unwrap();
        val["execution"].as_object_mut().unwrap().remove("toolCalls");
        val["usage"].as_object_mut().unwrap().remove("costUsd");
        val["verification"].as_object_mut().unwrap().remove("checksPassed");

        let check = validate_agent_run_json(&val);
        assert!(!check.passed);
        assert!(check.violations.iter().any(|v| v.contains("execution.toolCalls")));
        assert!(check.violations.iter().any(|v| v.contains("usage.costUsd")));
        assert!(check.violations.iter().any(|v| v.contains("verification.checksPassed")));
    }

    #[test]
    fn semantic_validation_enforces_execution_mode_and_non_negative_cost() {
        let mut run = sample_run();
        run.execution.mode = "invalid_mode".to_string();
        run.usage.cost_usd = -1.5;
        run.run_id.clear();

        let check = run.validate();
        assert!(!check.passed);
        assert!(check.violations.iter().any(|v| v.contains("execution.mode")));
        assert!(check.violations.iter().any(|v| v.contains("usage.costUsd")));
        assert!(check.violations.iter().any(|v| v.contains("runId")));
    }

    #[test]
    fn schema_validation_rejects_wrong_field_types() {
        let mut val = serde_json::to_value(sample_run()).unwrap();
        val["modelLocked"] = json!("yes");

        let check = validate_agent_run_json(&val);
        assert!(!check.passed);
        assert!(check.violations.iter().any(|v| v.contains("type mismatch")));
    }

    #[test]
    fn usage_telemetry_converts_from_event_usage() {
        let event_usage = Usage {
            input_tokens: Some(120),
            output_tokens: Some(45),
            cache_read_tokens: Some(30),
            cache_write_tokens: Some(10),
            total_cost_usd: Some(0.0042),
            num_turns: Some(3),
            duration_ms: Some(900),
        };
        let telemetry = UsageTelemetry::from_event_usage(&event_usage);
        assert_eq!(telemetry.input_tokens, 120);
        assert_eq!(telemetry.cached_input_tokens, 30);
        assert_eq!(telemetry.output_tokens, 45);
        assert_eq!(telemetry.cost_usd, 0.0042);
    }

    #[test]
    fn spec_manifest_exposes_expected_contract_metadata() {
        let manifest = SpecManifest::current();
        assert_eq!(manifest.spec_version, SPEC_VERSION);
        assert_eq!(manifest.harness_name, "hanzo-gen");
        assert_eq!(manifest.harness_version, HARNESS_VERSION);
        assert_eq!(manifest.schema_id, AGENT_RUN_SCHEMA_ID);
        assert_eq!(manifest.supported_modes, vec!["launch", "observe", "managed"]);
        assert_eq!(manifest.required_fields.len(), REQUIRED_AGENT_RUN_FIELDS.len());

        let val = serde_json::to_value(&manifest).unwrap();
        assert_eq!(val["specVersion"], SPEC_VERSION);
        assert_eq!(val["harnessName"], "hanzo-gen");
    }
}
