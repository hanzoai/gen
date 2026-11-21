//! Harbor benchmark trial adapter for Hanzo Gen.
//!
//! Provides `HarborTaskSpec`, `HarborTrialResult`, and pure filesystem helpers
//! to emit, read, and verify reproducible trial bundles (`harness.lock.json`,
//! `agent_run.json`, `evidence.json`) in a caller-provided directory without
//! reading process environment variables or user home directories.
//!
//! Fixed-model benchmark trials enforce `model_locked == true` across the task
//! specification, `HarnessLock`, and `AgentRun` telemetry record.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

use crate::contract::{
    validate_agent_run_json, AgentRun, ConformanceCheck, EvidenceRecord, HarnessLock,
};

/// Filename for the serialized [`HarnessLock`] inside a Harbor trial bundle directory.
pub const HARNESS_LOCK_FILE: &str = "harness.lock.json";

/// Filename for the serialized [`AgentRun`] inside a Harbor trial bundle directory.
pub const AGENT_RUN_FILE: &str = "agent_run.json";

/// Filename for the serialized [`EvidenceRecord`] inside a Harbor trial bundle directory.
pub const EVIDENCE_FILE: &str = "evidence.json";

/// Specification for a single Harbor benchmark task trial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarborTaskSpec {
    pub task_id: String,
    pub benchmark: String,
    pub prompt: String,
    pub model_id: String,
    /// Fixed-model benchmark trials require `model_locked == true`.
    pub model_locked: bool,
    pub sandbox_runtime: String,
    pub timeout_seconds: u64,
    #[serde(default)]
    pub environment_digest: String,
}

impl HarborTaskSpec {
    /// Validate conformance of this task specification for a fixed-model Harbor trial.
    pub fn validate(&self) -> ConformanceCheck {
        let mut violations = Vec::new();
        if self.task_id.trim().is_empty() {
            violations.push("taskId must be non-empty".to_string());
        }
        if self.benchmark.trim().is_empty() {
            violations.push("benchmark must be non-empty".to_string());
        }
        if self.prompt.trim().is_empty() {
            violations.push("prompt must be non-empty".to_string());
        }
        if self.model_id.trim().is_empty() {
            violations.push("modelId must be non-empty".to_string());
        }
        if !self.model_locked {
            violations.push(
                "fixed-model Harbor benchmark trials require modelLocked == true".to_string(),
            );
        }
        if self.sandbox_runtime.trim().is_empty() {
            violations.push("sandboxRuntime must be non-empty".to_string());
        }
        if self.timeout_seconds == 0 {
            violations.push("timeoutSeconds must be greater than 0".to_string());
        }
        if self.environment_digest.trim().is_empty() {
            violations.push("environmentDigest must be non-empty".to_string());
        }
        ConformanceCheck::from_violations("harbor.task_spec", violations)
    }

    /// Derive a [`HarnessLock`] from this task spec, agent version, and git commit.
    pub fn build_lock(
        &self,
        agent_version: impl Into<String>,
        git_commit: impl Into<String>,
    ) -> Result<HarnessLock> {
        let check = self.validate();
        if !check.passed {
            bail!(
                "invalid HarborTaskSpec: {}",
                check.violations.join("; ")
            );
        }
        Ok(HarnessLock::new(
            agent_version,
            git_commit,
            self.environment_digest.clone(),
            self.model_id.clone(),
            self.model_locked,
            self.sandbox_runtime.clone(),
        ))
    }
}

/// Complete, self-verifying result of a single Harbor benchmark trial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarborTrialResult {
    pub task_id: String,
    pub harness_lock: HarnessLock,
    pub agent_run: AgentRun,
    pub evidence: EvidenceRecord,
}

impl HarborTrialResult {
    /// Construct a `HarborTrialResult` and seal its [`EvidenceRecord`] against
    /// `harness_lock.digest()` and `agent_run`.
    #[allow(clippy::too_many_arguments)]
    pub fn seal(
        task_id: impl Into<String>,
        evidence_id: impl Into<String>,
        parent_id: Option<String>,
        generation: u64,
        harness_lock: HarnessLock,
        agent_run: AgentRun,
        product_fitness: f64,
        research_yield: f64,
    ) -> Result<Self> {
        let lock_digest = harness_lock.digest();
        let evidence = EvidenceRecord::new(
            evidence_id,
            parent_id,
            generation,
            lock_digest,
            agent_run.clone(),
            product_fitness,
            research_yield,
        )?;
        let result = Self {
            task_id: task_id.into(),
            harness_lock,
            agent_run,
            evidence,
        };
        let check = result.validate();
        if !check.passed {
            bail!(
                "HarborTrialResult failed conformance validation: {}",
                check.violations.join("; ")
            );
        }
        Ok(result)
    }

