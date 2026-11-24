//! Portable-session data: the "where it runs" context snapshot and the
//! machine-local resume store.
//!
//! PRIVACY (hard line): the snapshot is cwd / repo / ref / host / os / arch /
//! machine-id ONLY. It is built from explicit system sources, never from a
//! secret- or token-bearing environment variable, so no credential can leak into
//! it. Machine capability + metrics read only consts + `/proc` + cleared-env probe
//! stdout (`Machine::capture`); the sole environment read is the hostname fallback
//! ($HOSTNAME/$COMPUTERNAME when the `hostname` command is absent — a machine name,
//! never a secret). Git remote URLs are scrubbed of embedded credentials.

use crate::root::Root;
use anyhow::{Context, Result};
use crate::private;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// A repository's identity at snapshot time (all fields optional / best-effort).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Repo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Origin remote URL, credentials stripped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

impl Repo {
    pub fn is_empty(&self) -> bool {
        *self == Repo::default()
    }

    /// Best-effort git identity of `cwd` (empty when it is not a repo).
    pub fn capture(cwd: &Path) -> Repo {
        git_repo(cwd)
    }
}

/// The "where it runs" snapshot shown per session in mission-control. Contains
/// NO secrets — only location + identity fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(rename = "machineId")]
    pub machine_id: String,
    pub host: String,
    pub os: String,
    pub arch: String,
    pub cwd: String,
    pub backend: String,
    #[serde(rename = "backendVersion", skip_serializing_if = "Option::is_none")]
    pub backend_version: Option<String>,
    #[serde(skip_serializing_if = "Repo::is_empty")]
    pub repo: Repo,
}

impl Snapshot {
    /// Capture the snapshot for `cwd` running `backend` (`backend_version`
    /// best-effort). Reads only explicit sources — never the environment.
    pub fn capture(root: &Root, cwd: &Path, backend: &str, backend_version: Option<String>) -> Snapshot {
        Snapshot {
            machine_id: machine_id(root),
            host: hostname(root),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cwd: cwd.display().to_string(),
            backend: backend.to_string(),
            backend_version,
            repo: git_repo(cwd),
        }
    }

    /// The `status`-kind event payload: the snapshot tagged as the session's
    /// context record (optionally noting the session it was resumed from).
    pub fn context_payload(&self, resumed_from: Option<&str>) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        v["type"] = json!("context");
        if let Some(from) = resumed_from {
            v["resumedFrom"] = json!(from);
        }
        v
    }
}

// ---- machine capability plane (the cloud run-target: /v1/agent/targets) ----
//
// A linked machine reports what it IS (Spec) and what it is DOING now (Metrics) so
// mission-control can show which computer a session runs on and whether it can take
// more work. These are the SAME wire value types as cloud's `agents.Spec` /
// `agents.GPU` / `agents.Metrics`; the server sanitizes/bounds every field, so we
// send best-effort values and never fail. Captured from explicit system sources
// ONLY (/proc, sysctl, nvidia-smi, lspci, system_profiler) — never the environment.

/// One accelerator on the machine. Matches cloud's `agents.GPU`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    /// nvidia | amd | apple | intel | …
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vendor: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// VRAM bytes, 0 = unknown.
    #[serde(default)]
    pub memory: i64,
}

/// A machine's STATIC capability — what it IS. Matches cloud's `agent.Spec`.
/// Every field is best-effort; an unknown value stays 0 / empty.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spec {
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    /// Total RAM, bytes (0 = unknown).
    pub memory: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<Gpu>,
}

/// A machine's LIVE sample — what it is DOING now. Matches cloud's `agent.Metrics`
/// MINUS `at`: the server owns the staleness clock and stamps it, so a client can
/// never forge or backdate a heartbeat. The field names ARE the wire names, and the
/// console reads targets back into this same type, so an absent field is `None`
/// (unknown), never a zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Metrics {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub mem_used: i64,
    pub mem_free: i64,
    pub gpu_util: f64,
    /// Busy fraction of all cores over the sample window, 0..1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_util: Option<f64>,
    /// CPU package temperature, °C.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_temp: Option<f64>,
    /// Hottest GPU, °C.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_temp: Option<f64>,
    /// Total GPU power draw, W.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_power: Option<f64>,
    /// Total enforced GPU power limit, W.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_power_limit: Option<f64>,
    /// Memory held by GPU processes, bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_mem_used: Option<i64>,
    /// Dedicated VRAM, bytes; `None` when the GPU shares system memory (unified).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_mem_total: Option<i64>,
    /// Used bytes across local block filesystems.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_used: Option<i64>,
    /// Used plus available bytes on those filesystems (df's size, less reserved blocks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_total: Option<i64>,
    /// Block-device read rate, bytes/s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_read: Option<f64>,
    /// Block-device write rate, bytes/s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_write: Option<f64>,
    /// Receive rate over physical interfaces, bytes/s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_rx: Option<f64>,
    /// Transmit rate over physical interfaces, bytes/s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_tx: Option<f64>,
    /// Model id(s) served on this machine, comma-separated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Generation throughput over the sample window, tokens/s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode: Option<f64>,
    /// Prompt (prefill) throughput over the sample window, tokens/s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefill: Option<f64>,
    /// Mean time to first token of requests that started in the window, seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft: Option<f64>,
    /// Requests being served now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running: Option<i64>,
    /// Requests queued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<i64>,
    /// KV-cache occupancy, 0..1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kv_cache: Option<f64>,
}

