//! The cloud run-target registry client: `/v1/agents/targets`.
//!
//! A linked machine registers what it IS (`spec`) and what it is DOING now
//! (`metrics`) so mission-control can show which computer an agent runs on and
//! whether it can take more work — WITHOUT copying that fact onto every session.
//! The register upserts by `host`: re-linking the same machine refreshes ONE target
//! row instead of piling up duplicates.
//!
//! Org-scoped SERVER-SIDE — the gateway injects the org from the validated JWT
//! `owner`, so this client sends only the hanzo.id bearer and can neither send nor
//! forge an org. Everything here is BEST-EFFORT: a register/heartbeat failure is the
//! caller's to swallow, and it NEVER blocks or fails the coding session. See
//! `cloud/clients/agents/targets.go`.

use anyhow::{Context, Result};
use reqwest::{Client, Method};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

use super::context::{Machine, Metrics, Spec, TargetRecord};
use crate::http::send_json;

#[derive(Clone)]
pub struct TargetClient {
    http: Client,
    api: String, // base origin, no trailing slash
    token: String,
}

/// The register / refresh body. `label` + `host` are the hostname; `host` is the
/// upsert key. `metrics.at` is NEVER present (the struct has no such field) — the
/// server owns the staleness clock. The server sanitizes/bounds every field.
#[derive(Debug, Clone, Serialize)]
pub struct Register {
    pub label: String,
    pub kind: String,   // "gpu" when GPUs present, else "laptop"
    pub status: String, // "online"
    #[serde(skip_serializing_if = "String::is_empty")]
    pub capacity: String,
    pub host: String,
    pub spec: Spec,
    pub metrics: Metrics,
}

impl Register {
    /// Build the register body for `host` from a captured [`Machine`]: kind is
    /// "gpu" when the machine has any accelerator, else "laptop"; capacity is the
    /// spec's human summary.
    pub fn from_machine(host: &str, m: &Machine) -> Register {
        let kind = if m.spec.gpus.is_empty() { "laptop" } else { "gpu" };
        Register {
            label: host.to_string(),
            kind: kind.to_string(),
            status: "online".to_string(),
            capacity: m.spec.capacity(),
            host: host.to_string(),
            spec: m.spec.clone(),
            metrics: m.metrics.clone(),
        }
    }
}

impl TargetClient {
    pub fn new(api: &str, token: &str) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building target http client")?;
        Ok(Self { http, api: api.trim_end_matches('/').to_string(), token: token.to_string() })
    }

    /// Register-or-upsert this machine's target (`POST /v1/agents/targets`).
    /// Returns the target id cloud minted (201) or refreshed by host (200).
    pub async fn register(&self, body: &Register) -> Result<String> {
        let v = self.send(Method::POST, "/v1/agents/targets", Some(body)).await?;
        id_of(&v)
    }

    /// Refresh an existing target by id (`PATCH /v1/agents/targets/:id`). Sending
    /// the full body updates the capability and IS a metrics heartbeat (the server
    /// stamps its time). Errors on a non-2xx — e.g. a 404 for a target that was
    /// deleted or belongs to another org — so the caller can fall back to register.
    pub async fn refresh(&self, id: &str, body: &Register) -> Result<String> {
        let v = self.send(Method::PATCH, &format!("/v1/agents/targets/{id}"), Some(body)).await?;
        id_of(&v)
    }

    async fn send(&self, method: Method, path: &str, body: Option<&Register>) -> Result<Value> {
        let url = format!("{}{}", self.api, path);
        send_json(&self.http, method, &url, &self.token, body).await
    }
}

/// Extract the target id from a `targetView` response.
fn id_of(v: &Value) -> Result<String> {
    v.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("target response missing id")
}

