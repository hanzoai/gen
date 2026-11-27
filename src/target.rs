//! The cloud run-target registry client: `/v1/agent/targets`.
//!
//! A linked machine registers what it IS (`spec`) and what it is DOING now
//! (`metrics`) so mission-control can show which computer an agent runs on and
//! whether it can take more work — WITHOUT copying that fact onto every session.
//! The register upserts by `host`: re-linking the same machine refreshes ONE target
//! row instead of piling up duplicates.
//!
//! Org-scoped SERVER-SIDE — the gateway takes the org from the validated token. The
//! one thing this client adds is the `--org` selection, as `X-Org-Id`, which the
//! gateway checks against the token's signed membership and discards when it is
//! not one of the caller's orgs: a selection, never an assertion. Without one the
//! machine lands in the caller's home org. Everything here is BEST-EFFORT: a register/heartbeat failure is the
//! caller's to swallow, and it NEVER blocks or fails the coding session. See
//! `cloud/clients/agents/targets.go`.

use anyhow::{Context, Result};
use hanzo_client::{Http, Method, Request, Transport};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

use super::context::{hostname, machine_id, Machine, Metrics, Spec, TargetRecord};
use super::sample::Sampler;
use crate::host::Host;
use crate::root::Root;

#[derive(Clone)]
pub struct TargetClient {
    wire: Http,
    api: String, // base origin, no trailing slash
    token: String,
    /// The org selected with `--org`, sent as `X-Org-Id` — the selection every
    /// cloud call carries (`Config::org`), so a machine lands in the org the rest
    /// of the command acts in.
    org: Option<String>,
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
    /// The https url this machine's own `hanzo link` publishes its shell at. Absent
    /// leaves cloud's copy unchanged, `""` clears it, a url sets it — so a beat
    /// from `hanzo dev` or `hanzo shell`, which publish no shell, never wipes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
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
            terminal: None,
        }
    }
}

/// What cloud answers a register or heartbeat with: the target's id and the folders
/// its owner granted the web (`folders`), so a revoke reaches the machine on its
/// next beat.
#[derive(Debug, Clone, Deserialize)]
pub struct Answer {
    pub id: String,
    #[serde(default)]
    pub folders: Vec<String>,
}

/// The folders this machine's owner granted, as cloud last answered a beat; `None`
/// until an answer lands. The beat writes it, the runner reads and watches it.
pub type Grants = Arc<watch::Sender<Option<Vec<String>>>>;

/// Grants nobody has heard yet.
pub fn grants() -> Grants {
    Arc::new(watch::Sender::new(None))
}

/// The url this machine's shell is published at, as the process that publishes it
/// last said; `None` when it publishes none. The caller writes it, the beat reads it.
type Terminal = Arc<watch::Sender<Option<String>>>;

fn terminal() -> Terminal {
    Arc::new(watch::Sender::new(None))
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
            org: None,
        })
    }

    /// Act in `org` when one was selected (`--org`); `None` is the bearer's own.
    pub fn with_org(mut self, org: Option<&str>) -> Self {
        self.org = org.map(str::to_string);
        self
    }

    /// Register-or-upsert this machine's target (`POST /v1/agent/targets`).
    /// Answers the target cloud minted (201) or refreshed by host (200).
    pub async fn register(&self, body: &Register) -> Result<Answer> {
        let v = self.send(Method::POST, "/v1/agent/targets", Some(body)).await?;
        answer(v)
    }

    /// Refresh an existing target by id (`PATCH /v1/agent/targets/:id`). Sending
    /// the full body updates the capability and IS a metrics heartbeat (the server
    /// stamps its time). Errors on a non-2xx — e.g. a 404 for a target that was
    /// deleted or belongs to another org — so the caller can fall back to register.
    pub async fn refresh(&self, id: &str, body: &Register) -> Result<Answer> {
        let v = self.send(Method::PATCH, &format!("/v1/agent/targets/{id}"), Some(body)).await?;
        answer(v)
    }

    async fn send(&self, method: Method, path: &str, body: Option<&Register>) -> Result<Value> {
        let mut request =
            Request::new(method, format!("{}{}", self.api, path)).token(&self.token);
        if let Some(org) = &self.org {
            request = request.org(org);
        }
        if let Some(body) = body {
            request = request.body(serde_json::to_value(body).context("encoding the target")?);
        }
        Ok(self.wire.send(request).await?.ok()?)
    }
}