/// The machine a coding session runs on: its static capability (`spec`) and its
/// live sample (`metrics`), captured together so one GPU probe feeds both.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Machine {
    pub spec: Spec,
    pub metrics: Metrics,
}

impl Machine {
    /// Capture this machine's capability + live sample, best-effort. Every probe
    /// that is absent, fails, or times out yields 0/empty; NONE can block or fail
    /// the caller. Reads only explicit system sources — never the environment.
    pub async fn capture() -> Machine {
        let (gpus, gpu_util, board) = gpus().await;
        let (total, used, free) = memory().await;
        let (load1, load5, load15) = loadavg().await;
        let mut machine = Machine {
            spec: Spec {
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                cpus: cpus(),
                memory: total,
                gpus,
            },
            metrics: Metrics {
                load1: finite(load1),
                load5: finite(load5),
                load15: finite(load15),
                mem_used: used,
                mem_free: free,
                gpu_util: finite(gpu_util),
                ..Default::default()
            },
        };
        board.apply(&mut machine.metrics);
        machine
    }
}

impl Spec {
    /// A short human capacity summary for mission-control, e.g.
    /// "20 vCPU / 128G / 1× GB10". Unknown parts (no cpus/memory/GPU) drop out.
    pub fn capacity(&self) -> String {
        let mut parts = Vec::new();
        if self.cpus > 0 {
            parts.push(format!("{} vCPU", self.cpus));
        }
        if self.memory > 0 {
            parts.push(human_bytes(self.memory));
        }
        if !self.gpus.is_empty() {
            parts.push(gpu_summary(&self.gpus));
        }
        parts.join(" / ")
    }
}


mod probes;
pub(crate) use probes::probe;
use probes::*;

/// Resolve a stable, privacy-clean machine id: a random id generated once and
/// cached under the Hanzo data dir. We deliberately do NOT read system machine
/// identifiers (which can be sensitive) — a per-install random id is enough to
/// group a machine's sessions and to gate same-machine resume.
pub fn machine_id(root: &Root) -> String {
    let path = root.data().join("machine-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let id = existing.trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let id = hex(&bytes);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = private::write(&path, id.as_bytes());
    id
}

/// What this machine calls itself. THE one hostname in the CLI — a session, a
/// run-target and a snapshot must agree about which computer they are on, and
/// two probes with two fallbacks is the way they come to disagree.
///
/// Never empty, and never the word "unknown". Every source here can fail (no
/// `hostname` binary, an exported-but-empty `$HOSTNAME`), and an empty host is
/// the one value the console cannot group: those rows pile into a single
/// "unknown machine" bucket that no amount of looking can take apart. So the
/// last resort is the stable per-install [`machine_id`] — an opaque name is a
/// far weaker fact than `evo`, but it is still a fact, and it groups.
pub fn hostname(root: &Root) -> String {
    crate::exe::find("hostname")
        .and_then(|bin| crate::exe::command(bin).output().ok())
        .filter(|o| o.status.success())
        .and_then(|o| named(&String::from_utf8_lossy(&o.stdout)))
        .or_else(|| std::env::var("HOSTNAME").ok().and_then(|h| named(&h)))
        .or_else(|| std::env::var("COMPUTERNAME").ok().and_then(|h| named(&h)))
        .unwrap_or_else(|| machine_id(root))
}

/// A candidate that actually NAMES something, trimmed — or nothing. Every source
/// of a hostname can hand back a blank (a `hostname` binary that prints an empty
/// line, an exported-but-empty `$HOSTNAME`), and a blank passed on as if it were
/// a name is how a machine ends up unidentifiable rather than merely unnamed.
fn named(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Best-effort git context for `cwd`. A non-repo yields an empty [`Repo`].
fn git_repo(cwd: &Path) -> Repo {
    let git = |args: &[&str]| -> Option<String> {
        // `git -C <cwd>` reads config from a possibly-untrusted working tree.
        // Neutralize the system/global gitconfig (a planted `core.fsmonitor`,
        // pager, alias or `include.path` there could otherwise run a command)
        // and never let git block on a credential/terminal prompt.
        let out = crate::exe::command(crate::exe::find("git")?)
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    // Not a repo -> nothing to record.
    if git(&["rev-parse", "--is-inside-work-tree"]).as_deref() != Some("true") {
        return Repo::default();
    }
    Repo {
        root: git(&["rev-parse", "--show-toplevel"]),
        remote: git(&["remote", "get-url", "origin"]).map(|u| scrub_remote(&u)),
        branch: git(&["rev-parse", "--abbrev-ref", "HEAD"]),
        head: git(&["rev-parse", "HEAD"]),
    }
}

/// Strip any embedded credentials (`https://user:token@host/…`) from a remote
/// URL. scp-like remotes (`git@github.com:o/r.git`) carry no secret and are
/// left as-is. Never let a token ride along in a recorded/streamed remote.
pub fn scrub_remote(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) if !u.username().is_empty() || u.password().is_some() => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        _ => url.to_string(),
    }
}

// ---- machine-local resume store ----

/// A resumable session's machine-local record. Non-secret (ids, paths, ref).
/// This is the machine's authoritative handle for a native `--resume`; the same
/// fields are mirrored onto the cloud session record (as status events) so a
/// future web "continue" control can re-dispatch. See the coordinator's seam.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeRecord {
    #[serde(rename = "cloudSessionId")]
    pub cloud_session_id: String,
    /// The identity (`owner/name`) that created the cloud session, so a resume
    /// can tell whether the ACTIVE identity may re-attach to it. The cloud
    /// session is org-scoped server-side; this local note is what lets the CLI
    /// decide honestly instead of discovering it as a 403. Defaulted (empty =
    /// unknown provenance) so records written before this field still load.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub identity: String,
    pub backend: String,
    /// The backend's OWN session/thread id — what its native `--resume` needs.
    #[serde(rename = "backendSessionId")]
    pub backend_session_id: String,
    pub cwd: String,
    pub api: String,
    #[serde(rename = "machineId")]
    pub machine_id: String,
    #[serde(default, skip_serializing_if = "Repo::is_empty")]
    pub repo: Repo,
    /// Best-effort pointer to the backend's local transcript (bytes stay local;
    /// cross-machine upload is a documented follow-on).
    #[serde(rename = "transcriptPath", skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
}

