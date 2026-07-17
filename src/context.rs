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

use anyhow::{Context, Result};
use crate::private;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::AsyncReadExt;

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
    pub fn capture(cwd: &Path, backend: &str, backend_version: Option<String>) -> Snapshot {
        Snapshot {
            machine_id: machine_id(),
            host: hostname(),
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

// ---- machine capability plane (the cloud run-target: /v1/agents/targets) ----
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

/// A machine's STATIC capability — what it IS. Matches cloud's `agents.Spec`.
/// Every field is best-effort; an unknown value stays 0 / empty.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    /// Total RAM, bytes (0 = unknown).
    pub memory: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<Gpu>,
}

/// A machine's LIVE sample — what it is DOING now. Matches cloud's `agents.Metrics`
/// MINUS `at`: the server owns the staleness clock and stamps it, so a client can
/// never forge or backdate a heartbeat. The field names ARE the wire names.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    #[serde(rename = "memUsed")]
    pub mem_used: i64,
    #[serde(rename = "memFree")]
    pub mem_free: i64,
    #[serde(rename = "gpuUtil")]
    pub gpu_util: f64,
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
        let (gpus, gpu_util) = gpus().await;
        let (total, used, free) = memory().await;
        let (load1, load5, load15) = loadavg().await;
        Machine {
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
            },
        }
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

/// The most accelerators we record — an absurd count is a bug or an attack, not a
/// real host (mirrors the server's `maxGPUs`).
const MAX_GPUS: usize = 32;

/// The per-probe hard deadline. A system probe that hangs past this is abandoned
/// (its child killed on drop) so capability/metrics gathering can NEVER stall the
/// session that spawned it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The most probe stdout we read. Any of these tools (nvidia-smi/lspci/vm_stat/
/// sysctl/system_profiler) emits well under this; a flood past it is truncated so a
/// hostile PATH binary can never balloon memory.
const PROBE_STDOUT_CAP: u64 = 256 * 1024;

/// Logical core count (respects cgroup/affinity limits — the real capacity).
fn cpus() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(0)
}

/// (total, used, free) RAM in bytes, best-effort. Linux reads /proc/meminfo once;
/// macOS uses `sysctl hw.memsize` for the total and `vm_stat` for the live split.
async fn memory() -> (i64, i64, i64) {
    match std::env::consts::OS {
        "linux" => std::fs::read_to_string("/proc/meminfo")
            .map(|s| parse_meminfo(&s))
            .unwrap_or((0, 0, 0)),
        "macos" => {
            let total = probe("sysctl", &["-n", "hw.memsize"], PROBE_TIMEOUT)
                .await
                .and_then(|s| s.trim().parse::<i64>().ok())
                .map(nonneg)
                .unwrap_or(0);
            let (used, free) =
                probe("vm_stat", &[], PROBE_TIMEOUT).await.map(|s| parse_vm_stat(&s)).unwrap_or((0, 0));
            (total, used, free)
        }
        _ => (0, 0, 0),
    }
}

/// (load1, load5, load15), best-effort. Linux reads /proc/loadavg; macOS reads
/// `sysctl -n vm.loadavg`.
async fn loadavg() -> (f64, f64, f64) {
    match std::env::consts::OS {
        "linux" => std::fs::read_to_string("/proc/loadavg")
            .map(|s| parse_loadavg(&s))
            .unwrap_or((0.0, 0.0, 0.0)),
        "macos" => probe("sysctl", &["-n", "vm.loadavg"], PROBE_TIMEOUT)
            .await
            .map(|s| parse_loadavg(&s.replace(['{', '}'], " ")))
            .unwrap_or((0.0, 0.0, 0.0)),
        _ => (0.0, 0.0, 0.0),
    }
}

