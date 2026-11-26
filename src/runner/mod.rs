//! Live routed-run runner for linked machines (`/v1/agent/targets/:id/{key,claim,runs/:runId/report}`).
//!
//! Heartbeating (`target::beat`) proves a machine is `online`; long-polling
//! `POST /v1/agent/targets/:id/claim` proves a runner is listening (`serving: true`)
//! and executes routed `hanzo.app > Dev` runs locally in an isolated git checkout,
//! or, for a run that names a `dir`, in place in that folder when it lies inside one
//! the machine's owner granted (hanzo-inc/app#310).
//! Cloud's `routedFinalizer` owns closing the session after `/report` lands, so
//! the runner streams live events to `sessionId` without setting a terminal status.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::context::TargetRecord;
use crate::contract::AgentRun;
use crate::host::Host;
use crate::root::Root;
use crate::sample::Sampler;
use crate::target::{self, Grants};

/// Default wall-clock cap for a single routed run when `timeoutSeconds == 0` (4 hours,
/// matching Cloud's 4-hour `dev`/`desktop` lease ceiling).
pub(crate) const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(14_400);

/// Client HTTP timeout for `/claim` (server long-polls for 25s).
const CLAIM_HTTP_TIMEOUT: Duration = Duration::from_secs(40);

/// Concurrent local run slots. Keeping >=2 permits means while 1 run executes,
/// the claim loop still holds a free permit and continues long-polling `/claim`
/// every 25s so `serving` stays `true` throughout long runs.
const MAX_CONCURRENT_RUNS: usize = 2;

/// Pause before the next claim after one that failed, or that came back empty
/// sooner than `HELD` (the server did not hold the call).
const RETRY_WAIT: Duration = Duration::from_secs(3);

/// A claim the server held at least this long was a real long poll.
const HELD: Duration = Duration::from_secs(1);

/// Clamp limits matching `cloud/apps/agents/routing_http.go`.
const MAX_SHA_CHARS: usize = 128;
const MAX_TEXT_BYTES: usize = 60 * 1024;

/// The outcome a run in a folder outside every grant ends with, as cloud names it.
pub const FOLDER_NOT_GRANTED: &str = "folder_not_granted";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RoutedRun {
    pub session_id: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub clone_url: String,
    #[serde(default)]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub instructions: String,
    /// The run's own key to its repository, when the repository is one this machine
    /// holds no credential for, and the forge's host key to check it against. Held for
    /// the run and not after it.
    #[serde(default)]
    pub deploy_key: Secret,
    #[serde(default)]
    pub known_hosts: String,
    /// The folder the run works in, in place: no clone, no push, nothing removed after.
    /// Empty is a run in its own checkout.
    #[serde(default)]
    pub dir: String,
    /// Contain this ONE run in a read-only workspace: no writes to the granted
    /// folder or checkout, no commit, no push, no PR. The harness is refused if it
    /// cannot build the OS guard. NOT the same as `deploy_key` being read-only.
    #[serde(default)]
    pub read_only: bool,
}

/// A secret that a debug line leaves out.
#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    pub ok: bool,
    pub changed: bool,
    pub branch: String,
    pub commit_sha: String,
    pub diffstat: String,
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<AgentRun>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimKeyOut {
    claim_key: String,
}

#[derive(Debug)]
enum ClaimPoll {
    Empty,
    Forbidden,
    Run(Box<RoutedRun>),
}

pub type BoxExecFuture = Pin<Box<dyn Future<Output = Result<Option<AgentRun>>> + Send>>;
pub type Executor = Arc<dyn Fn(RoutedRun, PathBuf) -> BoxExecFuture + Send + Sync>;

/// Guard holding the background claim-and-execute loop open until dropped.
pub struct Runner(tokio::task::JoinHandle<()>);

