//! Conformance contract and telemetry schema for Hanzo Gen agent runs.
//!
//! Defines the wire-stable `AgentRun` telemetry record emitted by routed and
//! managed agent executions, the `SpecManifest` describing the supported
//! conformance surface, and `ConformanceCheck` / JSON schema validation helpers.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::event::Usage;

/// Conformance contract specification version implemented by this crate.
pub const SPEC_VERSION: &str = "1.0.0";

/// Harness version stamped onto emitted `AgentRun` records.
pub const HARNESS_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Canonical JSON schema URI for `AgentRun` records.
pub const AGENT_RUN_SCHEMA_ID: &str = "https://hanzo.ai/schemas/agent-run/v1";

/// Schema version stamped onto emitted `HarnessLock` records.
pub const HARNESS_LOCK_SCHEMA_VERSION: &str = "1.0.0";

/// Canonical JSON schema URI for `HarnessLock` records.
pub const HARNESS_LOCK_SCHEMA_ID: &str = "https://hanzo.ai/schemas/harness-lock/v1";

/// Canonical JSON schema URI for `EvidenceRecord` records.
pub const EVIDENCE_RECORD_SCHEMA_ID: &str = "https://hanzo.ai/schemas/evidence-record/v1";

/// Required top-level camelCase keys in a wire `HarnessLock` JSON object.
pub const REQUIRED_HARNESS_LOCK_FIELDS: &[&str] = &[
    "schemaVersion",
    "harnessVersion",
    "agentVersion",
    "gitCommit",
    "environmentDigest",
    "modelId",
    "modelLocked",
    "sandboxRuntime",
];

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

/// Render a JSON [`Value`] in canonical deterministic form with lexicographically
/// sorted object keys and no insignificant whitespace.
///
/// Sorting keys explicitly guarantees identical byte output even if a transitive
/// dependency enables `serde_json`'s `preserve_order` feature.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical_json(value, &mut out);
    out
}

fn write_canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            // Normalize -0.0 to 0.0 for cross-serializer stability.
            if let Some(f) = n.as_f64() {
                if f == 0.0 && f.is_sign_negative() {
                    out.push_str("0.0");
                    return;
                }
            }
            out.push_str(&n.to_string());
        }
        Value::String(s) => {
            out.push_str(&serde_json::to_string(s).expect("string JSON serialization is infallible"));
        }
        Value::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
            out.push('{');
            for (idx, (k, v)) in entries.into_iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).expect("key JSON serialization is infallible"));
                out.push(':');
                write_canonical_json(v, out);
            }
            out.push('}');
        }
    }
}

/// Compute the lowercase hex SHA-256 digest of the canonical JSON representation of `value`.
pub fn canonical_sha256(value: &Value) -> String {
    let canonical = canonical_json(value);
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

/// Return `true` iff `s` is a valid 64-character lowercase hexadecimal SHA-256 digest.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Versioned, reproducible lockfile pinning the exact harness, agent, source commit,
/// environment, model, and sandbox runtime for a run or benchmark trial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessLock {
    pub schema_version: String,
    pub harness_version: String,
    pub agent_version: String,
    pub git_commit: String,
    pub environment_digest: String,
    pub model_id: String,
    pub model_locked: bool,
    pub sandbox_runtime: String,
}