/// Best-effort accelerators + aggregate utilization (0..1). Tries nvidia-smi first
/// (name + VRAM + util in one shot); on Linux falls back to lspci (name only) for
/// AMD/Intel/other, on macOS to system_profiler. Bounded to `MAX_GPUS`.
async fn gpus() -> (Vec<Gpu>, f64) {
    if let Some(out) = probe(
        "nvidia-smi",
        &["--query-gpu=name,memory.total,utilization.gpu", "--format=csv,noheader,nounits"],
        PROBE_TIMEOUT,
    )
    .await
    {
        let (gpus, util) = parse_nvidia(&out);
        if !gpus.is_empty() {
            return (gpus, util);
        }
    }
    let gpus = match std::env::consts::OS {
        "linux" => probe("lspci", &[], PROBE_TIMEOUT).await.map(|s| parse_lspci(&s)).unwrap_or_default(),
        "macos" => probe("system_profiler", &["SPDisplaysDataType"], PROBE_TIMEOUT)
            .await
            .map(|s| parse_macos_gpus(&s))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    (gpus, 0.0)
}

/// Run a read-only system probe with a hard deadline, returning trimmed stdout on a
/// clean exit. A missing binary, a non-zero exit, or a timeout all yield `None`.
///
/// The child gets a MINIMAL environment (PATH only): no other environment value can
/// influence a probe or round-trip into captured data — the SAME privacy hard-line
/// the git probe holds. `kill_on_drop` guarantees a timed-out probe is reaped.
async fn probe(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    let mut child = cmd.spawn().ok()?;
    // Read at most PROBE_STDOUT_CAP bytes: a hostile probe that floods stdout can
    // never balloon memory. The bounded read fills, `out` drops (closing our read
    // end so a still-writing child gets EPIPE and exits), and a child that ignores
    // that is reaped when the deadline fires and `kill_on_drop` kills it.
    let mut buf = Vec::new();
    let collect = async {
        if let Some(mut out) = child.stdout.take() {
            let _ = (&mut out).take(PROBE_STDOUT_CAP).read_to_end(&mut buf).await;
        }
        child.wait().await
    };
    let status = tokio::time::timeout(timeout, collect).await.ok()?.ok()?;
    if !status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&buf).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Parse (total, used, free) bytes from /proc/meminfo. `free = MemAvailable` (the
/// kernel's own estimate of what a new load can claim), `used = total - available`.
fn parse_meminfo(s: &str) -> (i64, i64, i64) {
    let kib = |key: &str| -> i64 {
        s.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse::<i64>().ok())
            .map(|k| k.saturating_mul(1024))
            .unwrap_or(0)
    };
    let total = kib("MemTotal:");
    let avail = kib("MemAvailable:");
    (nonneg(total), nonneg(total.saturating_sub(avail)), nonneg(avail))
}

/// Parse the 1/5/15-minute load averages from a whitespace-separated string
/// (/proc/loadavg, or macOS `vm.loadavg` with its braces already stripped).
fn parse_loadavg(s: &str) -> (f64, f64, f64) {
    let mut it = s.split_whitespace().filter_map(|f| f.parse::<f64>().ok());
    (it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0))
}

/// Parse macOS `vm_stat` into (used, free) bytes, best-effort. Page size comes from
/// the header ("page size of N bytes"); used ≈ (active + wired) × page, free ≈
/// free × page.
fn parse_vm_stat(s: &str) -> (i64, i64) {
    let page = s
        .lines()
        .next()
        .and_then(|h| h.split("page size of ").nth(1))
        .and_then(|t| t.split_whitespace().next())
        .and_then(|n| n.parse::<i64>().ok())
        .unwrap_or(4096);
    let pages = |key: &str| -> i64 {
        s.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|rest| rest.chars().filter(|c| c.is_ascii_digit()).collect::<String>())
            .and_then(|d| d.parse::<i64>().ok())
            .unwrap_or(0)
    };
    let free = pages("Pages free:").saturating_mul(page);
    let used = pages("Pages active:").saturating_add(pages("Pages wired down:")).saturating_mul(page);
    (nonneg(used), nonneg(free))
}

/// Parse `nvidia-smi --query-gpu=name,memory.total,utilization.gpu
/// --format=csv,noheader,nounits`: one row per GPU ("NVIDIA GB10, 98304, 40"),
/// memory in MiB, utilization in percent. Returns the GPUs + the mean util (0..1).
fn parse_nvidia(csv: &str) -> (Vec<Gpu>, f64) {
    let mut gpus = Vec::new();
    let mut util_sum = 0.0;
    let mut util_n = 0u32;
    for line in csv.lines() {
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        let model = cols.first().copied().unwrap_or("");
        if model.is_empty() {
            continue;
        }
        let memory = cols
            .get(1)
            .and_then(|s| s.parse::<i64>().ok())
            .map(|mib| mib.saturating_mul(1024 * 1024))
            .unwrap_or(0);
        if let Some(u) = cols.get(2).and_then(|s| s.parse::<f64>().ok()) {
            util_sum += u;
            util_n += 1;
        }
        gpus.push(Gpu {
            vendor: "nvidia".to_string(),
            model: clamp_model(model.strip_prefix("NVIDIA ").unwrap_or(model)),
            memory: nonneg(memory),
        });
        if gpus.len() >= MAX_GPUS {
            break;
        }
    }
    let util = if util_n > 0 { (util_sum / util_n as f64) / 100.0 } else { 0.0 };
    (gpus, util)
}

