//! System capability and live-metrics probes for [`super::Machine`].

use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

use super::{Gpu, Metrics};

/// The most accelerators we record — an absurd count is a bug or an attack, not a
/// real host (mirrors the server's `maxGPUs`).
const MAX_GPUS: usize = 32;

/// The per-probe hard deadline. A system probe that hangs past this is abandoned
/// (its child killed on drop) so capability/metrics gathering can NEVER stall the
/// session that spawned it.
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The most probe stdout we read. Any of these tools (nvidia-smi/lspci/vm_stat/
/// sysctl/system_profiler) emits well under this; a flood past it is truncated so a
/// hostile PATH binary can never balloon memory.
pub(super) const PROBE_STDOUT_CAP: u64 = 256 * 1024;

/// Logical core count (respects cgroup/affinity limits — the real capacity).
pub(super) fn cpus() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(0)
}

/// (total, used, free) RAM in bytes, best-effort. Linux reads /proc/meminfo once;
/// macOS uses `sysctl hw.memsize` for the total and `vm_stat` for the live split.
pub(super) async fn memory() -> (i64, i64, i64) {
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
pub(super) async fn loadavg() -> (f64, f64, f64) {
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

/// The live readings nvidia-smi gives beside the inventory, aggregated across GPUs.
/// `None` is a reading the driver did not give (a GB10 has no `memory.total`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Board {
    pub temp: Option<f64>,
    pub power: Option<f64>,
    pub limit: Option<f64>,
    pub vram_used: Option<i64>,
    pub vram_total: Option<i64>,
}

impl Board {
    pub(super) fn apply(&self, m: &mut Metrics) {
        m.gpu_temp = self.temp;
        m.gpu_power = self.power;
        m.gpu_power_limit = self.limit;
        m.gpu_mem_used = self.vram_used;
        m.gpu_mem_total = self.vram_total;
    }
}

/// The nvidia-smi query: inventory, utilization and the board readings in one run.
const NVIDIA_QUERY: &str =
    "--query-gpu=name,memory.total,utilization.gpu,temperature.gpu,power.draw,power.limit,memory.used";

/// Best-effort accelerators + aggregate utilization (0..1) + board readings. Tries
/// nvidia-smi first (one query); on Linux falls back to lspci (name only) for
/// AMD/Intel/other, on macOS to system_profiler. Bounded to `MAX_GPUS`.
pub(super) async fn gpus() -> (Vec<Gpu>, f64, Board) {
    if let Some(out) =
        probe("nvidia-smi", &[NVIDIA_QUERY, "--format=csv,noheader,nounits"], PROBE_TIMEOUT).await
    {
        let (gpus, util, board) = parse_nvidia(&out);
        if !gpus.is_empty() {
            return (gpus, util, board);
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
    (gpus, 0.0, Board::default())
}

/// Run a read-only system probe with a hard deadline, returning trimmed stdout on a
/// clean exit. A missing binary, a non-zero exit, or a timeout all yield `None`.
///
/// The child gets a MINIMAL environment (PATH only): no other environment value can
/// influence a probe or round-trip into captured data — the SAME privacy hard-line
/// the git probe holds. `kill_on_drop` guarantees a timed-out probe is reaped.
pub(crate) async fn probe(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut cmd = tokio::process::Command::from(crate::exe::bare(crate::exe::find(program)?));
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
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
pub(super) fn parse_meminfo(s: &str) -> (i64, i64, i64) {
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
pub(super) fn parse_loadavg(s: &str) -> (f64, f64, f64) {
    let mut it = s.split_whitespace().filter_map(|f| f.parse::<f64>().ok());
    (it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0))
}

/// Parse macOS `vm_stat` into (used, free) bytes, best-effort. Page size comes from
/// the header ("page size of N bytes"); used ≈ (active + wired) × page, free ≈
/// free × page.
pub(super) fn parse_vm_stat(s: &str) -> (i64, i64) {
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

/// Parse `nvidia-smi --query-gpu=name,memory.total,utilization.gpu,temperature.gpu,
/// power.draw,power.limit,memory.used --format=csv,noheader,nounits`: one row per
/// GPU ("NVIDIA GB10, [N/A], 40, 54, 16.52, [N/A], [N/A]"), memory in MiB,
/// utilization in percent. Returns the GPUs, the mean util (0..1) and the board
/// readings: the hottest temperature, summed power, and VRAM summed only when every
/// GPU reports it — a unified GPU reports none, and a partial sum would lie.
pub(super) fn parse_nvidia(csv: &str) -> (Vec<Gpu>, f64, Board) {
    let mut gpus = Vec::new();
    let mut util_sum = 0.0;
    let mut util_n = 0u32;
    let mut board = Board::default();
    let (mut used, mut total, mut rows) = (Some(0i64), Some(0i64), 0);
    for line in csv.lines() {
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        let model = cols.first().copied().unwrap_or("");
        if model.is_empty() {
            continue;
        }
        let num = |i: usize| cols.get(i).and_then(|s| s.parse::<f64>().ok()).filter(|f| f.is_finite() && *f >= 0.0);
        let mib = |i: usize| num(i).map(|m| nonneg((m * 1048576.0) as i64));
        if let Some(u) = num(2) {
            util_sum += u;
            util_n += 1;
        }
        if let Some(t) = num(3) {
            board.temp = Some(board.temp.map_or(t, |b: f64| b.max(t)));
        }
        if let Some(p) = num(4) {
            board.power = Some(board.power.unwrap_or(0.0) + p);
        }
        if let Some(l) = num(5) {
            board.limit = Some(board.limit.unwrap_or(0.0) + l);
        }
        let vram = mib(1);
        total = total.zip(vram).map(|(a, b)| a + b);
        used = used.zip(mib(6)).map(|(a, b)| a + b);
        rows += 1;
        gpus.push(Gpu {
            vendor: "nvidia".to_string(),
            model: clamp_model(model.strip_prefix("NVIDIA ").unwrap_or(model)),
            memory: vram.unwrap_or(0),
        });
        if gpus.len() >= MAX_GPUS {
            break;
        }
    }
    if rows > 0 {
        board.vram_total = total.filter(|t| *t > 0);
        board.vram_used = used.filter(|_| board.vram_total.is_some());
    }
    let util = if util_n > 0 { (util_sum / util_n as f64) / 100.0 } else { 0.0 };
    (gpus, util, board)
}

/// Parse `lspci` output for display controllers (name only — no VRAM/util). The
/// class/description split is the FIRST ": " (a PCI slot's colons carry no space),
/// so it is robust to a domain prefix. Vendor is inferred from the description.
pub(super) fn parse_lspci(text: &str) -> Vec<Gpu> {
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
pub(super) fn parse_macos_gpus(text: &str) -> Vec<Gpu> {
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
pub(super) fn human_bytes(n: i64) -> String {
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
pub(super) fn gpu_summary(gpus: &[Gpu]) -> String {
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
pub(super) fn finite(x: f64) -> f64 {
    if x.is_finite() && x >= 0.0 {
        x
    } else {
        0.0
    }
}