    /// Validate all structural, fixed-model, and cryptographic invariants of this trial.
    pub fn validate(&self) -> ConformanceCheck {
        let mut violations = Vec::new();

        if self.task_id.trim().is_empty() {
            violations.push("taskId must be non-empty".to_string());
        }
        if self.task_id != self.agent_run.task_id {
            violations.push(format!(
                "taskId ({:?}) does not match agentRun.taskId ({:?})",
                self.task_id, self.agent_run.task_id
            ));
        }

        if !self.harness_lock.model_locked {
            violations.push(
                "harnessLock.modelLocked must be true for fixed-model benchmark trials".to_string(),
            );
        }
        if !self.agent_run.model_locked {
            violations.push(
                "agentRun.modelLocked must be true for fixed-model benchmark trials".to_string(),
            );
        }
        if self.harness_lock.model_id != self.agent_run.model_id {
            violations.push(format!(
                "harnessLock.modelId ({:?}) does not match agentRun.modelId ({:?})",
                self.harness_lock.model_id, self.agent_run.model_id
            ));
        }

        let lock_check = self.harness_lock.validate();
        for v in lock_check.violations {
            violations.push(format!("harnessLock.{v}"));
        }

        let run_check = self.agent_run.validate();
        for v in run_check.violations {
            violations.push(format!("agentRun.{v}"));
        }

        let expected_lock_digest = self.harness_lock.digest();
        if self.evidence.lock_digest != expected_lock_digest {
            violations.push(format!(
                "evidence.lockDigest ({}) does not match harnessLock.digest() ({})",
                self.evidence.lock_digest, expected_lock_digest
            ));
        }

        if self.evidence.agent_run != self.agent_run {
            violations.push(
                "evidence.agentRun does not match agent_run.json payload".to_string(),
            );
        }

        let evidence_check = self.evidence.validate();
        for v in evidence_check.violations {
            violations.push(format!("evidence.{v}"));
        }

        ConformanceCheck::from_violations("harbor.trial_result", violations)
    }
}

/// Concrete file paths written by [`emit_trial_bundle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialBundlePaths {
    pub harness_lock: PathBuf,
    pub agent_run: PathBuf,
    pub evidence: PathBuf,
}

/// Write a validated Harbor trial bundle (`harness.lock.json`, `agent_run.json`,
/// `evidence.json`) into the caller-provided directory `dir`.
///
/// Refuses to write if `trial.validate()` fails (including when `model_locked`
/// is `false` or the cryptographic commitment does not verify).
pub fn emit_trial_bundle(dir: &Path, trial: &HarborTrialResult) -> Result<TrialBundlePaths> {
    let check = trial.validate();
    if !check.passed {
        bail!(
            "refusing to emit invalid Harbor trial bundle: {}",
            check.violations.join("; ")
        );
    }

    fs::create_dir_all(dir)
        .with_context(|| format!("creating trial bundle directory {}", dir.display()))?;

    let lock_path = dir.join(HARNESS_LOCK_FILE);
    let run_path = dir.join(AGENT_RUN_FILE);
    let evidence_path = dir.join(EVIDENCE_FILE);

    let lock_bytes = serde_json::to_string_pretty(&trial.harness_lock)
        .context("serializing harness.lock.json")?;
    let run_bytes = serde_json::to_string_pretty(&trial.agent_run)
        .context("serializing agent_run.json")?;
    let evidence_bytes = serde_json::to_string_pretty(&trial.evidence)
        .context("serializing evidence.json")?;

    fs::write(&lock_path, format!("{lock_bytes}\n"))
        .with_context(|| format!("writing {}", lock_path.display()))?;
    fs::write(&run_path, format!("{run_bytes}\n"))
        .with_context(|| format!("writing {}", run_path.display()))?;
    fs::write(&evidence_path, format!("{evidence_bytes}\n"))
        .with_context(|| format!("writing {}", evidence_path.display()))?;

    Ok(TrialBundlePaths {
        harness_lock: lock_path,
        agent_run: run_path,
        evidence: evidence_path,
    })
}