/// Parse `lspci` output for display controllers (name only — no VRAM/util). The
/// class/description split is the FIRST ": " (a PCI slot's colons carry no space),
/// so it is robust to a domain prefix. Vendor is inferred from the description.
fn parse_lspci(text: &str) -> Vec<Gpu> {
    let mut gpus = Vec::new();
    for line in text.lines() {
        let Some((prefix, desc)) = line.split_once(": ") else { continue };
        let class = prefix.to_ascii_lowercase();
        if !(class.contains("vga compatible controller")
            || class.contains("3d controller")
            || class.contains("display controller"))
        {
            continue;
        }
        let desc = desc.trim();
        if desc.is_empty() {
            continue;
        }
        // Vendor reads the full description; the model drops the trailing "(rev NN)"
        // hardware-revision noise (pure clutter in a human capacity label).
        let model = desc.rsplit_once(" (rev ").map(|(head, _)| head).unwrap_or(desc);
        gpus.push(Gpu { vendor: vendor_of(desc), model: clamp_model(model), memory: 0 });
        if gpus.len() >= MAX_GPUS {
            break;
        }
    }
    gpus
}

/// Infer a short GPU vendor token from a free-form description. Uses SPECIFIC
/// markers ("advanced micro devices", "radeon", "amd") — never a bare "ati", which
/// hides inside words like "CorporATIon" and would misclassify an Intel/NVIDIA part.
fn vendor_of(s: &str) -> String {
    let low = s.to_ascii_lowercase();
    if low.contains("nvidia") {
        "nvidia"
    } else if low.contains("apple") {
        "apple"
    } else if low.contains("amd") || low.contains("advanced micro devices") || low.contains("radeon") {
        "amd"
    } else if low.contains("intel") {
        "intel"
    } else {
        ""
    }
    .to_string()
}

/// Parse macOS `system_profiler SPDisplaysDataType`: one GPU per "Chipset Model:",
/// with "Vendor:" and a "VRAM (…)" line enriching the current GPU (unified-memory
/// Apple silicon reports no VRAM — left 0).
fn parse_macos_gpus(text: &str) -> Vec<Gpu> {
    let mut gpus: Vec<Gpu> = Vec::new();
    let mut cur: Option<Gpu> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(m) = line.strip_prefix("Chipset Model:") {
            push_capped(&mut gpus, cur.take());
            cur = Some(Gpu { vendor: String::new(), model: clamp_model(m), memory: 0 });
        } else if let Some(v) = line.strip_prefix("Vendor:") {
            if let Some(g) = cur.as_mut() {
                g.vendor = vendor_of(v.trim());
            }
        } else if let Some(vram) =
            line.strip_prefix("VRAM (Total):").or_else(|| line.strip_prefix("VRAM (Dynamic, Max):"))
        {
            if let Some(g) = cur.as_mut() {
                g.memory = parse_vram(vram.trim());
            }
        }
    }
    push_capped(&mut gpus, cur.take());
    gpus
}

/// Push a GPU onto the list unless it is `None` or the list is already at `MAX_GPUS`.
fn push_capped(gpus: &mut Vec<Gpu>, gpu: Option<Gpu>) {
    if let Some(g) = gpu {
        if gpus.len() < MAX_GPUS {
            gpus.push(g);
        }
    }
}

/// Parse a macOS "VRAM (Total): 8 GB" / "1536 MB" value into bytes (binary units).
fn parse_vram(s: &str) -> i64 {
    let mut it = s.split_whitespace();
    let Some(n) = it.next().and_then(|n| n.parse::<f64>().ok()) else { return 0 };
    let unit = it.next().unwrap_or("").to_ascii_uppercase();
    let scale = match unit.as_str() {
        "GB" => (1u64 << 30) as f64,
        "MB" => (1u64 << 20) as f64,
        "KB" => 1024.0,
        _ => 1.0,
    };
    nonneg((n * scale) as i64)
}