/// Read a `targetView` response.
fn answer(v: Value) -> Result<Answer> {
    serde_json::from_value(v).context("target response missing id")
}

/// Register or refresh THIS machine's run-target, reusing the stored id when we have
/// one (a cheap PATCH heartbeat) and falling back to a fresh register when there is
/// none or the stored target is gone (deleted / different org). BEST-EFFORT: every
/// failure is logged at debug and swallowed — the coding session never depends on
/// this. The caller runs it detached so neither the capture nor the cloud write is
/// on the session's critical path. Answers what cloud answered: the target id and its granted folders.
/// The `body` is what to state; its `terminal` is stated as given: `None` says
/// nothing, `Some("")` clears it.
pub async fn sync(root: &Root, api: &str, token: &str, org: Option<&str>, machine_id: &str, host: &str, body: Register) -> Option<Answer> {
    let client = match TargetClient::new(api, token) {
        Ok(c) => c.with_org(org),
        Err(e) => {
            tracing::debug!("run-target client unavailable ({e}); skipping target register");
            return None;
        }
    };

    // Reuse the stored id ONLY for the same machine + host + api, so a copied data
    // dir or a renamed host re-registers instead of clobbering another target.
    let stored = TargetRecord::load(root, machine_id)
        .ok()
        .flatten()
        .filter(|r| r.host == host && r.api == api);

    let answer = match &stored {
        Some(rec) => match client.refresh(&rec.id, &body).await {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::debug!("target heartbeat failed ({e}); re-registering");
                client.register(&body).await.ok()
            }
        },
        None => client.register(&body).await.ok(),
    };

    match answer {
        Some(answer) => {
            let rec = TargetRecord {
                id: answer.id.clone(),
                host: host.to_string(),
                machine_id: machine_id.to_string(),
                api: api.to_string(),
                updated_at: chrono::Utc::now().timestamp(),
            };
            if let Err(e) = rec.save(root) {
                tracing::debug!("could not persist target id ({e})");
            }
            Some(answer)
        }
        None => {
            tracing::debug!("run-target register failed; session proceeds without a target");
            None
        }
    }
}

/// What a machine with no granted folder tells its owner, once: a linked, beating
/// machine reads ready, but no run from the web can touch a file until a grant.
fn no_folder_notice(id: &str, folders: &[String]) -> Option<String> {
    folders.is_empty().then(|| {
        format!("no folder on this machine ({id}) is granted to the web: `hanzo link grant <folder>` grants one")
    })
}