impl Drop for Runner {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawn a live routed-run runner for this machine.
pub fn serve(
    host: Arc<dyn Host>,
    root: &Root,
    brand: &str,
    api: &str,
    machine_id: &str,
    name: &str,
    grants: Grants,
) -> Runner {
    let exec_host = host.clone();
    let exec_root = root.clone();
    let exec_brand = brand.to_string();
    let executor: Executor = Arc::new(move |run: RoutedRun, cwd: PathBuf| {
        let host = exec_host.clone();
        let root = exec_root.clone();
        let brand = exec_brand.clone();
        Box::pin(async move {
            crate::run_routed_agent(
                host,
                &root,
                &brand,
                &cwd,
                &run.session_id,
                &run.prompt,
                Some(run.model.as_str()),
                Some(run.effort.as_str()),
                Some(run.instructions.as_str()),
                run.read_only,
            )
            .await
        })
    });
    serve_with(host, root, brand, api, machine_id, name, grants, executor)
}

#[allow(clippy::too_many_arguments)]
pub fn serve_with(
    host: Arc<dyn Host>,
    root: &Root,
    brand: &str,
    api: &str,
    machine_id: &str,
    name: &str,
    grants: Grants,
    executor: Executor,
) -> Runner {
    let root = root.clone();
    let brand = brand.to_string();
    let api = api.trim_end_matches('/').to_string();
    let machine_id = machine_id.to_string();
    let name = name.to_string();
    let handle = tokio::spawn(run_loop(host, root, brand, api, machine_id, name, grants, executor));
    Runner(handle)
}

#[derive(Clone)]
pub struct RunnerClient {
    http: reqwest::Client,
    api: String,
    org: Option<String>,
}

impl RunnerClient {
    pub fn new(api: &str, org: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(CLAIM_HTTP_TIMEOUT)
            .user_agent(concat!("hanzo-gen/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building runner http client")?;
        Ok(Self {
            http,
            api: api.trim_end_matches('/').to_string(),
            org,
        })
    }

    fn auth(&self, rb: reqwest::RequestBuilder, token: &str, claim_key: Option<&str>) -> reqwest::RequestBuilder {
        let mut rb = rb.bearer_auth(token);
        if let Some(org) = &self.org {
            rb = rb.header("X-Org-Id", org);
        }
        if let Some(key) = claim_key {
            rb = rb.header("X-Target-Key", key);
        }
        rb
    }

    pub async fn mint_key(&self, token: &str, target_id: &str) -> Result<String> {
        let url = format!("{}/v1/agent/targets/{target_id}/key", self.api);
        let resp = self
            .auth(self.http.post(&url), token, None)
            .json(&json!({}))
            .send()
            .await
            .context("minting target claim key")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("mint claim key returned {status}: {body}");
        }
        let out: ClaimKeyOut = resp.json().await.context("decoding claim key")?;
        if out.claim_key.trim().is_empty() {
            bail!("mint claim key returned an empty key");
        }
        Ok(out.claim_key)
    }

    async fn claim_once(&self, token: &str, target_id: &str, claim_key: &str) -> Result<ClaimPoll> {
        let url = format!("{}/v1/agent/targets/{target_id}/claim", self.api);
        let resp = self
            .auth(self.http.post(&url), token, Some(claim_key))
            .json(&json!({}))
            .send()
            .await
            .context("claiming routed run")?;
        let status = resp.status();
        if status.as_u16() == 204 {
            return Ok(ClaimPoll::Empty);
        }
        if status.as_u16() == 403 || status.as_u16() == 404 {
            return Ok(ClaimPoll::Forbidden);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("claim returned {status}: {body}");
        }
        let text = resp.text().await.context("reading claimed run body")?;
        if text.trim().is_empty() || text.trim() == "null" {
            return Ok(ClaimPoll::Empty);
        }
        let run: RoutedRun = serde_json::from_str(&text).context("decoding claimed routed run")?;
        Ok(ClaimPoll::Run(Box::new(run)))
    }