/// Read and validate a Harbor trial bundle from the caller-provided directory `dir`.
///
/// Verifies wire schemas, `model_locked == true`, lock digest binding, `agent_run`
/// consistency, and the offline SHA-256 `EvidenceRecord` commitment.
pub fn read_trial_bundle(dir: &Path) -> Result<HarborTrialResult> {
    let (trial, check) = load_and_check_bundle(dir)?;
    if !check.passed {
        bail!(
            "Harbor trial bundle in {} failed verification: {}",
            dir.display(),
            check.violations.join("; ")
        );
    }
    Ok(trial)
}

/// Inspect a Harbor trial bundle on disk and return its [`ConformanceCheck`]
/// without turning semantic or cryptographic violations into an `Err`.
pub fn verify_trial_bundle(dir: &Path) -> Result<ConformanceCheck> {
    let (_, check) = load_and_check_bundle(dir)?;
    Ok(check)
}

fn load_and_check_bundle(dir: &Path) -> Result<(HarborTrialResult, ConformanceCheck)> {
    let lock_path = dir.join(HARNESS_LOCK_FILE);
    let run_path = dir.join(AGENT_RUN_FILE);
    let evidence_path = dir.join(EVIDENCE_FILE);

    let lock_raw = fs::read_to_string(&lock_path)
        .with_context(|| format!("reading {}", lock_path.display()))?;
    let run_raw = fs::read_to_string(&run_path)
        .with_context(|| format!("reading {}", run_path.display()))?;
    let evidence_raw = fs::read_to_string(&evidence_path)
        .with_context(|| format!("reading {}", evidence_path.display()))?;

    let harness_lock: HarnessLock = serde_json::from_str(&lock_raw)
        .with_context(|| format!("parsing {}", lock_path.display()))?;

    let run_value: Value = serde_json::from_str(&run_raw)
        .with_context(|| format!("parsing {} as JSON", run_path.display()))?;
    let run_schema_check = validate_agent_run_json(&run_value);

    let agent_run: AgentRun = serde_json::from_value(run_value)
        .with_context(|| format!("decoding AgentRun from {}", run_path.display()))?;
    let evidence: EvidenceRecord = serde_json::from_str(&evidence_raw)
        .with_context(|| format!("parsing {}", evidence_path.display()))?;

    let trial = HarborTrialResult {
        task_id: agent_run.task_id.clone(),
        harness_lock,
        agent_run,
        evidence,
    };

    let mut violations = Vec::new();
    if !run_schema_check.passed {
        for v in run_schema_check.violations {
            violations.push(format!("agent_run.schema: {v}"));
        }
    }
    let trial_check = trial.validate();
    violations.extend(trial_check.violations);

    Ok((
        trial,
        ConformanceCheck::from_violations("harbor.trial_bundle", violations),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{ExecutionTelemetry, UsageTelemetry, VerificationTelemetry};

    fn sample_spec() -> HarborTaskSpec {
        HarborTaskSpec {
            task_id: "harbor_swebench_001".to_string(),
            benchmark: "swebench-verified".to_string(),
            prompt: "Fix the race condition in connection pool teardown.".to_string(),
            model_id: "enso-pro".to_string(),
            model_locked: true,
            sandbox_runtime: "microvm".to_string(),
            timeout_seconds: 900,
            environment_digest: "sha256:container-rust-1.82-bookworm".to_string(),
        }
    }

    fn sample_trial() -> HarborTrialResult {
        let spec = sample_spec();
        let lock = spec
            .build_lock("0.4.2", "7b9cd5e9c61c3f9a1b0d4c2e8f7a6b5c4d3e2f1a")
            .expect("valid spec builds lock");
        let mut run = AgentRun::new(
            "run_harbor_001",
            &spec.task_id,
            "0.4.2",
            &spec.model_id,
            true,
            "file:///artifacts/run_harbor_001/trace.jsonl",
        );
        run.execution = ExecutionTelemetry {
            mode: "managed".to_string(),
            steps: 9,
            tool_calls: 24,
            elapsed_ms: 18_450,
            exit_reason: "completed".to_string(),
        };
        run.usage = UsageTelemetry {
            input_tokens: 12_400,
            cached_input_tokens: 4_100,
            output_tokens: 1_850,
            cost_usd: 0.0412,
        };
        run.verification = VerificationTelemetry {
            checks_executed: vec!["cargo test".to_string()],
            checks_passed: true,
            critic_verdict: Some("pass".to_string()),
            grader_result: Some("1.0".to_string()),
        };

        HarborTrialResult::seal(
            &spec.task_id,
            "ev_harbor_001",
            None,
            0,
            lock,
            run,
            1.0,
            0.72,
        )
        .expect("sealing valid trial must succeed")
    }

    #[test]
    fn task_spec_enforces_model_locked() {
        let mut spec = sample_spec();
        assert!(spec.validate().passed);

        spec.model_locked = false;
        let check = spec.validate();
        assert!(!check.passed);
        assert!(check.violations.iter().any(|v| v.contains("modelLocked")));
        assert!(spec.build_lock("0.4.2", "abc1234").is_err());
    }

    #[test]
    fn harbor_trial_bundle_write_read_verify_round_trip() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let bundle_dir = tmp.path().join("trial-001");
        let trial = sample_trial();

        let paths = emit_trial_bundle(&bundle_dir, &trial).expect("emit bundle");
        assert!(paths.harness_lock.is_file());
        assert!(paths.agent_run.is_file());
        assert!(paths.evidence.is_file());
        assert_eq!(paths.harness_lock.file_name().unwrap(), HARNESS_LOCK_FILE);
        assert_eq!(paths.agent_run.file_name().unwrap(), AGENT_RUN_FILE);
        assert_eq!(paths.evidence.file_name().unwrap(), EVIDENCE_FILE);

        let verify = verify_trial_bundle(&bundle_dir).expect("verify bundle");
        assert!(verify.passed, "unexpected violations: {:?}", verify.violations);

        let loaded = read_trial_bundle(&bundle_dir).expect("read bundle");
        assert_eq!(loaded, trial);
        assert!(loaded.evidence.verify_commitment());
        assert_eq!(loaded.evidence.lock_digest, loaded.harness_lock.digest());
    }

    #[test]
    fn emit_rejects_unlocked_model_trial() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let mut trial = sample_trial();
        trial.harness_lock.model_locked = false;
        trial.agent_run.model_locked = false;
        trial.evidence.lock_digest = trial.harness_lock.digest();
        trial.evidence.agent_run = trial.agent_run.clone();
        trial.evidence.commitment_sha256 = trial.evidence.compute_commitment().unwrap();

        let err = emit_trial_bundle(tmp.path(), &trial).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("modelLocked"), "unexpected error: {msg}");
        assert!(!tmp.path().join(HARNESS_LOCK_FILE).exists());
    }

    #[test]
    fn read_and_verify_detect_tampered_lock_or_evidence_on_disk() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let trial = sample_trial();
        let paths = emit_trial_bundle(tmp.path(), &trial).expect("emit bundle");

        // Tamper with harness.lock.json after emission.
        let mut tampered_lock = trial.harness_lock.clone();
        tampered_lock.sandbox_runtime = "host".to_string();
        fs::write(
            &paths.harness_lock,
            serde_json::to_string_pretty(&tampered_lock).unwrap(),
        )
        .unwrap();

        let check = verify_trial_bundle(tmp.path()).expect("verify should return check");
        assert!(!check.passed);
        assert!(check.violations.iter().any(|v| v.contains("lockDigest")));
        assert!(read_trial_bundle(tmp.path()).is_err());

        // Restore lock, then tamper with evidence.json productFitness without resealing.
        fs::write(
            &paths.harness_lock,
            serde_json::to_string_pretty(&trial.harness_lock).unwrap(),
        )
        .unwrap();
        let mut tampered_evidence = trial.evidence.clone();
        tampered_evidence.product_fitness = 0.01;
        fs::write(
            &paths.evidence,
            serde_json::to_string_pretty(&tampered_evidence).unwrap(),
        )
        .unwrap();

        let check2 = verify_trial_bundle(tmp.path()).expect("verify should return check");
        assert!(!check2.passed);
        assert!(check2.violations.iter().any(|v| v.contains("commitmentSha256")));
        assert!(read_trial_bundle(tmp.path()).is_err());
    }
}