/// Register or refresh THIS machine's run-target, reusing the stored id when we have
/// one (a cheap PATCH heartbeat) and falling back to a fresh register when there is
/// none or the stored target is gone (deleted / different org). BEST-EFFORT: every
/// failure is logged at debug and swallowed — the coding session never depends on
/// this. The caller runs it detached so neither the capture nor the cloud write is
/// on the session's critical path.
pub async fn sync(api: &str, token: &str, machine_id: &str, host: &str, machine: &Machine) {
    let client = match TargetClient::new(api, token) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("run-target client unavailable ({e}); skipping target register");
            return;
        }
    };
    let body = Register::from_machine(host, machine);

    // Reuse the stored id ONLY for the same machine + host + api, so a copied data
    // dir or a renamed host re-registers instead of clobbering another target.
    let stored = TargetRecord::load(machine_id)
        .ok()
        .flatten()
        .filter(|r| r.host == host && r.api == api);

    let id = match &stored {
        Some(rec) => match client.refresh(&rec.id, &body).await {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::debug!("target heartbeat failed ({e}); re-registering");
                client.register(&body).await.ok()
            }
        },
        None => client.register(&body).await.ok(),
    };

    match id {
        Some(id) => {
            let rec = TargetRecord {
                id,
                host: host.to_string(),
                machine_id: machine_id.to_string(),
                api: api.to_string(),
                updated_at: chrono::Utc::now().timestamp(),
            };
            if let Err(e) = rec.save() {
                tracing::debug!("could not persist target id ({e})");
            }
        }
        None => tracing::debug!("run-target register failed; session proceeds without a target"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code::context::Gpu;
    use crate::commands::code::testmock::MockCloud;

    fn gpu_machine() -> Machine {
        Machine {
            spec: Spec {
                os: "linux".into(),
                arch: "arm64".into(),
                cpus: 20,
                memory: 137438953472,
                gpus: vec![Gpu { vendor: "nvidia".into(), model: "GB10".into(), memory: 103079215104 }],
            },
            metrics: Metrics { load1: 1.5, load5: 1.2, load15: 0.9, mem_used: 42, mem_free: 7, gpu_util: 0.4 },
        }
    }

    fn laptop_machine() -> Machine {
        Machine {
            spec: Spec { os: "macos".into(), arch: "arm64".into(), cpus: 8, memory: 16 * (1i64 << 30), gpus: vec![] },
            metrics: Metrics { load1: 0.3, ..Default::default() },
        }
    }

    #[test]
    fn register_body_matches_the_contract() {
        let body = Register::from_machine("evo", &gpu_machine());
        assert_eq!(body.label, "evo");
        assert_eq!(body.host, "evo");
        assert_eq!(body.kind, "gpu"); // GPUs present
        assert_eq!(body.status, "online");
        assert_eq!(body.capacity, "20 vCPU / 128G / 1× GB10");

        let v = serde_json::to_value(&body).unwrap();
        // Exactly the contract's top-level keys, camelCase spec/metrics inside.
        assert_eq!(v["spec"]["cpus"], 20);
        assert_eq!(v["spec"]["memory"], serde_json::json!(137438953472i64));
        assert_eq!(v["spec"]["gpus"][0]["model"], "GB10");
        assert_eq!(v["metrics"]["memUsed"], 42);
        assert_eq!(v["metrics"]["gpuUtil"], 0.4);
        assert!(v["metrics"].get("at").is_none(), "must not send the metrics timestamp");
    }

    #[test]
    fn kind_is_laptop_without_gpus_and_gpu_with_them() {
        assert_eq!(Register::from_machine("air", &laptop_machine()).kind, "laptop");
        assert_eq!(Register::from_machine("evo", &gpu_machine()).kind, "gpu");
    }

    #[tokio::test]
    async fn register_posts_the_body_with_bearer_and_no_org() {
        let mock = MockCloud::start().await;
        let client = TargetClient::new(&mock.base_url(), "TOK").unwrap();
        let id = client.register(&Register::from_machine("evo", &gpu_machine())).await.unwrap();
        assert_eq!(id, "tgt_mock");

        let reqs = mock.requests();
        let r = reqs.iter().find(|r| r.method == "POST" && r.path == "/v1/agents/targets").unwrap();
        assert_eq!(r.header("authorization").as_deref(), Some("Bearer TOK"));
        assert!(r.header("x-org-id").is_none(), "CLI must not send X-Org-Id");
        assert_eq!(r.json()["host"], "evo");
        assert_eq!(r.json()["kind"], "gpu");
        assert_eq!(r.json()["spec"]["cpus"], 20);
        assert_eq!(r.json()["metrics"]["gpuUtil"], 0.4);
    }

    #[tokio::test]
    async fn refresh_patches_by_id() {
        let mock = MockCloud::start().await;
        let client = TargetClient::new(&mock.base_url(), "T").unwrap();
        let id = client.refresh("tgt_1", &Register::from_machine("evo", &gpu_machine())).await.unwrap();
        assert_eq!(id, "tgt_1");
        let reqs = mock.requests();
        assert!(reqs.iter().any(|r| r.method == "PATCH" && r.path == "/v1/agents/targets/tgt_1"));
    }

    /// Fresh machine (no stored id) registers, then persists the id it got back.
    #[tokio::test]
    async fn sync_registers_when_no_id_is_stored_and_persists_it() {
        let mock = MockCloud::start().await;
        let machine = format!("syncfresh_{}", std::process::id());
        let _ = std::fs::remove_file(super::super::context::target_path_for_test(&machine));
        sync(&mock.base_url(), "T", &machine, "evo", &gpu_machine()).await;

        assert!(mock.requests().iter().any(|r| r.method == "POST" && r.path == "/v1/agents/targets"));
        let rec = TargetRecord::load(&machine).unwrap().unwrap();
        assert_eq!(rec.id, "tgt_mock");
        assert_eq!(rec.host, "evo");
        let _ = std::fs::remove_file(super::super::context::target_path_for_test(&machine));
    }

    /// A stored id is heartbeated (PATCH); if the target is gone (404) we fall back
    /// to a fresh register — self-healing across a delete or an org switch.
    #[tokio::test]
    async fn sync_falls_back_to_register_when_the_stored_target_is_gone() {
        let mock = MockCloud::start_target_missing().await;
        let machine = format!("syncgone_{}", std::process::id());
        // Seed a stored id that the server will 404 on PATCH.
        TargetRecord {
            id: "tgt_stale".into(),
            host: "evo".into(),
            machine_id: machine.clone(),
            api: mock.base_url(),
            updated_at: 1,
        }
        .save()
        .unwrap();

        sync(&mock.base_url(), "T", &machine, "evo", &gpu_machine()).await;

        let reqs = mock.requests();
        assert!(reqs.iter().any(|r| r.method == "PATCH" && r.path == "/v1/agents/targets/tgt_stale"), "tries the heartbeat first");
        assert!(reqs.iter().any(|r| r.method == "POST" && r.path == "/v1/agents/targets"), "falls back to register");
        // The freshly registered id replaced the stale one.
        assert_eq!(TargetRecord::load(&machine).unwrap().unwrap().id, "tgt_mock");
        let _ = std::fs::remove_file(super::super::context::target_path_for_test(&machine));
    }
}
