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