impl HarnessLock {
    /// Construct a new `HarnessLock` pre-populated with this crate's schema and harness versions.
    pub fn new(
        agent_version: impl Into<String>,
        git_commit: impl Into<String>,
        environment_digest: impl Into<String>,
        model_id: impl Into<String>,
        model_locked: bool,
        sandbox_runtime: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: HARNESS_LOCK_SCHEMA_VERSION.to_string(),
            harness_version: HARNESS_VERSION.to_string(),
            agent_version: agent_version.into(),
            git_commit: git_commit.into(),
            environment_digest: environment_digest.into(),
            model_id: model_id.into(),
            model_locked,
            sandbox_runtime: sandbox_runtime.into(),
        }
    }

    /// Validate semantic conformance of this `HarnessLock`.
    pub fn validate(&self) -> ConformanceCheck {
        let mut violations = Vec::new();
        if self.schema_version.trim().is_empty() {
            violations.push("schemaVersion must be non-empty".to_string());
        }
        if self.harness_version.trim().is_empty() {
            violations.push("harnessVersion must be non-empty".to_string());
        }
        if self.agent_version.trim().is_empty() {
            violations.push("agentVersion must be non-empty".to_string());
        }
        if self.git_commit.trim().is_empty() {
            violations.push("gitCommit must be non-empty".to_string());
        }
        if self.environment_digest.trim().is_empty() {
            violations.push("environmentDigest must be non-empty".to_string());
        }
        if self.model_id.trim().is_empty() {
            violations.push("modelId must be non-empty".to_string());
        }
        if self.sandbox_runtime.trim().is_empty() {
            violations.push("sandboxRuntime must be non-empty".to_string());
        }
        ConformanceCheck::from_violations("harness_lock.semantic", violations)
    }

    /// Return the canonical SHA-256 hex digest of this lockfile.
    pub fn digest(&self) -> String {
        let value = serde_json::to_value(self).expect("HarnessLock is always JSON-serializable");
        canonical_sha256(&value)
    }

    /// Serialize this `HarnessLock` to JSON and validate all required fields and semantics.
    pub fn to_validated_json(&self) -> Result<Value, ConformanceCheck> {
        let check = self.validate();
        if !check.passed {
            return Err(check);
        }
        serde_json::to_value(self).map_err(|e| {
            ConformanceCheck::from_violations(
                "harness_lock.schema",
                vec![format!("failed to serialize HarnessLock: {e}")],
            )
        })
    }
}

/// Immutable, offline-verifiable experiment evidence record tying an [`AgentRun`]
/// to its [`HarnessLock`] digest, lineage, and fitness/yield metrics via a canonical
/// SHA-256 commitment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceRecord {
    pub id: String,
    pub parent_id: Option<String>,
    pub generation: u64,
    pub lock_digest: String,
    pub agent_run: AgentRun,
    pub product_fitness: f64,
    pub research_yield: f64,
    pub commitment_sha256: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceCommitmentPayload<'a> {
    id: &'a str,
    parent_id: &'a Option<String>,
    generation: u64,
    lock_digest: &'a str,
    agent_run: &'a AgentRun,
    product_fitness: f64,
    research_yield: f64,
}

impl EvidenceRecord {
    /// Construct and seal an `EvidenceRecord`, computing its canonical `commitment_sha256`.
    pub fn new(
        id: impl Into<String>,
        parent_id: Option<String>,
        generation: u64,
        lock_digest: impl Into<String>,
        agent_run: AgentRun,
        product_fitness: f64,
        research_yield: f64,
    ) -> anyhow::Result<Self> {
        let mut record = Self {
            id: id.into(),
            parent_id,
            generation,
            lock_digest: lock_digest.into(),
            agent_run,
            product_fitness,
            research_yield,
            commitment_sha256: String::new(),
        };
        record.commitment_sha256 = record.compute_commitment()?;
        Ok(record)
    }

    /// Compute the deterministic canonical SHA-256 commitment over all record fields
    /// except `commitmentSha256`.
    pub fn compute_commitment(&self) -> anyhow::Result<String> {
        if !self.product_fitness.is_finite() {
            anyhow::bail!("productFitness must be finite, got {}", self.product_fitness);
        }
        if !self.research_yield.is_finite() {
            anyhow::bail!("researchYield must be finite, got {}", self.research_yield);
        }
        if !self.agent_run.usage.cost_usd.is_finite() {
            anyhow::bail!(
                "agentRun.usage.costUsd must be finite, got {}",
                self.agent_run.usage.cost_usd
            );
        }
        let payload = EvidenceCommitmentPayload {
            id: &self.id,
            parent_id: &self.parent_id,
            generation: self.generation,
            lock_digest: &self.lock_digest,
            agent_run: &self.agent_run,
            product_fitness: self.product_fitness,
            research_yield: self.research_yield,
        };
        let value = serde_json::to_value(&payload)
            .context("serializing EvidenceRecord commitment payload")?;
        Ok(canonical_sha256(&value))
    }