    pub async fn report(&self, token: &str, target_id: &str, run_id: &str, claim_key: &str, report: &RunReport) -> Result<u16> {
        let url = format!("{}/v1/agent/targets/{target_id}/runs/{run_id}/report", self.api);
        let resp = self
            .auth(self.http.post(&url), token, Some(claim_key))
            .json(report)
            .send()
            .await
            .context("reporting routed run")?;
        Ok(resp.status().as_u16())
    }
}

fn claim_key_path(root: &Root, api: &str, org: Option<&str>, target_id: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(api.as_bytes());
    hasher.update(b"\0");
    hasher.update(org.unwrap_or_default().as_bytes());
    hasher.update(b"\0");
    hasher.update(target_id.as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    root.data().join("targets").join(format!("claim-{}.key", &hex[..24]))
}

fn load_claim_key(root: &Root, api: &str, org: Option<&str>, target_id: &str) -> Option<String> {
    let p = claim_key_path(root, api, org, target_id);
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_claim_key(root: &Root, api: &str, org: Option<&str>, target_id: &str, key: &str) {
    let p = claim_key_path(root, api, org, target_id);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = crate::private::write(&p, key.as_bytes());
}

/// This machine's target id, registering when none is stored or no answer has said
/// which folders are granted yet.
#[allow(clippy::too_many_arguments)]
async fn resolve_target_id(
    root: &Root,
    api: &str,
    token: &str,
    org: Option<&str>,
    machine_id: &str,
    host_name: &str,
    sampler: &mut Sampler,
    grants: &Grants,
) -> Option<String> {
    if let Some(rec) = TargetRecord::load(root, machine_id)
        .ok()
        .flatten()
        .filter(|r| r.host == host_name && r.api == api && grants.borrow().is_some())
    {
        return Some(rec.id);
    }
    sync(root, api, token, org, machine_id, host_name, sampler, grants).await;
    TargetRecord::load(root, machine_id)
        .ok()
        .flatten()
        .filter(|r| r.host == host_name && r.api == api)
        .map(|r| r.id)
}

/// [`target::sync`], keeping the granted folders it answers.
#[allow(clippy::too_many_arguments)]
async fn sync(root: &Root, api: &str, token: &str, org: Option<&str>, machine_id: &str, host_name: &str, sampler: &mut Sampler, grants: &Grants) {
    let machine = sampler.machine().await;
    if let Some(answer) = target::sync(root, api, token, org, machine_id, host_name, &machine).await {
        grants.send_replace(Some(answer.folders));
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    host: Arc<dyn Host>,
    root: Root,
    brand: String,
    api: String,
    machine_id: String,
    host_name: String,
    grants: Grants,
    executor: Executor,
) {
    let org = host.profile().await.org;
    let client = match RunnerClient::new(&api, org.clone()) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("runner client unavailable ({e})");
            return;
        }
    };
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_RUNS));
    let mut sampler = Sampler::new();
    let mut cached_key: Option<(String, String)> = None;

    loop {
        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return,
        };

        let Some(token) = host
            .account(&brand)
            .await
            .ok()
            .flatten()
            .map(|a| a.token)
        else {
            drop(permit);
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };

        let Some(target_id) = resolve_target_id(
            &root,
            &api,
            &token,
            org.as_deref(),
            &machine_id,
            &host_name,
            &mut sampler,
            &grants,
        )
        .await
        else {
            drop(permit);
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };

