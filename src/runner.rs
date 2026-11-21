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
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::context::TargetRecord;
use crate::contract::AgentRun;
use crate::host::Host;
use crate::root::Root;
use crate::sample::Sampler;
use crate::target::{self, Grants};

/// Default wall-clock cap for a single routed run when `timeoutSeconds == 0`.
const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(1800);

/// Client HTTP timeout for `/claim` (server long-polls for 25s).
const CLAIM_HTTP_TIMEOUT: Duration = Duration::from_secs(40);

/// Concurrent local run slots. Keeping >=2 permits means while 1 run executes,
/// the claim loop still holds a free permit and continues long-polling `/claim`
/// every 25s so `serving` stays `true` throughout long runs.
const MAX_CONCURRENT_RUNS: usize = 2;

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
    if let Some(folders) = target::sync(root, api, token, org, machine_id, host_name, &machine).await {
        grants.send_replace(Some(folders));
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

        match client.claim_once(&token, &target_id, &claim_key).await {
            Ok(ClaimPoll::Empty) => {
                drop(permit);
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
                tokio::spawn(async move {
                    let _permit = permit;
                    handle_claimed_run(
                        host,
                        &root,
                        &brand,
                        &api,
                        org.as_deref(),
                        &client,
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
                tokio::time::sleep(Duration::from_secs(3)).await;
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
    target_id: &str,
    initial_claim_key: &str,
    run: RoutedRun,
    grants: &Grants,
    executor: Executor,
) {
    let report = if run.dir.is_empty() {
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
            Err(e) => return failure_report(&run.branch, format!("workspace setup failed: {e}")),
        }
    } else {
        None
    };

    if has_repo {
        let ssh = access.as_ref().map(|a| a.ssh.as_str());
        if let Err(e) = prepare_git_workspace(work_dir, &run.clone_url, &run.base, &run.branch, ssh).await {
            return failure_report(&run.branch, format!("workspace setup failed: {e}"));
        }
        base_sha = git_stdout(work_dir, &["rev-parse", "HEAD"]).await.unwrap_or_default();
    } else {
        // Nothing to clone, so the workspace is an empty repository: the agent will not
        // start in a directory that is not one.
        if let Err(e) = std::fs::create_dir_all(work_dir) {
            return failure_report(&run.branch, format!("creating workspace dir: {e}"));
        }
        if let Err(e) = run_git(Some(work_dir), &["init", "--quiet"]).await {
            return failure_report(&run.branch, format!("workspace setup failed: {e}"));
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

    if has_repo && !run.branch.trim().is_empty() {
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
                    err_msg = push_err.to_string();
                } else {
                    err_msg = format!("{err_msg}; git finalize failed: {push_err}");
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

/// A run's deploy key and the host key it is checked against, on disk for the run:
/// the directory goes when the value does.
struct Access {
    dir: PathBuf,
    ssh: String,
}

impl Access {
    fn make(work_dir: &Path, key: &str, known_hosts: &str) -> Result<Access> {
        let dir = PathBuf::from(format!("{}.ssh", work_dir.display()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).context("creating the key directory")?;
        let mut access = Access { dir, ssh: String::new() };
        let key_file = access.dir.join("key");
        let hosts_file = access.dir.join("known_hosts");
        write_private(&key_file, &format!("{}\n", key.trim_end()))?;
        write_private(&hosts_file, &format!("{}\n", known_hosts.trim_end()))?;
        access.ssh = format!(
            "ssh -i {} -o IdentitiesOnly=yes -o UserKnownHostsFile={} -o StrictHostKeyChecking=yes -o BatchMode=yes",
            quoted(&key_file),
            quoted(&hosts_file),
        );
        Ok(access)
    }
}

impl Drop for Access {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\\', "/").replace('\'', "'\\''"))
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("writing {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

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

async fn prepare_git_workspace(work_dir: &Path, clone_url: &str, base: &str, branch: &str, ssh: Option<&str>) -> Result<()> {
    let base = base.trim();
    let dest = work_dir.to_string_lossy();
    let cloned = if !base.is_empty() {
        clone(ssh, &["--branch", base], clone_url, &dest).await
    } else {
        Err(anyhow!("no explicit base branch"))
    };
    if cloned.is_err() {
        let _ = std::fs::remove_dir_all(work_dir);
        clone(ssh, &[], clone_url, &dest).await.context("git clone failed")?;
        if !base.is_empty() {
            run_git(Some(work_dir), &["checkout", base])
                .await
                .with_context(|| format!("git checkout {base} failed"))?;
        }
    }
    // The key serves the pushes after the clone too, the agent's own among them.
    if let Some(s) = ssh {
        run_git(Some(work_dir), &["config", "core.sshCommand", s]).await?;
    }

    if git_stdout(work_dir, &["config", "user.email"]).await.unwrap_or_default().is_empty() {
        let _ = run_git(Some(work_dir), &["config", "user.email", "dev@hanzo.ai"]).await;
    }
    if git_stdout(work_dir, &["config", "user.name"]).await.unwrap_or_default().is_empty() {
        let _ = run_git(Some(work_dir), &["config", "user.name", "Hanzo"]).await;
    }

    let branch = branch.trim();
    if !branch.is_empty() {
        run_git(Some(work_dir), &["checkout", "-B", branch])
            .await
            .with_context(|| format!("git checkout -B {branch} failed"))?;
    }
    Ok(())
}

/// `git clone`, through the run's own key when it has one.
async fn clone(ssh: Option<&str>, options: &[&str], url: &str, dest: &str) -> Result<String> {
    let through = ssh.map(|s| format!("core.sshCommand={s}"));
    let mut args: Vec<&str> = Vec::new();
    if let Some(t) = &through {
        args.extend(["-c", t]);
    }
    args.push("clone");
    args.extend(options);
    args.extend([url, dest]);
    run_git(None, &args).await
}

async fn finalize_git_changes(work_dir: &Path, base_sha: &str, branch: &str) -> Result<Option<(String, String)>> {
    let status = git_stdout(work_dir, &["status", "--porcelain"]).await?;
    if !status.is_empty() {
        run_git(Some(work_dir), &["add", "-A"]).await?;
        run_git(Some(work_dir), &["commit", "-m", "chore: apply changes from Hanzo run"]).await?;
    }

    let head_sha = git_stdout(work_dir, &["rev-parse", "HEAD"]).await?;
    if head_sha.is_empty() || head_sha == base_sha {
        return Ok(None);
    }

    let refspec = format!("HEAD:refs/heads/{}", branch.trim());
    run_git(Some(work_dir), &["push", "-u", "origin", &refspec])
        .await
        .with_context(|| format!("git push origin {refspec} failed"))?;

    let stat = if !base_sha.is_empty() {
        git_stdout(work_dir, &["diff", "--stat", &format!("{base_sha}..HEAD")])
            .await
            .unwrap_or_default()
    } else {
        String::new()
    };
    Ok(Some((head_sha, stat)))
}

async fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String> {
    let git = crate::exe::find("git").context("git is not installed")?;
    let mut cmd = Command::from(crate::exe::command(git));
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().await.context("spawning git")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        bail!("git {} exited {}: {detail}", args.first().copied().unwrap_or(""), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String> {
    run_git(Some(cwd), args).await
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
mod tests {
    use super::*;
    use crate::testmock::MockCloud;

    struct StubHost {
        api: String,
        org: Option<String>,
    }

    impl Host for StubHost {
        fn profile(&self) -> crate::host::Pending<'_, crate::Profile> {
            let mut p = crate::Profile::unread();
            p.api = self.api.clone();
            p.org = self.org.clone();
            Box::pin(async move { p })
        }
        fn account<'a>(&'a self, _: &'a str) -> crate::host::Pending<'a, Result<Option<crate::Account>>> {
            Box::pin(async {
                Ok(Some(crate::Account {
                    owner: "jamessburnss1313".into(),
                    name: "james".into(),
                    token: "TOK".into(),
                }))
            })
        }
        fn refresh(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn key(&self, _: crate::Provider) -> Result<Option<String>> {
            Ok(None)
        }
        fn meter<'a>(&'a self, _: &'a str, _: &'a [String]) -> crate::host::Pending<'a, std::result::Result<serde_json::Value, String>> {
            Box::pin(async { Err("no meter".into()) })
        }
    }

    async fn until(what: &str, f: impl Fn() -> bool) {
        for _ in 0..3000 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn runner_mints_key_claims_clones_commits_pushes_and_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let seed = tmp.path().join("seed");

        run_git(None, &["init", "--bare", "--initial-branch=main", &origin.to_string_lossy()]).await.unwrap();
        run_git(None, &["clone", &origin.to_string_lossy(), &seed.to_string_lossy()]).await.unwrap();
        run_git(Some(&seed), &["config", "user.email", "test@hanzo.ai"]).await.unwrap();
        run_git(Some(&seed), &["config", "user.name", "Test"]).await.unwrap();
        std::fs::write(seed.join("README.md"), "hello\n").unwrap();
        run_git(Some(&seed), &["add", "."]).await.unwrap();
        run_git(Some(&seed), &["commit", "-m", "initial"]).await.unwrap();
        run_git(Some(&seed), &["push", "-u", "origin", "HEAD:refs/heads/main"]).await.unwrap();

        let queued = RoutedRun {
            session_id: "sess_routed_1".into(),
            repo: "jamessburnss1313-dev/guided-growth".into(),
            project: String::new(),
            base: "main".into(),
            branch: "hanzo/fix-landing".into(),
            prompt: "Update README".into(),
            clone_url: origin.to_string_lossy().to_string(),
            timeout_seconds: 60,
            model: "enso-auto".into(),
            effort: "medium".into(),
            instructions: String::new(),
            deploy_key: Secret::default(),
            known_hosts: String::new(),
            dir: String::new(),
        };

        let mock = MockCloud::start_with_routed_runs(vec![queued], true).await;
        let root = crate::root::Root::at(tmp.path().join("hanzo-root"));
        let host = Arc::new(StubHost {
            api: mock.base_url(),
            org: Some("jamessburnss1313".into()),
        });

        let exec: Executor = Arc::new(|run, cwd| {
            Box::pin(async move {
                assert_eq!(run.session_id, "sess_routed_1");
                std::fs::write(cwd.join("README.md"), "hello from live runner\n")?;
                Ok(None)
            })
        });

        let held = serve_with(host, &root, "hanzo", &mock.base_url(), "mach_1", "James-MacBook-Air.local", target::grants(), exec);
        until("routed run report to land", || {
            mock.requests().iter().any(|r| r.method == "POST" && r.path == "/v1/agent/targets/tgt_mock/runs/sess_routed_1/report")
        })
        .await;
        drop(held);

        let reqs = mock.requests();
        let key_mints = reqs.iter().filter(|r| r.method == "POST" && r.path == "/v1/agent/targets/tgt_mock/key").count();
        assert!(key_mints >= 2, "must mint initial key and re-mint after simulated 403 rotation");

        let report_req = reqs
            .iter()
            .find(|r| r.method == "POST" && r.path == "/v1/agent/targets/tgt_mock/runs/sess_routed_1/report")
            .unwrap();
        assert_eq!(report_req.header("x-org-id").as_deref(), Some("jamessburnss1313"));
        assert_eq!(report_req.header("x-target-key").as_deref(), Some("ck_fresh"));
        let body = report_req.json();
        assert_eq!(body["ok"], true);
        assert_eq!(body["changed"], true);
        assert_eq!(body["branch"], "hanzo/fix-landing");
        assert!(!body["commitSha"].as_str().unwrap_or_default().is_empty());
        assert!(body["diffstat"].as_str().unwrap_or_default().contains("README.md"));

        let pushed_sha = run_git(Some(&origin), &["rev-parse", "refs/heads/hanzo/fix-landing"]).await.unwrap();
        assert_eq!(pushed_sha, body["commitSha"].as_str().unwrap());

        let commit = run_git(Some(&origin), &["log", "-1", "--format=%an|%s", "refs/heads/hanzo/fix-landing"]).await.unwrap();
        let (author, subject) = commit.split_once('|').unwrap();
        assert_ne!(author, "Hanzo Dev");
        assert_eq!(subject, "chore: apply changes from Hanzo run");
    }

    // A run with its own deploy key clones and pushes through it, and the key is gone
    // with the run. A local path stands in for the forge: git never asks it for ssh.
    #[tokio::test]
    async fn a_runs_deploy_key_is_held_for_the_run_and_removed_after() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        run_git(None, &["init", "--bare", "--initial-branch=main", &origin.to_string_lossy()]).await.unwrap();
        let seed = tmp.path().join("seed");
        run_git(None, &["clone", &origin.to_string_lossy(), &seed.to_string_lossy()]).await.unwrap();
        run_git(Some(&seed), &["config", "user.email", "t@hanzo.ai"]).await.unwrap();
        run_git(Some(&seed), &["config", "user.name", "T"]).await.unwrap();
        std::fs::write(seed.join("a"), "a\n").unwrap();
        run_git(Some(&seed), &["add", "."]).await.unwrap();
        run_git(Some(&seed), &["commit", "-m", "init"]).await.unwrap();
        run_git(Some(&seed), &["push", "origin", "HEAD:refs/heads/main"]).await.unwrap();

        let work_dir = tmp.path().join("workspace");
        let keys = tmp.path().join("workspace.ssh");
        let run = RoutedRun {
            session_id: "sess_key_1".into(),
            repo: "api".into(),
            project: String::new(),
            base: "main".into(),
            branch: "agent/key".into(),
            prompt: "x".into(),
            clone_url: origin.to_string_lossy().to_string(),
            timeout_seconds: 30,
            model: String::new(),
            effort: String::new(),
            instructions: String::new(),
            deploy_key: serde_json::from_str("\"PRIVATE-KEY\"").unwrap(),
            known_hosts: "git.test ssh-ed25519 AAAA".into(),
            dir: String::new(),
        };
        let seen = keys.clone();
        let exec: Executor = Arc::new(move |_run, cwd| {
            let keys = seen.clone();
            Box::pin(async move {
                let key = std::fs::read_to_string(keys.join("key"))?;
                anyhow::ensure!(key.trim() == "PRIVATE-KEY", "the key was not written");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(keys.join("key"))?.permissions().mode() & 0o777;
                    anyhow::ensure!(mode == 0o600, "the key is mode {mode:o}");
                }
                let through = git_stdout(&cwd, &["config", "core.sshCommand"]).await?;
                anyhow::ensure!(through.contains("IdentitiesOnly=yes") && through.contains("StrictHostKeyChecking=yes"), "{through}");
                std::fs::write(cwd.join("b"), "b\n")?;
                Ok(None)
            })
        });
        let report = execute_workspace_run(&work_dir, &run, exec).await;
        assert!(report.ok, "{}", report.error);
        assert!(report.changed);
        assert!(!keys.exists(), "the key outlived its run");
        assert_eq!(format!("{:?}", run.deploy_key), "Secret(..)");
        assert!(!format!("{run:?}").contains("PRIVATE-KEY"));
    }

    // A run with no repository still works in a directory the agent accepts: `dev`
    // (codex exec) refuses one that is not inside a git work tree and exits at once, which is
    // how every bare run on a linked machine ended in an error nobody read.
    #[tokio::test]
    async fn a_run_without_a_repository_works_inside_a_git_work_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = tmp.path().join("workspace");
        let run = RoutedRun {
            session_id: "sess_bare_1".into(),
            repo: String::new(),
            project: String::new(),
            base: String::new(),
            branch: String::new(),
            prompt: "Say ok".into(),
            clone_url: String::new(),
            timeout_seconds: 30,
            model: "enso-auto".into(),
            effort: "medium".into(),
            instructions: String::new(),
            deploy_key: Secret::default(),
            known_hosts: String::new(),
            dir: String::new(),
        };
        let exec: Executor = Arc::new(|_run, cwd| {
            Box::pin(async move {
                let inside = git_stdout(&cwd, &["rev-parse", "--is-inside-work-tree"]).await?;
                anyhow::ensure!(inside == "true", "the workspace is not a git work tree: {inside:?}");
                Ok(None)
            })
        });
        let report = execute_workspace_run(&work_dir, &run, exec).await;
        assert!(report.ok, "a bare run failed: {}", report.error);
        assert!(!report.changed);
    }

    #[tokio::test]
    async fn execute_workspace_run_propagates_valid_agent_run_telemetry() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = tmp.path().join("workspace");
        let run = RoutedRun {
            session_id: "sess_telemetry_1".into(),
            repo: String::new(),
            project: String::new(),
            base: String::new(),
            branch: String::new(),
            prompt: "Verify telemetry".into(),
            clone_url: String::new(),
            timeout_seconds: 30,
            model: "enso-auto".into(),
            effort: "medium".into(),
            instructions: String::new(),
            deploy_key: Secret::default(),
            known_hosts: String::new(),
            dir: String::new(),
        };
        let exec: Executor = Arc::new(|r, _cwd| {
            Box::pin(async move {
                let mut telemetry = AgentRun::new(
                    &r.session_id,
                    &r.session_id,
                    "0.1.0",
                    &r.model,
                    true,
                    crate::summary::url(&r.session_id),
                );
                telemetry.execution.steps = 2;
                telemetry.execution.tool_calls = 3;
                telemetry.execution.elapsed_ms = 250;
                telemetry.usage.input_tokens = 100;
                telemetry.usage.output_tokens = 40;
                telemetry.usage.cost_usd = 0.001;
                telemetry.verification.checks_passed = true;
                Ok(Some(telemetry))
            })
        });

        let report = execute_workspace_run(&work_dir, &run, exec).await;
        assert!(report.ok);
        let telemetry = report.telemetry.as_ref().expect("telemetry should be populated");
        assert!(telemetry.validate().passed);

        let serialized = serde_json::to_value(&report).unwrap();
        let check = crate::contract::validate_agent_run_json(&serialized["telemetry"]);
        assert!(check.passed, "violations: {:?}", check.violations);
        assert_eq!(serialized["telemetry"]["runId"], "sess_telemetry_1");
        assert_eq!(serialized["telemetry"]["execution"]["toolCalls"], 3);
    }

    fn folder_run(dir: &Path) -> RoutedRun {
        RoutedRun {
            session_id: "sess_folder_1".into(),
            repo: String::new(),
            project: String::new(),
            base: String::new(),
            branch: String::new(),
            prompt: "Tidy the notes".into(),
            clone_url: String::new(),
            timeout_seconds: 30,
            model: String::new(),
            effort: String::new(),
            instructions: String::new(),
            deploy_key: Secret::default(),
            known_hosts: String::new(),
            dir: dir.to_string_lossy().into_owned(),
        }
    }

    fn granted(folders: &[&Path]) -> Grants {
        let g = target::grants();
        g.send_replace(Some(folders.iter().map(|f| f.to_string_lossy().into_owned()).collect()));
        g
    }

    fn refused(r: Result<PathBuf>) -> bool {
        r.is_err_and(|e| e.to_string().starts_with(FOLDER_NOT_GRANTED))
    }

    // A grant covers itself and what lies under it, and nothing beside it.
    #[test]
    fn a_grant_holds_its_folder_and_what_is_under_it() {
        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("site");
        std::fs::create_dir_all(site.join("src")).unwrap();
        std::fs::create_dir_all(tmp.path().join("site2")).unwrap();
        let grant = [site.to_string_lossy().into_owned()];
        let real = std::fs::canonicalize(&site).unwrap();
        assert_eq!(inside(&site, &grant).unwrap(), real);
        assert_eq!(inside(&site.join("src"), &grant).unwrap(), real.join("src"));
        assert!(refused(inside(&tmp.path().join("site2"), &grant)), "a sibling sharing a prefix is not inside");
        assert!(refused(inside(tmp.path(), &grant)), "a parent is not inside");
        assert!(refused(inside(Path::new("site/src"), &grant)), "a relative dir is not inside");
        assert!(refused(inside(&site, &[])), "no grant, no folder");
    }

    // A symlink inside a grant that points out of it is refused; one pointing in is not.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_grant_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("site");
        let secrets = tmp.path().join("secrets");
        std::fs::create_dir_all(site.join("src")).unwrap();
        std::fs::create_dir_all(&secrets).unwrap();
        std::os::unix::fs::symlink(&secrets, site.join("out")).unwrap();
        std::os::unix::fs::symlink(site.join("src"), tmp.path().join("in")).unwrap();
        let grant = [site.to_string_lossy().into_owned()];
        assert!(refused(inside(&site.join("out"), &grant)));
        assert!(refused(inside(&site.join("out").join("."), &grant)));
        assert_eq!(inside(&tmp.path().join("in"), &grant).unwrap(), std::fs::canonicalize(site.join("src")).unwrap());
    }

    // `..` is resolved before the check: climbing out is refused, climbing back in is not.
    #[test]
    fn dot_dot_cannot_climb_out_of_a_grant() {
        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("site");
        std::fs::create_dir_all(site.join("src")).unwrap();
        std::fs::create_dir_all(tmp.path().join("other")).unwrap();
        let grant = [site.to_string_lossy().into_owned()];
        assert!(refused(inside(&site.join("..").join("other"), &grant)));
        assert!(refused(inside(&site.join(".."), &grant)));
        assert_eq!(inside(&site.join("src").join(".."), &grant).unwrap(), std::fs::canonicalize(&site).unwrap());
    }

    // A folder that is not on disk is refused, and a grant that is not on disk grants nothing.
    #[test]
    fn a_missing_folder_or_grant_grants_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(site.join("notes.md"), "x").unwrap();
        let grant = [site.to_string_lossy().into_owned()];
        assert!(refused(inside(&site.join("gone"), &grant)));
        assert!(refused(inside(&site.join("notes.md"), &grant)), "a file is not a folder to work in");
        let gone = [tmp.path().join("gone").to_string_lossy().into_owned()];
        assert!(refused(inside(&site, &gone)));
    }

    // A run in a granted folder works there in place: no clone, no repository made, and
    // what the agent wrote stays.
    #[tokio::test]
    async fn a_run_in_a_granted_folder_works_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("site");
        std::fs::create_dir_all(&site).unwrap();
        let real = std::fs::canonicalize(&site).unwrap();
        let exec: Executor = Arc::new(move |_run, cwd| {
            let real = real.clone();
            Box::pin(async move {
                anyhow::ensure!(cwd == real, "the run worked in {}", cwd.display());
                std::fs::write(cwd.join("notes.md"), "done\n")?;
                Ok(None)
            })
        });
        let report = execute_folder_run(&folder_run(&site), &granted(&[&site]), exec).await;
        assert!(report.ok, "{}", report.error);
        assert!(!report.changed);
        assert_eq!(std::fs::read_to_string(site.join("notes.md")).unwrap(), "done\n");
        assert!(!site.join(".git").exists(), "the folder was made a repository");
    }

    // A run outside every grant runs nothing and reports folder_not_granted.
    #[tokio::test]
    async fn a_run_outside_every_grant_runs_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("site");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = ran.clone();
        let exec: Executor = Arc::new(move |_run, _cwd| {
            seen.store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(None) })
        });
        for grants in [granted(&[&site]), target::grants()] {
            let report = execute_folder_run(&folder_run(&other), &grants, exec.clone()).await;
            assert!(!report.ok);
            assert!(report.error.starts_with(FOLDER_NOT_GRANTED), "{}", report.error);
        }
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst), "a refused run executed");
    }

    // A beat that no longer carries the run's folder stops the run at once.
    #[tokio::test]
    async fn revoking_the_folder_stops_the_run() {
        struct Dropped(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("site");
        let keep = tmp.path().join("keep");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::create_dir_all(&keep).unwrap();
        let grants = granted(&[&site, &keep]);
        let started = Arc::new(tokio::sync::Notify::new());
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (go, flag) = (started.clone(), stopped.clone());
        let exec: Executor = Arc::new(move |_run, _cwd| {
            let (go, guard) = (go.clone(), Dropped(flag.clone()));
            Box::pin(async move {
                let _guard = guard;
                go.notify_one();
                std::future::pending::<()>().await;
                Ok(None)
            })
        });
        let run = folder_run(&site);
        let revoke = async {
            started.notified().await;
            grants.send_replace(Some(vec![keep.to_string_lossy().into_owned()]));
        };
        let (report, ()) = tokio::join!(execute_folder_run(&run, &grants, exec), revoke);
        assert!(!report.ok);
        assert!(report.error.starts_with(FOLDER_NOT_GRANTED) && report.error.contains("revoked"), "{}", report.error);
        assert!(stopped.load(std::sync::atomic::Ordering::SeqCst), "the agent outlived the revoke");
    }

    // The runner learns the granted folders from cloud's answer and checks a claimed run's
    // dir against them: one inside runs, one outside is reported folder_not_granted.
    #[tokio::test]
    async fn the_runner_checks_a_claimed_runs_folder_against_the_beat() {
        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("site");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mut outside = folder_run(&other);
        outside.session_id = "sess_folder_2".into();
        let mock = MockCloud::start_with_routed_runs(vec![folder_run(&site), outside], false).await;
        mock.grant(&[&site.to_string_lossy()]);
        let root = crate::root::Root::at(tmp.path().join("hanzo-root"));
        let host = Arc::new(StubHost { api: mock.base_url(), org: None });
        let exec: Executor = Arc::new(|run, _cwd| {
            Box::pin(async move {
                anyhow::ensure!(run.session_id == "sess_folder_1", "{} ran", run.session_id);
                Ok(None)
            })
        });
        let held = serve_with(host, &root, "hanzo", &mock.base_url(), "mach_1", "evo", target::grants(), exec);
        let report = |id: &str| {
            let path = format!("/v1/agent/targets/tgt_mock/runs/{id}/report");
            mock.requests().into_iter().find(|r| r.path == path).map(|r| r.json())
        };
        until("both reports", || report("sess_folder_1").is_some() && report("sess_folder_2").is_some()).await;
        drop(held);
        assert_eq!(report("sess_folder_1").unwrap()["ok"], true);
        let denied = report("sess_folder_2").unwrap();
        assert_eq!(denied["ok"], false);
        assert!(denied["error"].as_str().unwrap().starts_with(FOLDER_NOT_GRANTED), "{denied}");
    }
}
