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
use hanzo_client::{Http, Method, Request, Transport};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

use super::context::{Machine, Metrics, Spec, TargetRecord};
use crate::config::Config;
use crate::iam::{paths, store};

#[derive(Clone)]
pub struct TargetClient {
    wire: Http,
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
        let wire = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building target http client")?;
        Ok(Self {
            wire: Http::new(wire),
            api: api.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
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
        let mut request =
            Request::new(method, format!("{}{}", self.api, path)).token(&self.token);
        if let Some(body) = body {
            request = request.body(serde_json::to_value(body).context("encoding the target")?);
        }
        Ok(self.wire.send(request).await?.ok()?)
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

/// How often a held-open machine says it is still alive.
///
/// Cloud's `LiveWindow` is 90 seconds and its comment names THIS beat: a target
/// that has not written inside the window reads offline no matter what its row
/// says. Beating at a third of the window means two beats can be lost — a suspended
/// laptop, a flaky link — before a live machine is reported dead.
pub const BEAT: Duration = Duration::from_secs(30);

/// A heartbeat that runs for exactly as long as the caller holds this guard.
///
/// REGISTERING IS NOT BEING ALIVE. A register stamps the server's staleness clock
/// once; ninety seconds later cloud calls the machine offline — correctly, because
/// nothing has said otherwise since. A machine that registered at link time and
/// then went quiet is indistinguishable from one that was unplugged, which is why
/// the console filled with dead-looking boxes that were in fact running.
///
/// There is no goodbye. Ending the beat IS the ending: the window expires and the
/// machine reads offline on its own. A second "I am leaving" message would be a
/// second way to say the same thing, and the one that gets lost when the power
/// cord goes.
pub struct Beat(tokio::task::JoinHandle<()>);

impl Drop for Beat {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Hold this machine's run-target open: register now, then re-state it every
/// [`BEAT`] until the returned guard drops.
///
/// Each beat RE-CAPTURES the machine, because a heartbeat's payload is what the
/// box is doing NOW — a repeat of the load average from an hour ago is a timestamp
/// wearing a sample's clothes.
///
/// AND EACH BEAT RE-READS THE CREDENTIAL. An access token lives one hour; a link
/// lives as long as the shell does. A beat holding the token it was handed at
/// startup therefore beats correctly for an hour and then spends the rest of the
/// session sending an expired bearer — which `sync` swallows, because every
/// failure here is best-effort by design. The machine goes offline while the
/// process is still running and still serving, which is the exact symptom the
/// heartbeat exists to prevent. `store::active_token` is the ONE accessor that
/// refreshes, and it takes its own `Config` clone: the credential file is the
/// shared state, not the struct, and its writes are already serialized under the
/// credential lock.
pub fn beat(cfg: &Config, api: &str, machine_id: &str, host: &str) -> Beat {
    beat_every(BEAT, Creds::Refreshing(Box::new(cfg.clone())), api, machine_id, host)
}

/// Where a beat gets its bearer.
///
/// Production has exactly ONE source — `store::active_token`, the single accessor
/// that refreshes — and the test variant is compiled only under `cfg(test)`, so
/// there is no second credential path to drift. It exists because the loop is what
/// needs observing, and a unit test must not read the developer's real vault.
enum Creds {
    Refreshing(Box<Config>),
    #[cfg(test)]
    Fixed(String),
}

impl Creds {
    async fn bearer(&mut self) -> Option<String> {
        match self {
            Creds::Refreshing(cfg) => store::active_token(cfg, paths::DEFAULT_BRAND)
                .await
                .ok()
                .flatten()
                .map(|(_, t)| t.access_token),
            #[cfg(test)]
            Creds::Fixed(t) => Some(t.clone()),
        }
    }
}

/// [`beat`] with the period as a parameter, so a test can watch the loop repeat
/// without waiting minutes for it. `BEAT` is the ONE period production uses — this
/// is the mechanism, not a setting.
fn beat_every(period: Duration, mut creds: Creds, api: &str, machine_id: &str, host: &str) -> Beat {
    let api = api.to_string();
    let (machine_id, host) = (machine_id.to_string(), host.to_string());
    Beat(tokio::spawn(async move {
        loop {
            // A beat with no credential is a beat that cannot land, so skip the
            // capture too rather than probe the machine for nothing.
            match creds.bearer().await {
                Some(token) => {
                    let machine = Machine::capture().await;
                    sync(&api, &token, &machine_id, &host, &machine).await;
                }
                None => tracing::debug!("no credential for this beat; the machine will read offline"),
            }
            tokio::time::sleep(period).await;
        }
    }))
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

    /// The beat has to land INSIDE cloud's window, repeatedly. One register is what
    /// the CLI used to do, and it is why a machine that was running read offline
    /// ninety seconds later: the row was never written again, so the only fact the
    /// server had was stale.
    #[test]
    fn the_beat_fits_inside_the_window_it_is_answering() {
        // cloud/apps/agents/targets.go: LiveWindow = 90 * time.Second.
        assert!(
            BEAT.as_secs() * 3 <= 90,
            "two beats must be losable before a live machine reads dead"
        );
    }

    /// Wait until `f` holds, or fail — polling, because what is being observed is
    /// another task making progress and a fixed sleep would encode this machine's
    /// speed as the contract.
    async fn until(what: &str, f: impl Fn() -> bool) {
        for _ in 0..600 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Holding a machine open writes MORE THAN ONCE — the difference between
    /// "registered" and "alive", and the whole reason this exists.
    #[tokio::test]
    async fn a_held_machine_keeps_saying_so() {
        let mock = MockCloud::start().await;
        let machine = format!("beat_{}", std::process::id());
        let _ = std::fs::remove_file(super::super::context::target_path_for_test(&machine));

        let held = beat_every(Duration::from_millis(10), Creds::Fixed("T".into()), &mock.base_url(), &machine, "evo");
        until("a machine to keep beating", || mock.requests().len() >= 3).await;
        drop(held);

        let _ = std::fs::remove_file(super::super::context::target_path_for_test(&machine));
    }

    /// A beat asks for its bearer EVERY time, so a credential that changes under
    /// it is picked up. Holding the token handed over at startup is why a link
    /// beat correctly for one hour — the life of an access token — and then spent
    /// the rest of the session sending an expired bearer, which `sync` swallows
    /// by design. The machine read offline while the shell was still serving.
    #[tokio::test]
    async fn every_beat_re_reads_the_credential() {
        let mock = MockCloud::start().await;
        let machine = format!("beatcred_{}", std::process::id());
        let _ = std::fs::remove_file(super::super::context::target_path_for_test(&machine));

        let held = beat_every(
            Duration::from_millis(10),
            Creds::Fixed("T".into()),
            &mock.base_url(),
            &machine,
            "evo",
        );
        until("several beats", || mock.requests().len() >= 3).await;
        drop(held);

        // Every request carried a bearer — none went out unauthenticated because a
        // cached token had gone stale.
        let reqs = mock.requests();
        assert!(
            reqs.iter().all(|r| r.header("authorization").as_deref() == Some("Bearer T")),
            "a beat must carry a freshly-read credential",
        );
        let _ = std::fs::remove_file(super::super::context::target_path_for_test(&machine));
    }

    /// Letting go stops the beat, and stopping is the whole ending: cloud's window
    /// expires on its own. Nothing sends a goodbye, so nothing can fail to.
    #[tokio::test]
    async fn letting_go_stops_it() {
        let mock = MockCloud::start().await;
        let machine = format!("beatdrop_{}", std::process::id());
        let _ = std::fs::remove_file(super::super::context::target_path_for_test(&machine));

        let held = beat_every(Duration::from_millis(10), Creds::Fixed("T".into()), &mock.base_url(), &machine, "evo");
        until("the first beat", || !mock.requests().is_empty()).await;
        drop(held);
        // A beat already in flight may still land, so settle before reading the mark.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let after_drop = mock.requests().len();

        tokio::time::sleep(Duration::from_millis(500)).await; // 50 periods' worth
        assert_eq!(mock.requests().len(), after_drop, "a dropped beat writes nothing more");
        let _ = std::fs::remove_file(super::super::context::target_path_for_test(&machine));
    }
}