        let claim_key = match &cached_key {
            Some((id, k)) if id == &target_id => k.clone(),
            _ => match load_claim_key(&root, &api, org.as_deref(), &target_id) {
                Some(k) => {
                    cached_key = Some((target_id.clone(), k.clone()));
                    k
                }
                None => match client.mint_key(&token, &target_id).await {
                    Ok(k) => {
                        save_claim_key(&root, &api, org.as_deref(), &target_id, &k);
                        cached_key = Some((target_id.clone(), k.clone()));
                        k
                    }
                    Err(e) => {
                        tracing::debug!("could not mint target claim key ({e})");
                        drop(permit);
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                },
            },
        };

        let started = std::time::Instant::now();
        match client.claim_once(&token, &target_id, &claim_key).await {
            Ok(ClaimPoll::Empty) => {
                drop(permit);
                if started.elapsed() < HELD {
                    tokio::time::sleep(RETRY_WAIT).await;
                }
            }
            Ok(ClaimPoll::Forbidden) => {
                drop(permit);
                sync(&root, &api, &token, org.as_deref(), &machine_id, &host_name, &mut sampler, &grants).await;
                let refreshed_id = TargetRecord::load(&root, &machine_id)
                    .ok()
                    .flatten()
                    .filter(|r| r.host == host_name && r.api == api)
                    .map(|r| r.id)
                    .unwrap_or(target_id);
                match client.mint_key(&token, &refreshed_id).await {
                    Ok(k) => {
                        save_claim_key(&root, &api, org.as_deref(), &refreshed_id, &k);
                        cached_key = Some((refreshed_id, k));
                    }
                    Err(e) => {
                        tracing::debug!("claim key re-mint failed ({e})");
                        cached_key = None;
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
            Ok(ClaimPoll::Run(run)) => {
                let host = host.clone();
                let root = root.clone();
                let brand = brand.clone();
                let api = api.clone();
                let org = org.clone();
                let client = client.clone();
                let executor = executor.clone();
                let grants = grants.clone();
                let machine_id = machine_id.clone();
                let host_name = host_name.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    handle_claimed_run(
                        host,
                        &root,
                        &brand,
                        &api,
                        org.as_deref(),
                        &client,
                        &machine_id,
                        &host_name,
                        &target_id,
                        &claim_key,
                        *run,
                        &grants,
                        executor,
                    )
                    .await;
                });
            }
            Err(e) => {
                tracing::debug!("target claim poll failed ({e})");
                drop(permit);
                tokio::time::sleep(RETRY_WAIT).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_claimed_run(
    host: Arc<dyn Host>,
    root: &Root,
    brand: &str,
    api: &str,
    org: Option<&str>,
    client: &RunnerClient,
    machine_id: &str,
    host_name: &str,
    target_id: &str,
    initial_claim_key: &str,
    run: RoutedRun,
    grants: &Grants,
    executor: Executor,
) {
    // Refuse BEFORE touching a checkout or the granted folder when this harness
    // has no way to contain writes. An agent must never be started writable with
    // a read-only flag on the wire.
    let report = if run.read_only && !readonly::supported() {
        failure_report(&run.branch, format!("{}: this harness has no read-only workspace sandbox", readonly::READ_ONLY_UNSUPPORTED))
    } else if run.dir.is_empty() {
        let run_slug = sanitize_id(&run.session_id);
        let work_dir = root.data().join("runs").join(format!(
            "{}-{:08x}",
            if run_slug.is_empty() { "run" } else { &run_slug },
            rand::random::<u32>()
        ));
        let report = execute_workspace_run(&work_dir, &run, executor).await;
        let _ = std::fs::remove_dir_all(&work_dir);
        report
    } else {
        // The grants are as old as the last beat, and `hanzo link grant` signals nothing:
        // a folder the runner cannot place is asked about once before it is refused.
        if inside(Path::new(&run.dir), grants.borrow().as_deref().unwrap_or_default()).is_err() {
            if let Some(token) = host.account(brand).await.ok().flatten().map(|a| a.token) {
                sync(root, api, &token, org, machine_id, host_name, &mut Sampler::new(), grants).await;
            }
        }
        execute_folder_run(&run, grants, executor).await
    };

    let mut key = load_claim_key(root, api, org, target_id).unwrap_or_else(|| initial_claim_key.to_string());
    for attempt in 0..3 {
        let Some(token) = host.account(brand).await.ok().flatten().map(|a| a.token) else {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        match client.report(&token, target_id, &run.session_id, &key, &report).await {
            Ok(code) if (200..300).contains(&code) => break,
            Ok(403) if attempt == 0 => {
                if let Ok(fresh_key) = client.mint_key(&token, target_id).await {
                    save_claim_key(root, api, org, target_id, &fresh_key);
                    key = fresh_key;
                    continue;
                }
            }
            Ok(code) => {
                tracing::debug!("report run {} returned HTTP {code} (attempt {})", run.session_id, attempt + 1);
            }
            Err(e) => {
                tracing::debug!("report run {} failed: {e} (attempt {})", run.session_id, attempt + 1);
            }
        }
        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
    }
}

pub async fn execute_workspace_run(work_dir: &Path, run: &RoutedRun, executor: Executor) -> RunReport {
    if let Some(parent) = work_dir.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return failure_report(&run.branch, format!("creating runs dir: {e}"));
        }
    }
    let _ = std::fs::remove_dir_all(work_dir);

    let has_repo = !run.clone_url.trim().is_empty();
    let mut base_sha = String::new();

    let access = if has_repo && !run.deploy_key.0.is_empty() {
        match Access::make(work_dir, &run.deploy_key.0, &run.known_hosts) {
            Ok(a) => Some(a),
            Err(e) => return failure_report(&run.branch, format!("workspace setup failed: {e:#}")),
        }
    } else {
        None
    };

    if has_repo {
        let ssh = access.as_ref().map(|a| a.ssh.as_str());
        if let Err(e) = prepare_git_workspace(work_dir, &run.clone_url, &run.base, &run.branch, ssh).await {
            return failure_report(&run.branch, format!("workspace setup failed: {e:#}"));
        }
        base_sha = git_stdout(work_dir, &["rev-parse", "HEAD"]).await.unwrap_or_default();
    } else {
        // Nothing to clone, so the workspace is an empty repository: the agent will not
        // start in a directory that is not one.
        if let Err(e) = std::fs::create_dir_all(work_dir) {
            return failure_report(&run.branch, format!("creating workspace dir: {e}"));
        }
        if let Err(e) = run_git(Some(work_dir), &["init", "--quiet"]).await {
            return failure_report(&run.branch, format!("workspace setup failed: {e:#}"));
        }
    }

    let mut ok = true;
    let mut err_msg = String::new();
    let mut telemetry = None;
    match agent(run, work_dir, executor).await {
        Ok(t) => telemetry = t,
        Err(e) => {
            ok = false;
            err_msg = e.to_string();
        }
    }

    let mut changed = false;
    let mut commit_sha = String::new();
    let mut diffstat = String::new();

    if run.read_only {
        // No commit, no push, no PR — ever. The OS guard keeps the agent from
        // writing the checkout; this second check fails the run closed if one
        // slipped through, instead of reporting a false "worked".
        if let Ok(status) = git_stdout(work_dir, &["status", "--porcelain"]).await {
            if !status.trim().is_empty() {
                ok = false;
                err_msg = if err_msg.is_empty() {
                    "read_only_violation: the checkout changed during a read-only run".to_string()
                } else {
                    format!("{err_msg}; read_only_violation: the checkout changed during a read-only run")
                };
            }
        }
    } else if has_repo && !run.branch.trim().is_empty() {
        match finalize_git_changes(work_dir, &base_sha, &run.branch).await {
            Ok(Some((sha, stat))) => {
                changed = true;
                commit_sha = clamp_chars(&sha, MAX_SHA_CHARS);
                diffstat = clamp_bytes(&stat, MAX_TEXT_BYTES);
            }
            Ok(None) => {}
            Err(push_err) => {
                ok = false;
                if err_msg.is_empty() {
                    err_msg = format!("{push_err:#}");
                } else {
                    err_msg = format!("{err_msg}; git finalize failed: {push_err:#}");
                }
            }
        }
    }

    RunReport {
        ok,
        changed,
        branch: run.branch.clone(),
        commit_sha,
        diffstat,
        error: clamp_bytes(&err_msg, MAX_TEXT_BYTES),
        telemetry,
    }
}

/// The agent on `run` in `cwd`, ended at the run's time limit.
async fn agent(run: &RoutedRun, cwd: &Path, executor: Executor) -> Result<Option<AgentRun>> {
    let timeout = if run.timeout_seconds > 0 {
        Duration::from_secs(run.timeout_seconds)
    } else {
        DEFAULT_RUN_TIMEOUT
    };
    match tokio::time::timeout(timeout, executor(run.clone(), cwd.to_path_buf())).await {
        Ok(out) => out,
        Err(_) => bail!("run timed out after {}s", timeout.as_secs()),
    }
}

/// Run `run` in place in its `dir`, which must lie inside a folder in `grants`; the
/// run is stopped the moment a beat answers that its folder is no longer granted.
pub async fn execute_folder_run(run: &RoutedRun, grants: &Grants, executor: Executor) -> RunReport {
    let mut granted = grants.subscribe();
    let dir = match inside(Path::new(&run.dir), granted.borrow_and_update().as_deref().unwrap_or_default()) {
        Ok(dir) => dir,
        Err(e) => return failure_report(&run.branch, e.to_string()),
    };
    let revoked = async {
        while granted.changed().await.is_ok() {
            if inside(&dir, granted.borrow_and_update().as_deref().unwrap_or_default()).is_err() {
                return;
            }
        }
        std::future::pending::<()>().await
    };
    let out = tokio::select! {
        out = agent(run, &dir, executor) => out,
        () = revoked => Err(anyhow!("{FOLDER_NOT_GRANTED}: the folder {} was revoked, and the run was stopped", run.dir)),
    };
    match out {
        Ok(telemetry) => RunReport {
            ok: true,
            changed: false,
            branch: run.branch.clone(),
            commit_sha: String::new(),
            diffstat: String::new(),
            error: String::new(),
            telemetry,
        },
        Err(e) => failure_report(&run.branch, e.to_string()),
    }
}

/// The real path of `dir` when it is a directory that is, or lies under, a granted
/// folder. Both sides are real paths (symlinks, `..` and `/proc` links resolved), so
/// no spelling escapes a grant, and a grant missing from disk grants nothing.
pub fn inside(dir: &Path, folders: &[String]) -> Result<PathBuf> {
    let refused = || anyhow!("{FOLDER_NOT_GRANTED}: the folder {} is not inside one this machine's owner granted", dir.display());
    let real = Some(dir)
        .filter(|d| d.is_absolute())
        .and_then(|d| std::fs::canonicalize(d).ok())
        .filter(|d| d.is_dir())
        .ok_or_else(refused)?;
    folders
        .iter()
        .map(Path::new)
        .filter(|f| f.is_absolute())
        .filter_map(|f| std::fs::canonicalize(f).ok())
        .any(|f| real.starts_with(f))
        .then_some(real)
        .ok_or_else(refused)
}

mod git;
use git::*;
pub(crate) mod readonly;

fn failure_report(branch: &str, err: String) -> RunReport {
    RunReport {
        ok: false,
        changed: false,
        branch: branch.to_string(),
        commit_sha: String::new(),
        diffstat: String::new(),
        error: clamp_bytes(&err, MAX_TEXT_BYTES),
        telemetry: None,
    }
}


fn sanitize_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}

fn clamp_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn clamp_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests;