    /// Verify offline that `commitment_sha256` matches the canonical SHA-256 commitment
    /// of this record's contents.
    pub fn verify_commitment(&self) -> bool {
        if !is_sha256_hex(&self.commitment_sha256) {
            return false;
        }
        match self.compute_commitment() {
            Ok(expected) => expected == self.commitment_sha256,
            Err(_) => false,
        }
    }

    /// Validate semantic conformance and cryptographic commitment integrity.
    pub fn validate(&self) -> ConformanceCheck {
        let mut violations = Vec::new();
        if self.id.trim().is_empty() {
            violations.push("id must be non-empty".to_string());
        }
        if !is_sha256_hex(&self.lock_digest) {
            violations.push(format!(
                "lockDigest must be a 64-character lowercase hex SHA-256 digest, got {:?}",
                self.lock_digest
            ));
        }
        if !self.product_fitness.is_finite() {
            violations.push(format!(
                "productFitness must be finite, got {}",
                self.product_fitness
            ));
        }
        if !self.research_yield.is_finite() {
            violations.push(format!(
                "researchYield must be finite, got {}",
                self.research_yield
            ));
        }
        let run_check = self.agent_run.validate();
        for v in run_check.violations {
            violations.push(format!("agentRun.{v}"));
        }
        if !self.verify_commitment() {
            violations.push("commitmentSha256 does not match computed canonical commitment".to_string());
        }
        ConformanceCheck::from_violations("evidence_record.semantic", violations)
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

    fn sample_lock() -> HarnessLock {
        HarnessLock::new(
            "0.4.2",
            "7b9cd5e9c61c3f9a1b0d4c2e8f7a6b5c4d3e2f1a",
            "sha256:env-linux-x86_64-rust-1.82",
            "enso-auto",
            true,
            "microvm",
        )
    }

    #[test]
    fn canonical_json_is_key_order_independent() {
        let a = json!({
            "z": 1,
            "a": { "d": true, "b": [3, 2, 1], "c": null }
        });
        let b = json!({
            "a": { "c": null, "b": [3, 2, 1], "d": true },
            "z": 1
        });
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(canonical_sha256(&a), canonical_sha256(&b));
        assert_eq!(
            canonical_json(&a),
            r#"{"a":{"b":[3,2,1],"c":null,"d":true},"z":1}"#
        );
    }

    #[test]
    fn harness_lock_round_trip_and_digest_stability() {
        let lock = sample_lock();
        let digest1 = lock.digest();
        assert!(is_sha256_hex(&digest1), "digest must be 64-char lowercase hex");

        let val = lock.to_validated_json().expect("sample lock must validate");
        let obj = val.as_object().expect("must serialize as JSON object");
        for &key in REQUIRED_HARNESS_LOCK_FIELDS {
            assert!(obj.contains_key(key), "missing wire key {key}");
        }
        assert!(obj.get("schema_version").is_none(), "must not emit snake_case");
        assert_eq!(obj["schemaVersion"], HARNESS_LOCK_SCHEMA_VERSION);
        assert_eq!(obj["harnessVersion"], HARNESS_VERSION);
        assert_eq!(obj["modelLocked"], true);

        let decoded: HarnessLock = serde_json::from_value(val).expect("round-trip decode");
        assert_eq!(decoded, lock);
        assert_eq!(decoded.digest(), digest1, "digest must remain stable across round-trip");

        // Mutating any field must change the canonical digest.
        let mut mutated = lock.clone();
        mutated.sandbox_runtime = "runc".to_string();
        assert_ne!(mutated.digest(), digest1);

        let mut unlocked = lock.clone();
        unlocked.model_locked = false;
        assert_ne!(unlocked.digest(), digest1);
    }

    #[test]
    fn harness_lock_validation_rejects_empty_fields() {
        let mut lock = sample_lock();
        lock.git_commit.clear();
        lock.sandbox_runtime = "   ".to_string();

        let check = lock.validate();
        assert!(!check.passed);
        assert!(check.violations.iter().any(|v| v.contains("gitCommit")));
        assert!(check.violations.iter().any(|v| v.contains("sandboxRuntime")));
    }

    #[test]
    fn evidence_record_commitment_is_deterministic_and_round_trips() {
        let lock = sample_lock();
        let run = sample_run();
        let record = EvidenceRecord::new(
            "ev_001",
            Some("ev_000".to_string()),
            1,
            lock.digest(),
            run.clone(),
            0.9125,
            0.45,
        )
        .expect("sealing evidence record must succeed");

        assert!(is_sha256_hex(&record.commitment_sha256));
        assert!(record.verify_commitment());
        assert!(record.validate().passed);

        // Recomputing commitment is deterministic.
        let recomputed = record.compute_commitment().expect("recompute");
        assert_eq!(recomputed, record.commitment_sha256);

        // Constructing an identical record produces the exact same commitment.
        let identical = EvidenceRecord::new(
            "ev_001",
            Some("ev_000".to_string()),
            1,
            lock.digest(),
            run,
            0.9125,
            0.45,
        )
        .expect("sealing identical record");
        assert_eq!(identical.commitment_sha256, record.commitment_sha256);

        // JSON wire format uses camelCase and preserves commitment verification.
        let raw = serde_json::to_string(&record).expect("serialize");
        let val: Value = serde_json::from_str(&raw).expect("parse value");
        assert!(val.get("parentId").is_some());
        assert!(val.get("lockDigest").is_some());
        assert!(val.get("agentRun").is_some());
        assert!(val.get("productFitness").is_some());
        assert!(val.get("researchYield").is_some());
        assert!(val.get("commitmentSha256").is_some());

        let decoded: EvidenceRecord = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(decoded, record);
        assert!(decoded.verify_commitment());
    }

    #[test]
    fn evidence_record_detects_tampering_on_every_field() {
        let lock = sample_lock();
        let base = EvidenceRecord::new(
            "ev_002",
            None,
            0,
            lock.digest(),
            sample_run(),
            0.85,
            0.30,
        )
        .expect("seal");
        assert!(base.verify_commitment());

        let mut t_id = base.clone();
        t_id.id = "ev_003".to_string();
        assert!(!t_id.verify_commitment());

        let mut t_parent = base.clone();
        t_parent.parent_id = Some("ev_000".to_string());
        assert!(!t_parent.verify_commitment());

        let mut t_gen = base.clone();
        t_gen.generation = 2;
        assert!(!t_gen.verify_commitment());

        let mut t_lock = base.clone();
        t_lock.lock_digest = "0".repeat(64);
        assert!(!t_lock.verify_commitment());

        let mut t_run = base.clone();
        t_run.agent_run.verification.checks_passed = false;
        assert!(!t_run.verify_commitment());

        let mut t_fitness = base.clone();
        t_fitness.product_fitness = 0.99;
        assert!(!t_fitness.verify_commitment());
        let check = t_fitness.validate();
        assert!(!check.passed);
        assert!(check.violations.iter().any(|v| v.contains("commitmentSha256")));

        let mut t_yield = base.clone();
        t_yield.research_yield = 0.31;
        assert!(!t_yield.verify_commitment());

        let mut t_commit = base.clone();
        t_commit.commitment_sha256 = "f".repeat(64);
        assert!(!t_commit.verify_commitment());
    }

    #[test]
    fn evidence_record_rejects_non_finite_metrics() {
        let lock = sample_lock();
        let err = EvidenceRecord::new(
            "ev_nan",
            None,
            0,
            lock.digest(),
            sample_run(),
            f64::NAN,
            0.1,
        );
        assert!(err.is_err());

        let err_inf = EvidenceRecord::new(
            "ev_inf",
            None,
            0,
            lock.digest(),
            sample_run(),
            0.5,
            f64::INFINITY,
        );
        assert!(err_inf.is_err());
    }
}