/// Keep an answer's granted folders in `grants`; the notice for the FIRST answer
/// this beat hears, if it granted nothing. `heard` is the beat's own latch.
fn heard_answer(grants: &Grants, heard: &mut bool, answer: Answer) -> Option<String> {
    let first = !std::mem::replace(heard, true);
    let notice = first.then(|| no_folder_notice(&answer.id, &answer.folders)).flatten();
    grants.send_replace(Some(answer.folders));
    notice
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
pub struct Beat {
    task: tokio::task::JoinHandle<()>,
    terminal: Terminal,
    /// The url cloud last accepted from this beat: what `withdraw` waits on.
    acked: Terminal,
}

impl Drop for Beat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Beat {
    /// State the url this machine's shell is published at, now and on every beat
    /// after; `None` withdraws it, saying so once. The terminal is a fact about the
    /// MACHINE, so it lives on this row, whose beat cloud already reads for
    /// liveness, and not on a session, which cloud stops after an hour idle.
    pub fn terminal(&self, url: Option<&str>) {
        self.terminal.send_replace(url.map(str::to_string));
    }

    /// Withdraw the terminal and wait, for a few seconds at most, until cloud has
    /// heard it. A link ends by dropping this guard, which stops the beat, so a
    /// withdrawal left to the next beat would never be said.
    pub async fn withdraw(&self) {
        self.terminal(None);
        let mut acked = self.acked.subscribe();
        let _ = tokio::time::timeout(Duration::from_secs(5), acked.wait_for(Option::is_none)).await;
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
/// heartbeat exists to prevent. [`Host::account`] is the ONE accessor, and it
/// refreshes: the credential is the host's shared state, not a value held here.
/// It is asked for `brand`'s account, the one the session it beats for runs as.
pub fn beat(host: Arc<dyn Host>, root: &Root, brand: &str, api: &str, machine_id: &str, name: &str) -> Beat {
    beat_with(host, root, brand, api, machine_id, name, None)
}

/// [`beat`], writing each answer's granted folders to `grants` when someone serves
/// on them; the first answer with none says so once.
fn beat_with(host: Arc<dyn Host>, root: &Root, brand: &str, api: &str, machine_id: &str, name: &str, grants: Option<Grants>) -> Beat {
    let creds = Creds::Refreshing { host, brand: brand.to_string() };
    beat_every(BEAT, creds, root, api, machine_id, name, grants)
}

/// Where a beat gets its bearer.
///
/// Production has exactly ONE source — the host's account, which refreshes — and
/// the test variant is compiled only under `cfg(test)`, so there is no second
/// credential path to drift. It exists because the loop is what needs observing,
/// and a unit test must not read the developer's real credential.
enum Creds {
    Refreshing { host: Arc<dyn Host>, brand: String },
    #[cfg(test)]
    Fixed(String),
}

impl Creds {
    /// The org selected for this host's calls, sent as `X-Org-Id`.
    async fn org(&self) -> Option<String> {
        match self {
            Creds::Refreshing { host, .. } => host.profile().await.org,
            #[cfg(test)]
            Creds::Fixed(_) => None,
        }
    }

    async fn bearer(&mut self) -> Option<String> {
        match self {
            Creds::Refreshing { host, brand } => host.account(brand).await.ok().flatten().map(|a| a.token),
            #[cfg(test)]
            Creds::Fixed(t) => Some(t.clone()),
        }
    }
}

/// `hanzo beat` — hold this machine in the fleet with no session attached.
///
/// A target is live only while something beats. [`beat`] is held by `hanzo dev`
/// and by `hanzo link`, and both end when the work they host ends — so a machine
/// sitting at an idle prompt reads OFFLINE to the console and to whoever is
/// choosing where the next session lands, while being the least loaded box in the
/// lab. This is the beat on its own: the same refreshed-credential loop, the same
/// sampler, nothing else. Foreground until interrupted, so a service manager owns
/// the lifetime; dropping the guard aborts it and the row goes stale on its own,
/// which is how cloud decides a machine left.
/// Hold this machine open in the fleet AND serve routed runs (`serving: true`).
pub fn beat_and_serve(
    host: Arc<dyn Host>,
    root: &Root,
    brand: &str,
    api: &str,
    machine_id: &str,
    name: &str,
) -> (Beat, crate::runner::Runner) {
    let api = api.trim_end_matches('/');
    let grants = grants();
    let b = beat_with(host.clone(), root, brand, api, machine_id, name, Some(grants.clone()));
    let r = crate::runner::serve(host, root, brand, api, machine_id, name, grants);
    (b, r)
}

pub async fn present(host: Arc<dyn Host>, root: &Root, brand: &str) -> Result<()> {
    let api = host.profile().await.api;
    let name = hostname(root);
    let (_beat, _runner) = beat_and_serve(host, root, brand, &api, &machine_id(root), &name);
    println!(
        "● {name} present and serving in the fleet — one beat every {}s, ready for routed runs. Ctrl-C leaves.",
        BEAT.as_secs()
    );
    tokio::signal::ctrl_c().await.context("waiting for Ctrl-C")?;
    Ok(())
}

/// [`beat`] with the period as a parameter, so a test can watch the loop repeat
/// without waiting minutes for it. `BEAT` is the ONE period production uses — this
/// is the mechanism, not a setting.
fn beat_every(period: Duration, mut creds: Creds, root: &Root, api: &str, machine_id: &str, host: &str, grants: Option<Grants>) -> Beat {
    let root = root.clone();
    let api = api.to_string();
    let (machine_id, host) = (machine_id.to_string(), host.to_string());
    let (said, acked) = (terminal(), terminal());
    let term_acked = acked.clone();
    let mut wanted = said.subscribe();
    let task = tokio::spawn(async move {
        // One sampler for the life of the beat, so every beat after the first
        // carries the rates over the window since the one before it.
        let mut sampler = Sampler::new();
        let org = creds.org().await;
        let mut heard = false;
        // Whether cloud holds a url from this beat, so that withdrawing it is said
        // once — and again next beat if that one is lost.
        let mut stated = false;
        loop {
            // A beat with no credential is a beat that cannot land, so skip the
            // capture too rather than probe the machine for nothing.
            match creds.bearer().await {
                Some(token) => {
                    let machine = sampler.machine().await;
                    let url = wanted.borrow_and_update().clone();
                    let say = match (&url, stated) {
                        (Some(u), _) => Some(u.as_str()),
                        (None, true) => Some(""),
                        (None, false) => None,
                    };
                    let mut body = Register::from_machine(&host, &machine);
                    body.terminal = say.map(str::to_string);
                    if let Some(answer) = sync(&root, &api, &token, org.as_deref(), &machine_id, &host, body).await {
                        stated = url.is_some();
                        term_acked.send_replace(url);
                        if let Some(grants) = &grants {
                            if let Some(line) = heard_answer(grants, &mut heard, answer) {
                                crate::warn(&line);
                            }
                        }
                    }
                }
                None => tracing::debug!("no credential for this beat; the machine will read offline"),
            }
            // A change of terminal is stated at once rather than a period late.
            tokio::select! {
                () = tokio::time::sleep(period) => {}
                _ = wanted.changed() => {}
            }
        }
    });
    Beat { task, terminal: said, acked }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Gpu;
    use crate::testmock::MockCloud;

    fn gpu_machine() -> Machine {
        Machine {
            spec: Spec {
                os: "linux".into(),
                arch: "arm64".into(),
                cpus: 20,
                memory: 137438953472,
                gpus: vec![Gpu { vendor: "nvidia".into(), model: "GB10".into(), memory: 103079215104 }],
                ..Default::default()
            },
            metrics: Metrics { load1: 1.5, load5: 1.2, load15: 0.9, mem_used: 42, mem_free: 7, gpu_util: 0.4, ..Default::default() },
        }
    }

    fn laptop_machine() -> Machine {
        Machine {
            spec: Spec { os: "macos".into(), arch: "arm64".into(), cpus: 8, memory: 16 * (1i64 << 30), gpus: vec![], ..Default::default() },
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
        assert!(v.get("terminal").is_none(), "a body with no terminal says nothing about one");
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
        let id = client.register(&Register::from_machine("evo", &gpu_machine())).await.unwrap().id;
        assert_eq!(id, "tgt_mock");

        let reqs = mock.requests();
        let r = reqs.iter().find(|r| r.method == "POST" && r.path == "/v1/agent/targets").unwrap();
        assert_eq!(r.header("authorization").as_deref(), Some("Bearer TOK"));
        assert!(r.header("x-org-id").is_none(), "with no --org selected, the CLI sends no X-Org-Id");
        assert_eq!(r.json()["host"], "evo");
        assert_eq!(r.json()["kind"], "gpu");
        assert_eq!(r.json()["spec"]["cpus"], 20);
        assert_eq!(r.json()["metrics"]["gpuUtil"], 0.4);
    }

    #[tokio::test]
    async fn a_selected_org_rides_every_target_call() {
        let mock = MockCloud::start().await;
        let client = TargetClient::new(&mock.base_url(), "T").unwrap().with_org(Some("acme"));
        let body = Register::from_machine("evo", &gpu_machine());
        let id = client.register(&body).await.unwrap().id;
        let _ = client.refresh(&id, &body).await;
        let reqs = mock.requests();
        let calls: Vec<_> = reqs.iter().filter(|r| r.path.starts_with("/v1/agent/targets")).collect();
        assert!(calls.len() >= 2, "register and refresh both reached cloud");
        for r in calls {
            assert_eq!(r.header("x-org-id").as_deref(), Some("acme"), "{} {} lost the selected org", r.method, r.path);
        }
    }

    #[tokio::test]
    async fn refresh_patches_by_id() {
        let mock = MockCloud::start().await;
        let client = TargetClient::new(&mock.base_url(), "T").unwrap();
        let id = client.refresh("tgt_1", &Register::from_machine("evo", &gpu_machine())).await.unwrap().id;
        assert_eq!(id, "tgt_1");
        let reqs = mock.requests();
        assert!(reqs.iter().any(|r| r.method == "PATCH" && r.path == "/v1/agent/targets/tgt_1"));
    }

    /// Fresh machine (no stored id) registers, then persists the id it got back.
    #[tokio::test]
    async fn sync_registers_when_no_id_is_stored_and_persists_it() {
        let mock = MockCloud::start().await;
        let root = crate::root::scratch();
        let machine = format!("syncfresh_{}", std::process::id());
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
        sync(&root, &mock.base_url(), "T", None, &machine, "evo", Register::from_machine("evo", &gpu_machine())).await;

        assert!(mock.requests().iter().any(|r| r.method == "POST" && r.path == "/v1/agent/targets"));
        let rec = TargetRecord::load(&root, &machine).unwrap().unwrap();
        assert_eq!(rec.id, "tgt_mock");
        assert_eq!(rec.host, "evo");
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
    }

    /// A stored id is heartbeated (PATCH); if the target is gone (404) we fall back
    /// to a fresh register — self-healing across a delete or an org switch.
    #[tokio::test]
    async fn sync_falls_back_to_register_when_the_stored_target_is_gone() {
        let mock = MockCloud::start_target_missing().await;
        let root = crate::root::scratch();
        let machine = format!("syncgone_{}", std::process::id());
        // Seed a stored id that the server will 404 on PATCH.
        TargetRecord {
            id: "tgt_stale".into(),
            host: "evo".into(),
            machine_id: machine.clone(),
            api: mock.base_url(),
            updated_at: 1,
        }
        .save(&root)
        .unwrap();

        sync(&root, &mock.base_url(), "T", None, &machine, "evo", Register::from_machine("evo", &gpu_machine())).await;

        let reqs = mock.requests();
        assert!(reqs.iter().any(|r| r.method == "PATCH" && r.path == "/v1/agent/targets/tgt_stale"), "tries the heartbeat first");
        assert!(reqs.iter().any(|r| r.method == "POST" && r.path == "/v1/agent/targets"), "falls back to register");
        // The freshly registered id replaced the stale one.
        assert_eq!(TargetRecord::load(&root, &machine).unwrap().unwrap().id, "tgt_mock");
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
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
    /// speed as the contract. The ceiling is thirty seconds for the same reason:
    /// at six, `make test` on a host at a load average past a hundred timed out
    /// here with the beat still running, and that run gates the release.
    async fn until(what: &str, f: impl Fn() -> bool) {
        for _ in 0..3000 {
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
        let root = crate::root::scratch();
        let machine = format!("beat_{}", std::process::id());
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));

        let held = beat_every(Duration::from_millis(10), Creds::Fixed("T".into()), &root, &mock.base_url(), &machine, "evo", Some(grants()));
        until("a machine to keep beating", || mock.requests().len() >= 3).await;
        drop(held);

        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
    }

    /// A beat asks for its bearer EVERY time, so a credential that changes under
    /// it is picked up. Holding the token handed over at startup is why a link
    /// beat correctly for one hour — the life of an access token — and then spent
    /// the rest of the session sending an expired bearer, which `sync` swallows
    /// by design. The machine read offline while the shell was still serving.
    #[tokio::test]
    async fn every_beat_re_reads_the_credential() {
        let mock = MockCloud::start().await;
        let root = crate::root::scratch();
        let machine = format!("beatcred_{}", std::process::id());
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));

        let held = beat_every(
            Duration::from_millis(10),
            Creds::Fixed("T".into()),
            &root,
            &mock.base_url(),
            &machine,
            "evo",
            Some(grants()),
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
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
    }

    /// A beat speaks as the brand of the session it beats for: a `--brand lux`
    /// session's machine registers to lux, with lux's token, never the default's.
    #[tokio::test]
    async fn a_beat_asks_for_its_sessions_brand() {
        struct Asked(std::sync::Mutex<Vec<String>>);
        impl Host for Asked {
            fn profile(&self) -> crate::host::Pending<'_, crate::Profile> {
                Box::pin(async { crate::Profile::unread() })
            }
            fn account<'a>(&'a self, brand: &'a str) -> crate::host::Pending<'a, Result<Option<crate::Account>>> {
                self.0.lock().unwrap().push(brand.to_string());
                Box::pin(async { Ok(Some(crate::Account { owner: "lux".into(), name: "z".into(), token: "T".into() })) })
            }
            fn refresh(&self, _: &str) -> Vec<String> {
                Vec::new()
            }
            fn key(&self, _: crate::Provider) -> Result<Option<String>> {
                Ok(None)
            }
            fn meter<'a>(&'a self, _: &'a str, _: &'a [String]) -> crate::host::Pending<'a, std::result::Result<Value, String>> {
                Box::pin(async { Err("no meter".to_string()) })
            }
        }
        let mock = MockCloud::start().await;
        let root = crate::root::scratch();
        let machine = format!("beatbrand_{}", std::process::id());
        let host = Arc::new(Asked(std::sync::Mutex::new(Vec::new())));
        let held = beat(host.clone(), &root, "lux", &mock.base_url(), &machine, "evo");
        until("the first beat", || !mock.requests().is_empty()).await;
        drop(held);
        assert_eq!(host.0.lock().unwrap().first().map(String::as_str), Some("lux"));
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
    }

    fn answer_of(folders: &[&str]) -> Answer {
        Answer { id: "tgt_1".into(), folders: folders.iter().map(|f| f.to_string()).collect() }
    }

    #[test]
    fn the_notice_names_the_command_only_when_nothing_is_granted() {
        let line = no_folder_notice("tgt_1", &[]).unwrap();
        assert!(line.contains("hanzo link grant"), "{line}");
        assert!(line.contains("tgt_1"), "{line}");
        assert_eq!(no_folder_notice("tgt_1", &["/home/me/site".to_string()]), None);
    }

    #[test]
    fn two_empty_answers_give_one_notice() {
        let g = grants();
        let mut heard = false;
        assert!(heard_answer(&g, &mut heard, answer_of(&[])).is_some());
        assert!(heard_answer(&g, &mut heard, answer_of(&[])).is_none());
        assert_eq!(g.borrow().as_deref(), Some(&[][..]));
    }

    #[test]
    fn a_first_answer_with_a_folder_gives_none_now_or_after_a_revoke() {
        let g = grants();
        let mut heard = false;
        assert!(heard_answer(&g, &mut heard, answer_of(&["/home/me/site"])).is_none());
        assert!(heard_answer(&g, &mut heard, answer_of(&[])).is_none());
        assert_eq!(g.borrow().as_deref(), Some(&[][..]));
    }

    /// Each beat keeps the folders cloud answers as granted, so a revoke lands on the next.
    #[tokio::test]
    async fn a_beat_keeps_the_granted_folders() {
        let mock = MockCloud::start().await;
        mock.grant(&["/home/me/site", "/home/me/notes"]);
        let root = crate::root::scratch();
        let machine = format!("beatgrants_{}", std::process::id());
        let g = grants();
        let held = beat_every(Duration::from_millis(10), Creds::Fixed("T".into()), &root, &mock.base_url(), &machine, "evo", Some(g.clone()));
        until("the grants", || g.borrow().as_ref().is_some_and(|f| f.len() == 2)).await;
        mock.grant(&["/home/me/notes"]);
        until("the revoke", || g.borrow().as_deref() == Some(&["/home/me/notes".to_string()][..])).await;
        drop(held);
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
    }

    #[tokio::test]
    async fn a_beat_states_the_terminal_it_was_given() {
        let mock = MockCloud::start().await;
        let root = crate::root::scratch();
        let machine = format!("beatterm_{}", std::process::id());
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
        let held = beat_every(Duration::from_millis(10), Creds::Fixed("T".into()), &root, &mock.base_url(), &machine, "evo", Some(grants()));
        held.terminal(Some("https://box.share.hanzo.ai"));
        until("the url stated", || mock.requests().iter().any(|r| r.json()["terminal"] == "https://box.share.hanzo.ai")).await;
        let seen = mock.requests().len();
        until("three more beats", || mock.requests().len() >= seen + 3).await;
        drop(held);
        let reqs = mock.requests();
        assert!(
            reqs[seen..].iter().all(|r| r.json()["terminal"] == "https://box.share.hanzo.ai"),
            "every beat after it is set states the terminal",
        );
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
    }

    #[tokio::test]
    async fn a_beat_with_no_terminal_sends_no_key() {
        let mock = MockCloud::start().await;
        let root = crate::root::scratch();
        let machine = format!("beatnoterm_{}", std::process::id());
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
        let held = beat_every(Duration::from_millis(10), Creds::Fixed("T".into()), &root, &mock.base_url(), &machine, "evo", Some(grants()));
        until("three beats", || mock.requests().len() >= 3).await;
        drop(held);
        assert!(
            mock.requests().iter().all(|r| r.json().get("terminal").is_none()),
            "a beat that has no terminal says nothing about it",
        );
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
    }

    #[tokio::test]
    async fn clearing_the_terminal_says_so_once() {
        let mock = MockCloud::start().await;
        let root = crate::root::scratch();
        let machine = format!("beatclear_{}", std::process::id());
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
        let held = beat_every(Duration::from_millis(10), Creds::Fixed("T".into()), &root, &mock.base_url(), &machine, "evo", Some(grants()));
        held.terminal(Some("https://box.share.hanzo.ai"));
        until("the url stated", || mock.requests().iter().any(|r| r.json()["terminal"] == "https://box.share.hanzo.ai")).await;
        held.terminal(None);
        until("the clear", || mock.requests().iter().any(|r| r.json()["terminal"] == "")).await;
        let settled = mock.requests().len();
        until("beats after the clear", || mock.requests().len() >= settled + 3).await;
        drop(held);
        let said: Vec<_> = mock.requests().iter().filter_map(|r| r.json().get("terminal").cloned()).collect();
        assert_eq!(said.iter().filter(|v| *v == "").count(), 1, "cleared once: {said:?}");
        assert_eq!(said.last().unwrap(), "", "nothing after the clear restates the url: {said:?}");
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
    }

    #[tokio::test]
    async fn a_terminal_change_is_said_at_once_and_withdraw_waits_for_it() {
        let mock = MockCloud::start().await;
        let root = crate::root::scratch();
        let machine = format!("beatwithdraw_{}", std::process::id());
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
        let held = beat_every(Duration::from_secs(3600), Creds::Fixed("T".into()), &root, &mock.base_url(), &machine, "evo", Some(grants()));
        until("the first beat", || !mock.requests().is_empty()).await;
        held.terminal(Some("https://box.share.hanzo.ai"));
        until("the url stated within the hour", || mock.requests().iter().any(|r| r.json()["terminal"] == "https://box.share.hanzo.ai")).await;
        held.withdraw().await;
        assert!(mock.requests().iter().any(|r| r.json()["terminal"] == ""), "the clear was heard before withdraw returned");
        drop(held);
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
    }

    /// Letting go stops the beat, and stopping is the whole ending: cloud's window
    /// expires on its own. Nothing sends a goodbye, so nothing can fail to.
    #[tokio::test]
    async fn letting_go_stops_it() {
        let mock = MockCloud::start().await;
        let root = crate::root::scratch();
        let machine = format!("beatdrop_{}", std::process::id());
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));

        let held = beat_every(Duration::from_millis(10), Creds::Fixed("T".into()), &root, &mock.base_url(), &machine, "evo", Some(grants()));
        until("the first beat", || !mock.requests().is_empty()).await;
        drop(held);
        // A beat already in flight may still land, so settle before reading the mark.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let after_drop = mock.requests().len();

        tokio::time::sleep(Duration::from_millis(500)).await; // 50 periods' worth
        assert_eq!(mock.requests().len(), after_drop, "a dropped beat writes nothing more");
        let _ = std::fs::remove_file(crate::context::target_path_for_test(&root, &machine));
    }
}