impl ResumeRecord {
    /// The `status`-kind event payload mirroring the resume handle to cloud.
    pub fn resume_payload(&self) -> Value {
        json!({
            "type": "resume",
            "backend": self.backend,
            "backendSessionId": self.backend_session_id,
            "machineId": self.machine_id,
            "cwd": self.cwd,
            "transcriptPath": self.transcript_path,
        })
    }

    pub fn save(&self, root: &Root) -> Result<()> {
        let path = record_path(root, &self.cloud_session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating resume store dir")?;
        }
        let json = serde_json::to_vec_pretty(self).context("serializing resume record")?;
        private::write(&path, &json).context("writing resume record")
    }

    pub fn load(root: &Root, cloud_session_id: &str) -> Result<Option<ResumeRecord>> {
        let path = record_path(root, cloud_session_id);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("parsing resume record")?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading resume record"),
        }
    }
}

/// The machine-local record of THIS machine's cloud run-target: the id cloud minted
/// on register, so a later run refreshes (heartbeats) the SAME target instead of
/// piling up duplicates. Keyed by machine (one file per install). Reuse is gated on
/// a matching host + api so a copied data dir or a renamed host re-registers rather
/// than clobbering another machine's target. Non-secret (id / host / api).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetRecord {
    pub id: String,
    pub host: String,
    #[serde(rename = "machineId")]
    pub machine_id: String,
    pub api: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

impl TargetRecord {
    pub fn load(root: &Root, machine_id: &str) -> Result<Option<TargetRecord>> {
        match std::fs::read(target_path(root, machine_id)) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("parsing target record")?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading target record"),
        }
    }

    pub fn save(&self, root: &Root) -> Result<()> {
        let path = target_path(root, &self.machine_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating target store dir")?;
        }
        let json = serde_json::to_vec_pretty(self).context("serializing target record")?;
        private::write(&path, &json).context("writing target record")
    }
}

/// Reduce an id to a safe single-segment filename stem (ascii alnum / `_` / `-`).
fn safe_name(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect()
}

fn record_path(root: &Root, cloud_session_id: &str) -> PathBuf {
    // The id is cloud-minted (`sess_<hex>`); guard the filename regardless.
    root.data().join("code").join("sessions").join(format!("{}.json", safe_name(cloud_session_id)))
}

fn target_path(root: &Root, machine_id: &str) -> PathBuf {
    root.data().join("code").join("targets").join(format!("{}.json", safe_name(machine_id)))
}

/// The target-record path, exposed for test cleanup only.
#[cfg(test)]
pub(crate) fn target_path_for_test(root: &Root, machine_id: &str) -> PathBuf {
    target_path(root, machine_id)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests;