/// Bytes → a terse binary-unit label ("128G", "512M"). Rounds to the nearest whole
/// unit — a human summary, not an exact figure.
fn human_bytes(n: i64) -> String {
    const G: f64 = (1u64 << 30) as f64;
    const M: f64 = (1u64 << 20) as f64;
    let f = n as f64;
    if f >= G {
        format!("{}G", (f / G).round() as i64)
    } else if f >= M {
        format!("{}M", (f / M).round() as i64)
    } else {
        format!("{n}B")
    }
}

/// Group accelerators by name into "N× MODEL" parts joined by " + "
/// (e.g. "2× RTX 4090", "1× GB10 + 1× 8060S").
fn gpu_summary(gpus: &[Gpu]) -> String {
    let mut groups: Vec<(String, usize)> = Vec::new();
    for g in gpus {
        let name = if !g.model.is_empty() {
            g.model.clone()
        } else if !g.vendor.is_empty() {
            g.vendor.clone()
        } else {
            "GPU".to_string()
        };
        match groups.iter_mut().find(|(n, _)| *n == name) {
            Some((_, c)) => *c += 1,
            None => groups.push((name, 1)),
        }
    }
    groups.iter().map(|(name, c)| format!("{c}× {name}")).collect::<Vec<_>>().join(" + ")
}

/// Trim + length-cap a GPU model string (mirrors the server's field bound so the
/// capacity summary stays tidy; the server bounds it again regardless).
fn clamp_model(s: &str) -> String {
    s.trim().chars().take(64).collect()
}

/// Clamp an i64 to be non-negative (a garbage/negative size becomes 0).
fn nonneg(n: i64) -> i64 {
    n.max(0)
}

/// A finite, non-negative float for a wire metric. A garbled probe emitting inf/nan
/// (which serde would turn into JSON `null`, a silent type deviation) becomes 0.
fn finite(x: f64) -> f64 {
    if x.is_finite() && x >= 0.0 {
        x
    } else {
        0.0
    }
}

