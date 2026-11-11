//! Live routed-run runner for linked machines (`/v1/agent/targets/:id/{key,claim,runs/:runId/report}`).
//!
//! Heartbeating (`target::beat`) proves a machine is `online`; long-polling
//! `POST /v1/agent/targets/:id/claim` proves a runner is listening (`serving: true`)
//! and executes routed `hanzo.app > Dev` runs locally in an isolated git checkout.
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
use crate::host::Host;
use crate::root::Root;
use crate::sample::Sampler;
use crate::target;

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    pub ok: bool,
    pub changed: bool,
    pub branch: String,
    pub commit_sha: String,
    pub diffstat: String,
    pub error: String,
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
    Run(RoutedRun),
}

pub type BoxExecFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
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
    serve_with(host, root, brand, api, machine_id, name, executor)
}

pub fn serve_with(
    host: Arc<dyn Host>,
    root: &Root,
    brand: &str,
    api: &str,
    machine_id: &str,
    name: &str,
    executor: Executor,
) -> Runner {
    let root = root.clone();
    let brand = brand.to_string();
    let api = api.trim_end_matches('/').to_string();
    let machine_id = machine_id.to_string();
    let name = name.to_string();
    let handle = tokio::spawn(run_loop(host, root, brand, api, machine_id, name, executor));
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
        Ok(ClaimPoll::Run(run))
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

async fn resolve_target_id(
    root: &Root,
    api: &str,
    token: &str,
    org: Option<&str>,
    machine_id: &str,
    host_name: &str,
    sampler: &mut Sampler,
) -> Option<String> {
    if let Some(rec) = TargetRecord::load(root, machine_id)
        .ok()
        .flatten()
        .filter(|r| r.host == host_name && r.api == api)
    {
        return Some(rec.id);
    }
    let machine = sampler.machine().await;
    target::sync(root, api, token, org, machine_id, host_name, &machine).await;
    TargetRecord::load(root, machine_id)
        .ok()
        .flatten()
        .filter(|r| r.host == host_name && r.api == api)
        .map(|r| r.id)
}

async fn run_loop(
    host: Arc<dyn Host>,
    root: Root,
    brand: String,
    api: String,
    machine_id: String,
    host_name: String,
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
                let machine = sampler.machine().await;
                target::sync(&root, &api, &token, org.as_deref(), &machine_id, &host_name, &machine).await;
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
                        run,
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
    executor: Executor,
) {
    let run_slug = sanitize_id(&run.session_id);
    let work_dir = root.data().join("runs").join(format!(
        "{}-{:08x}",
        if run_slug.is_empty() { "run" } else { &run_slug },
        rand::random::<u32>()
    ));

    let report = execute_workspace_run(&work_dir, &run, executor).await;
    let _ = std::fs::remove_dir_all(&work_dir);

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

    if has_repo {
        if let Err(e) = prepare_git_workspace(work_dir, &run.clone_url, &run.base, &run.branch).await {
            return failure_report(&run.branch, format!("workspace setup failed: {e}"));
        }
        base_sha = git_stdout(work_dir, &["rev-parse", "HEAD"]).await.unwrap_or_default();
    } else if let Err(e) = std::fs::create_dir_all(work_dir) {
        return failure_report(&run.branch, format!("creating workspace dir: {e}"));
    }

    let timeout = if run.timeout_seconds > 0 {
        Duration::from_secs(run.timeout_seconds)
    } else {
        DEFAULT_RUN_TIMEOUT
    };

    let mut ok = true;
    let mut err_msg = String::new();
    match tokio::time::timeout(timeout, executor(run.clone(), work_dir.to_path_buf())).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            ok = false;
            err_msg = e.to_string();
        }
        Err(_) => {
            ok = false;
            err_msg = format!("run timed out after {}s", timeout.as_secs());
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
    }
}

fn failure_report(branch: &str, err: String) -> RunReport {
    RunReport {
        ok: false,
        changed: false,
        branch: branch.to_string(),
        commit_sha: String::new(),
        diffstat: String::new(),
        error: clamp_bytes(&err, MAX_TEXT_BYTES),
    }
}

async fn prepare_git_workspace(work_dir: &Path, clone_url: &str, base: &str, branch: &str) -> Result<()> {
    let base = base.trim();
    let cloned = if !base.is_empty() {
        run_git(None, &["clone", "--branch", base, clone_url, &work_dir.to_string_lossy()]).await
    } else {
        Err(anyhow!("no explicit base branch"))
    };
    if cloned.is_err() {
        let _ = std::fs::remove_dir_all(work_dir);
        run_git(None, &["clone", clone_url, &work_dir.to_string_lossy()])
            .await
            .context("git clone failed")?;
        if !base.is_empty() {
            run_git(Some(work_dir), &["checkout", base])
                .await
                .with_context(|| format!("git checkout {base} failed"))?;
        }
    }

    if git_stdout(work_dir, &["config", "user.email"]).await.unwrap_or_default().is_empty() {
        let _ = run_git(Some(work_dir), &["config", "user.email", "dev@hanzo.ai"]).await;
    }
    if git_stdout(work_dir, &["config", "user.name"]).await.unwrap_or_default().is_empty() {
        let _ = run_git(Some(work_dir), &["config", "user.name", "Hanzo Dev"]).await;
    }

    let branch = branch.trim();
    if !branch.is_empty() {
        run_git(Some(work_dir), &["checkout", "-B", branch])
            .await
            .with_context(|| format!("git checkout -B {branch} failed"))?;
    }
    Ok(())
}

async fn finalize_git_changes(work_dir: &Path, base_sha: &str, branch: &str) -> Result<Option<(String, String)>> {
    let status = git_stdout(work_dir, &["status", "--porcelain"]).await?;
    if !status.is_empty() {
        run_git(Some(work_dir), &["add", "-A"]).await?;
        run_git(Some(work_dir), &["commit", "-m", "chore: apply changes from Hanzo Dev run"]).await?;
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
                Ok(())
            })
        });

        let held = serve_with(host, &root, "hanzo", &mock.base_url(), "mach_1", "James-MacBook-Air.local", exec);
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
    }
}