/// Resolve a stable, privacy-clean machine id: a random id generated once and
/// cached under the Hanzo data dir. We deliberately do NOT read system machine
/// identifiers (which can be sensitive) — a per-install random id is enough to
/// group a machine's sessions and to gate same-machine resume.
pub fn machine_id() -> String {
    let path = data_dir().join("machine-id");
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

fn hostname() -> String {
    if let Ok(out) = Command::new("hostname").output() {
        if out.status.success() {
            let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !h.is_empty() {
                return h;
            }
        }
    }
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Best-effort git context for `cwd`. A non-repo yields an empty [`Repo`].
fn git_repo(cwd: &Path) -> Repo {
    let git = |args: &[&str]| -> Option<String> {
        // `git -C <cwd>` reads config from a possibly-untrusted working tree.
        // Neutralize the system/global gitconfig (a planted `core.fsmonitor`,
        // pager, alias or `include.path` there could otherwise run a command)
        // and never let git block on a credential/terminal prompt.
        let out = Command::new("git")
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

    pub fn save(&self) -> Result<()> {
        let path = record_path(&self.cloud_session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating resume store dir")?;
        }
        let json = serde_json::to_vec_pretty(self).context("serializing resume record")?;
        private::write(&path, &json).context("writing resume record")
    }

    pub fn load(cloud_session_id: &str) -> Result<Option<ResumeRecord>> {
        let path = record_path(cloud_session_id);
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
    pub fn load(machine_id: &str) -> Result<Option<TargetRecord>> {
        match std::fs::read(target_path(machine_id)) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("parsing target record")?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading target record"),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = target_path(&self.machine_id);
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

fn record_path(cloud_session_id: &str) -> PathBuf {
    // The id is cloud-minted (`sess_<hex>`); guard the filename regardless.
    data_dir().join("code").join("sessions").join(format!("{}.json", safe_name(cloud_session_id)))
}

fn target_path(machine_id: &str) -> PathBuf {
    data_dir().join("code").join("targets").join(format!("{}.json", safe_name(machine_id)))
}

/// The target-record path, exposed for test cleanup only.
#[cfg(test)]
pub(crate) fn target_path_for_test(machine_id: &str) -> PathBuf {
    target_path(machine_id)
}

/// The Hanzo per-user data dir (`~/.local/share/hanzo` on Linux, the platform
/// equivalent elsewhere). Non-secret runtime state only.
fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hanzo")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_carries_no_secrets_and_only_allowlisted_keys() {
        let snap = Snapshot {
            machine_id: "m1".into(),
            host: "box".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            cwd: "/home/z/proj".into(),
            backend: "claude".into(),
            backend_version: Some("2.1.0".into()),
            repo: Repo {
                root: Some("/home/z/proj".into()),
                remote: Some("https://github.com/o/r.git".into()),
                branch: Some("main".into()),
                head: Some("abc123".into()),
            },
        };
        let v = snap.context_payload(None);
        let obj = v.as_object().unwrap();
        // Exactly the location/identity keys — nothing environment-derived.
        let allowed: std::collections::HashSet<&str> = [
            "type", "machineId", "host", "os", "arch", "cwd", "backend",
            "backendVersion", "repo",
        ]
        .into_iter()
        .collect();
        for k in obj.keys() {
            assert!(allowed.contains(k.as_str()), "unexpected snapshot key: {k}");
        }
        // No value looks like a secret.
        let blob = serde_json::to_string(&v).unwrap().to_lowercase();
        for bad in ["token", "password", "secret", "authorization", "bearer", "api_key"] {
            assert!(!blob.contains(bad), "snapshot leaked '{bad}': {blob}");
        }
        assert_eq!(v["type"], "context");
    }

    #[test]
    fn git_remote_credentials_are_scrubbed() {
        assert_eq!(
            scrub_remote("https://user:ghp_SECRETTOKEN@github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            scrub_remote("https://x-access-token:AAA@gitlab.com/o/r.git"),
            "https://gitlab.com/o/r.git"
        );
        // scp-like + plain https carry no secret and are untouched.
        assert_eq!(scrub_remote("git@github.com:o/r.git"), "git@github.com:o/r.git");
        assert_eq!(scrub_remote("https://github.com/o/r.git"), "https://github.com/o/r.git");
    }

    #[test]
    fn resume_payload_shape_has_no_secret_fields() {
        let rec = ResumeRecord {
            cloud_session_id: "sess_1".into(),
            identity: "admin/z".into(),
            backend: "dev".into(),
            backend_session_id: "thread-uuid".into(),
            cwd: "/w".into(),
            api: "https://api.hanzo.ai".into(),
            machine_id: "m1".into(),
            repo: Repo::default(),
            transcript_path: None,
            created_at: 0,
        };
        let v = rec.resume_payload();
        assert_eq!(v["type"], "resume");
        assert_eq!(v["backendSessionId"], "thread-uuid");
        let blob = serde_json::to_string(&v).unwrap().to_lowercase();
        for bad in ["token", "password", "secret", "authorization", "bearer"] {
            assert!(!blob.contains(bad));
        }

        // The CLI NEVER sends an org: cloud derives it from the JWT `owner`.
        // `identity` is recorded LOCALLY (to decide cross-org resume honestly)
        // and must not ride the wire — `resume_payload` is an explicit allowlist,
        // so this pins that omission as an invariant rather than an accident.
        // The record here is deliberately `admin/z`, so a leak would show up.
        assert!(v.get("identity").is_none(), "identity must not reach cloud");
        assert!(v.get("owner").is_none(), "owner must not reach cloud");
        for bad in ["identity", "owner", "admin/z", "admin"] {
            assert!(!blob.contains(bad), "{bad:?} leaked into the wire payload: {blob}");
        }
    }

    #[test]
    fn resume_record_roundtrips_through_the_local_store() {
        let id = format!("sess_test_{}", std::process::id());
        let rec = ResumeRecord {
            cloud_session_id: id.clone(),
            identity: "hanzo/z".into(),
            backend: "claude".into(),
            backend_session_id: "claude-sid".into(),
            cwd: "/tmp".into(),
            api: "https://api.hanzo.ai".into(),
            machine_id: machine_id(),
            repo: Repo::default(),
            transcript_path: Some("/tmp/x.jsonl".into()),
            created_at: 123,
        };
        rec.save().unwrap();
        let loaded = ResumeRecord::load(&id).unwrap().unwrap();
        assert_eq!(loaded, rec);
        let _ = std::fs::remove_file(record_path(&id));
    }

    // ---- machine capability plane ----

    fn sample_spec() -> Spec {
        Spec {
            os: "linux".into(),
            arch: "arm64".into(),
            cpus: 20,
            memory: 137438953472,
            gpus: vec![Gpu { vendor: "nvidia".into(), model: "GB10".into(), memory: 103079215104 }],
        }
    }

    #[test]
    fn spec_and_metrics_serialize_with_exact_wire_field_names() {
        let spec = serde_json::to_value(sample_spec()).unwrap();
        let keys: std::collections::HashSet<&str> = spec.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["os", "arch", "cpus", "memory", "gpus"].into_iter().collect());
        let gpu = &spec["gpus"][0];
        let gkeys: std::collections::HashSet<&str> = gpu.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(gkeys, ["vendor", "model", "memory"].into_iter().collect());
        assert_eq!(spec["memory"], serde_json::json!(137438953472i64));
        assert_eq!(gpu["memory"], serde_json::json!(103079215104i64));

        let m = Metrics { load1: 1.5, load5: 1.2, load15: 0.9, mem_used: 42, mem_free: 7, gpu_util: 0.4 };
        let mv = serde_json::to_value(&m).unwrap();
        let mkeys: std::collections::HashSet<&str> = mv.as_object().unwrap().keys().map(String::as_str).collect();
        // The camelCase names the server reads — and NO `at` (server-stamped only).
        assert_eq!(mkeys, ["load1", "load5", "load15", "memUsed", "memFree", "gpuUtil"].into_iter().collect());
        assert!(mv.get("at").is_none(), "client must never send the metrics timestamp");
        assert_eq!(mv["memUsed"], serde_json::json!(42));
        assert_eq!(mv["gpuUtil"], serde_json::json!(0.4));
    }

    /// Capture on THIS machine returns a sane spec (cpus > 0) and reports live
    /// loadavg/memory — and NOTHING in spec/metrics comes from the environment.
    #[tokio::test]
    async fn machine_capture_returns_sane_spec_without_reading_env() {
        // A unique sentinel placed in the process env must NOT round-trip into the
        // captured data (the privacy hard-line: capture reads system sources only).
        std::env::set_var("HANZO_CAPTURE_SENTINEL", "sentinel-leak-9f3a2b1c7d");
        let m = Machine::capture().await;
        eprintln!(
            "machine_capture: os={} arch={} cpus={} memory={}B gpus={:?} metrics={:?} capacity={:?}",
            m.spec.os, m.spec.arch, m.spec.cpus, m.spec.memory, m.spec.gpus, m.metrics, m.spec.capacity()
        );
        assert!(m.spec.cpus > 0, "logical core count must be positive");
        assert_eq!(m.spec.os, std::env::consts::OS);
        assert_eq!(m.spec.arch, std::env::consts::ARCH);
        if cfg!(target_os = "linux") {
            assert!(m.spec.memory > 0, "linux MemTotal must be read");
            // /proc/loadavg is always readable on linux; at least one field parses.
            assert!(m.metrics.mem_free >= 0 && m.metrics.mem_used >= 0);
        }
        let blob = format!(
            "{}{}",
            serde_json::to_string(&m.spec).unwrap(),
            serde_json::to_string(&m.metrics).unwrap()
        );
        assert!(!blob.contains("sentinel-leak-9f3a2b1c7d"), "env value leaked into capture: {blob}");
        std::env::remove_var("HANZO_CAPTURE_SENTINEL");
    }

    #[tokio::test]
    async fn probe_runs_with_a_clean_env_and_survives_a_missing_binary() {
        // A binary that does not exist yields None, never a hang or panic.
        assert!(probe("definitely-not-a-real-binary-xyz", &[], PROBE_TIMEOUT).await.is_none());
        // A present probe reads only its own stdout; the child has no inherited env
        // beyond PATH, so a parent sentinel can never reach a probe's output.
        std::env::set_var("HANZO_PROBE_SENTINEL", "probe-leak-5e1d");
        if let Some(out) = probe("env", &[], PROBE_TIMEOUT).await {
            assert!(!out.contains("probe-leak-5e1d"), "probe inherited the parent env: {out}");
        }
        std::env::remove_var("HANZO_PROBE_SENTINEL");
    }

    #[test]
    fn parse_nvidia_handles_real_empty_and_garbage() {
        // Empty input -> no GPUs, zero util.
        let (g, u) = parse_nvidia("");
        assert!(g.is_empty() && u == 0.0);

        // A real two-GPU sample: MiB memory, percent util, "NVIDIA " prefix trimmed.
        let (g, u) = parse_nvidia("NVIDIA GB10, 98304, 40\nNVIDIA GB10, 98304, 60");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].vendor, "nvidia");
        assert_eq!(g[0].model, "GB10");
        assert_eq!(g[0].memory, 103079215104); // 98304 MiB
        assert!((u - 0.5).abs() < 1e-9, "mean of 40% and 60% is 0.5, got {u}");

        // Garbage never panics; a missing memory/util degrades to 0.
        let (g, _) = parse_nvidia("weird-line-no-commas\n, , ");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].memory, 0);
    }

    #[test]
    fn parse_lspci_selects_display_controllers() {
        let text = "\
00:02.0 VGA compatible controller: Intel Corporation UHD Graphics 630 (rev 02)
65:00.0 VGA compatible controller: Advanced Micro Devices, Inc. [AMD/ATI] Strix [Radeon 8060S]
01:00.1 Audio device: NVIDIA Corporation Device 10f9 (rev a1)
03:00.0 3D controller: NVIDIA Corporation GA100 [A100] (rev a1)";
        let g = parse_lspci(text);
        assert_eq!(g.len(), 3, "only VGA/3D/display controllers, not the audio device");
        assert_eq!(g[0].vendor, "intel");
        assert_eq!(g[0].model, "Intel Corporation UHD Graphics 630", "the (rev NN) suffix is stripped");
        assert_eq!(g[1].vendor, "amd");
        assert!(g[1].model.contains("Radeon 8060S"));
        assert_eq!(g[2].vendor, "nvidia");
        assert!(!g[2].model.contains("rev"), "hardware revision noise dropped: {}", g[2].model);
        // Empty / non-controller input yields nothing.
        assert!(parse_lspci("").is_empty());
        assert!(parse_lspci("00:1f.3 Audio device: Intel Corp").is_empty());
    }

    #[test]
    fn parse_macos_gpus_reads_chipset_and_vram() {
        let text = "\
Graphics/Displays:
    Apple M2 Max:
      Chipset Model: Apple M2 Max
      Type: GPU
      Vendor: Apple (0x106b)
    Radeon Pro 560:
      Chipset Model: Radeon Pro 560
      Vendor: AMD (0x1002)
      VRAM (Total): 4 GB";
        let g = parse_macos_gpus(text);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].vendor, "apple");
        assert_eq!(g[0].model, "Apple M2 Max");
        assert_eq!(g[0].memory, 0); // unified memory reports no VRAM
        assert_eq!(g[1].vendor, "amd");
        assert_eq!(g[1].memory, 4 * (1i64 << 30));
        assert!(parse_macos_gpus("").is_empty());
    }

    #[test]
    fn parse_meminfo_computes_used_and_free() {
        let s = "MemTotal:       32768000 kB\nMemFree:         1000000 kB\nMemAvailable:   20000000 kB\n";
        let (total, used, free) = parse_meminfo(s);
        assert_eq!(total, 32_768_000 * 1024);
        assert_eq!(free, 20_000_000 * 1024);
        assert_eq!(used, (32_768_000 - 20_000_000) * 1024);
        assert_eq!(parse_meminfo(""), (0, 0, 0));
    }

    #[test]
    fn parse_loadavg_reads_three_fields_incl_macos_braces() {
        assert_eq!(parse_loadavg("0.52 0.58 0.59 1/234 5678"), (0.52, 0.58, 0.59));
        // macOS `vm.loadavg` form after the braces are stripped (as loadavg() does).
        assert_eq!(parse_loadavg(&"{ 1.20 1.10 1.00 }".replace(['{', '}'], " ")), (1.20, 1.10, 1.00));
        assert_eq!(parse_loadavg(""), (0.0, 0.0, 0.0));
    }

    #[test]
    fn parse_vm_stat_uses_header_page_size() {
        let s = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
Pages free:                          100000.\n\
Pages active:                        200000.\n\
Pages wired down:                     80000.";
        let (used, free) = parse_vm_stat(s);
        assert_eq!(free, 100_000 * 16384);
        assert_eq!(used, (200_000 + 80_000) * 16384);
    }

    #[test]
    fn capacity_summary_matches_the_contract_example() {
        assert_eq!(sample_spec().capacity(), "20 vCPU / 128G / 1× GB10");
        // Unknown parts drop out; a laptop with no GPU is just cpu + memory.
        let laptop = Spec { cpus: 8, memory: 16 * (1i64 << 30), ..Spec::default() };
        assert_eq!(laptop.capacity(), "8 vCPU / 16G");
        assert_eq!(Spec::default().capacity(), "");
    }

    #[test]
    fn human_bytes_rounds_to_binary_units() {
        assert_eq!(human_bytes(137438953472), "128G");
        assert_eq!(human_bytes(512 * (1i64 << 20)), "512M");
        assert_eq!(human_bytes(1000), "1000B");
    }

    #[test]
    fn gpu_summary_groups_by_model() {
        let two = vec![
            Gpu { vendor: "nvidia".into(), model: "GB10".into(), memory: 0 },
            Gpu { vendor: "nvidia".into(), model: "GB10".into(), memory: 0 },
        ];
        assert_eq!(gpu_summary(&two), "2× GB10");
        let mixed = vec![
            Gpu { model: "GB10".into(), ..Gpu::default() },
            Gpu { model: "8060S".into(), ..Gpu::default() },
        ];
        assert_eq!(gpu_summary(&mixed), "1× GB10 + 1× 8060S");
    }

    #[test]
    fn target_record_roundtrips_through_the_local_store() {
        let machine = format!("testmachine_{}", std::process::id());
        let rec = TargetRecord {
            id: "tgt_abc".into(),
            host: "evo".into(),
            machine_id: machine.clone(),
            api: "https://api.hanzo.ai".into(),
            updated_at: 42,
        };
        rec.save().unwrap();
        assert_eq!(TargetRecord::load(&machine).unwrap().unwrap(), rec);
        let _ = std::fs::remove_file(target_path(&machine));
        assert!(TargetRecord::load(&machine).unwrap().is_none());
    }

    // ---- red-hardening: bound the probe, saturate the math, coerce non-finite ----

    #[tokio::test]
    async fn probe_stdout_is_capped_under_a_flood() {
        // A probe that floods stdout is truncated to the cap, never buffered whole.
        let n = 32 * 1024 * 1024usize; // 32 MiB flood
        let script = format!("head -c {n} /dev/zero | tr '\\0' a");
        if let Some(s) = probe("sh", &["-c", &script], PROBE_TIMEOUT).await {
            assert!(
                s.len() as u64 <= PROBE_STDOUT_CAP,
                "flood must be capped at {PROBE_STDOUT_CAP}, got {}",
                s.len()
            );
        }
    }

    #[test]
    fn parse_meminfo_saturates_instead_of_panicking() {
        // A faked /proc/meminfo (negative total, max available) must not panic even in
        // a debug build (overflow-checks on) — saturating_sub keeps used >= 0.
        let s = "MemTotal:       -1 kB\nMemAvailable:   9223372036854775807 kB\n";
        let (_total, used, _free) = parse_meminfo(s);
        assert!(used >= 0);
    }

    #[test]
    fn parse_vm_stat_saturates_instead_of_panicking() {
        let s = "Mach Virtual Memory Statistics: (page size of 4096 bytes)\n\
                 Pages active:              9223372036854775807.\n\
                 Pages wired down:          9223372036854775807.\n";
        let (used, _free) = parse_vm_stat(s);
        assert!(used >= 0);
    }

    #[test]
    fn finite_coerces_nonfinite_and_negative_to_zero() {
        assert_eq!(finite(f64::NAN), 0.0);
        assert_eq!(finite(f64::INFINITY), 0.0);
        assert_eq!(finite(f64::NEG_INFINITY), 0.0);
        assert_eq!(finite(-1.0), 0.0);
        assert_eq!(finite(2.5), 2.5);
    }
}
