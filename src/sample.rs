//! What a machine is doing, read twice: the rates.
//!
//! [`Machine::capture`] reads what one look can tell — inventory, load, memory,
//! the GPU board. Utilization, disk and network traffic, and model serving are
//! counters, and a counter says nothing until it is read again. A [`Sampler`]
//! keeps the previous reading so each [`Sampler::machine`] answers for the window
//! since the last one: the heartbeat's thirty seconds, the console's two.
//!
//! The metrics are the ones sparkDash charts for a DGX Spark, from the same
//! sources: `/proc/stat` for CPU, the first CPU hwmon (else thermal zone) for its
//! temperature, `statvfs` over local block filesystems, `/proc/diskstats` and
//! `/proc/net/dev` for physical devices only, and a model server's Prometheus
//! `/metrics` (vLLM, SGLang) for tokens/s and time to first token. A reading this
//! machine cannot give stays `None`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use super::context::{probe, Machine, Metrics};

/// Well-known model-server ports, probed beside the ones found under GPU processes:
/// vLLM's default and SGLang's.
const SERVE_PORTS: [u16; 2] = [8000, 30000];

/// How long a model server gets to answer `/metrics`.
const SERVE_TIMEOUT: Duration = Duration::from_millis(800);

/// How far up the process tree a GPU process's listener is looked for. A vLLM or
/// SGLang API server is the parent of the engine process that holds the GPU.
const ANCESTORS: usize = 4;

/// Counters from one reading, kept for the next.
#[derive(Debug, Clone, Default)]
struct Counters {
    cpu: Option<(u64, u64)>,
    disk: Option<(u64, u64)>,
    net: Option<(u64, u64)>,
    serve: HashMap<u16, Serve>,
}

/// A model server's counters at one instant.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Serve {
    pub model: Option<String>,
    pub generated: Option<f64>,
    pub prompt: Option<f64>,
    pub steps: Option<f64>,
    pub ttft_sum: Option<f64>,
    pub ttft_count: Option<f64>,
    pub running: Option<f64>,
    pub waiting: Option<f64>,
    pub kv: Option<f64>,
}

/// A stateful reader of this machine. Hold one and call [`Sampler::machine`] on a
/// period; the first call has no rates, every later one does.
pub struct Sampler {
    last: Option<(Instant, Counters)>,
    http: reqwest::Client,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Sampler {
        let http = reqwest::Client::builder().timeout(SERVE_TIMEOUT).build().unwrap_or_default();
        Sampler { last: None, http }
    }

    /// Capture the machine and fill in everything that needs a window.
    pub async fn machine(&mut self) -> Machine {
        let mut machine = Machine::capture().await;
        let now = Instant::now();
        let mut next = Counters::default();
        let m = &mut machine.metrics;

        next.cpu = read("/proc/stat").and_then(|s| parse_cpu(&s));
        m.cpu_temp = cpu_temp();

        let apps = gpu_apps().await;
        if m.gpu_mem_used.is_none() && !apps.is_empty() {
            m.gpu_mem_used = Some(apps.iter().map(|a| a.1).sum());
        }
        if machine.spec.gpus.iter().all(|g| g.vendor != "nvidia") {
            amdgpu(m);
        }

        let (used, total) = disks();
        if total > 0 {
            m.disk_used = Some(used);
            m.disk_total = Some(total);
        }
        next.disk = read("/proc/diskstats").map(|s| parse_diskstats(&s, physical_block));
        next.net = read("/proc/net/dev").map(|s| parse_netdev(&s, physical_net));

        let mut ports: Vec<u16> = listeners(&apps.iter().map(|a| a.0).collect::<Vec<_>>());
        ports.extend(SERVE_PORTS);
        ports.sort_unstable();
        ports.dedup();
        for port in ports {
            if let Some(serve) = self.serve(port).await {
                next.serve.insert(port, serve);
            }
        }

        let prev = self.last.as_ref().map(|(at, c)| (now.duration_since(*at).as_secs_f64(), c));
        rates(m, &next, prev);
        self.last = Some((now, next));
        machine
    }

    /// Read one model server's `/metrics`. Only a vLLM or SGLang exposition counts:
    /// anything else on the port is not a model server we can measure.
    async fn serve(&self, port: u16) -> Option<Serve> {
        let url = format!("http://127.0.0.1:{port}/metrics");
        let resp = self.http.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        parse_serve(&resp.text().await.ok()?)
    }
}

/// Fill the windowed readings from two sets of counters `dt` seconds apart. With
/// no previous reading only the gauges (running, waiting, KV cache, model) land.
fn rates(m: &mut Metrics, next: &Counters, prev: Option<(f64, &Counters)>) {
    let mut models: Vec<String> = Vec::new();
    let (mut running, mut waiting, mut kv) = (None, None, None);
    for s in next.serve.values() {
        if let Some(name) = &s.model {
            if !models.contains(name) {
                models.push(name.clone());
            }
        }
        running = add(running, s.running);
        waiting = add(waiting, s.waiting);
        if let Some(k) = s.kv {
            kv = Some(kv.map_or(k, |c: f64| c.max(k)).clamp(0.0, 1.0));
        }
    }
    if !next.serve.is_empty() {
        let joined = models.join(", ");
        m.model = (!joined.is_empty()).then(|| joined.chars().take(128).collect());
        m.running = running.map(|r| r as i64);
        m.waiting = waiting.map(|w| w as i64);
        m.kv_cache = kv;
    }

    let Some((dt, prev)) = prev.filter(|(dt, _)| *dt > 0.0) else { return };
    if let (Some(a), Some(b)) = (prev.cpu, next.cpu) {
        let (total, idle) = (b.0.saturating_sub(a.0), b.1.saturating_sub(a.1));
        if total > 0 {
            m.cpu_util = Some((total.saturating_sub(idle)) as f64 / total as f64);
        }
    }
    let per_sec = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(a), Some(b)) if b >= a => Some((b - a) as f64 / dt),
        _ => None,
    };
    m.disk_read = per_sec(prev.disk.map(|d| d.0), next.disk.map(|d| d.0));
    m.disk_write = per_sec(prev.disk.map(|d| d.1), next.disk.map(|d| d.1));
    m.net_rx = per_sec(prev.net.map(|n| n.0), next.net.map(|n| n.0));
    m.net_tx = per_sec(prev.net.map(|n| n.1), next.net.map(|n| n.1));

    let (mut decode, mut prefill, mut ttft) = (None, None, Vec::new());
    for (port, s) in &next.serve {
        let Some(p) = prev.serve.get(port) else { continue };
        let (d, f, t) = serve_rates(p, s, dt);
        decode = add(decode, d);
        prefill = add(prefill, f);
        ttft.extend(t);
    }
    m.decode = decode;
    m.prefill = prefill;
    m.ttft = (!ttft.is_empty()).then(|| ttft.iter().sum::<f64>() / ttft.len() as f64);
}

/// One server's decode tok/s, prefill tok/s and mean TTFT over `dt` seconds, the
/// way sparkDash derives them from the same counters. Prefill prefers the engine's
/// step tokens beyond generation (live, even mid-request); with none, prompt
/// tokens per second of time-to-first-token; with no TTFT either, per second of
/// wall time. A counter that went backwards is a restarted server: no rate.
fn serve_rates(p: &Serve, s: &Serve, dt: f64) -> (Option<f64>, Option<f64>, Option<f64>) {
    let delta = |a: Option<f64>, b: Option<f64>| a.zip(b).map(|(a, b)| b - a).filter(|d| *d >= 0.0);
    let gen = delta(p.generated, s.generated);
    let prompt = delta(p.prompt, s.prompt);
    let ttft_sum = delta(p.ttft_sum, s.ttft_sum);
    let ttft_n = delta(p.ttft_count, s.ttft_count);

    let decode = gen.map(|g| g / dt);
    let live = delta(p.steps, s.steps).map(|steps| (steps - gen.unwrap_or(0.0)).max(0.0));
    // Speculative decoding adds draft tokens to the step count; a surplus under
    // half the generated tokens is that, not a prefill.
    let live = live.filter(|l| *l > 0.0 && !(gen.unwrap_or(0.0) > 0.0 && *l < gen.unwrap_or(0.0) * 0.5));
    let prefill = match (live, prompt) {
        (Some(l), _) => Some(l / dt),
        (None, Some(n)) if n > 0.0 => match ttft_sum.filter(|t| *t > 0.0) {
            Some(t) => Some(n / t),
            None => Some(n / dt),
        },
        (None, Some(_)) => Some(0.0),
        (None, None) => None,
    };
    let ttft = ttft_sum.zip(ttft_n.filter(|n| *n > 0.0)).map(|(s, n)| s / n);
    (decode, prefill, ttft)
}

fn add(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// `(total, idle)` jiffies from the aggregate `cpu` line of `/proc/stat`. Guest
/// time is already inside user time, so only the first eight fields count, and
/// iowait is idle.
fn parse_cpu(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let f: Vec<u64> = line.split_whitespace().skip(1).take(8).filter_map(|n| n.parse().ok()).collect();
    if f.len() < 5 {
        return None;
    }
    Some((f.iter().sum(), f[3] + f[4]))
}

/// The CPU temperature sparkDash reads: the first `temp*_input` of the first CPU
/// hwmon (coretemp, k10temp, zenpower, acpitz), else the first thermal zone.
fn cpu_temp() -> Option<f64> {
    let milli = |p: &Path| {
        std::fs::read_to_string(p).ok()?.trim().parse::<f64>().ok().filter(|t| *t > 0.0 && *t < 200_000.0)
    };
    for dir in sorted("/sys/class/hwmon") {
        let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
        if matches!(name.trim(), "coretemp" | "k10temp" | "zenpower" | "acpitz") {
            if let Some(t) = milli(&dir.join("temp1_input")) {
                return Some(t / 1000.0);
            }
        }
    }
    sorted("/sys/class/thermal")
        .into_iter()
        .filter(|d| d.file_name().is_some_and(|n| n.to_string_lossy().starts_with("thermal_zone")))
        .find_map(|d| milli(&d.join("temp")))
        .map(|t| t / 1000.0)
}

fn sorted(dir: &str) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .map(|it| it.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort_by(|a, b| natural(a).cmp(&natural(b)));
    v
}

/// `thermal_zone10` after `thermal_zone9`: order by the trailing number.
fn natural(p: &Path) -> (String, u64) {
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let digits: String = name.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
    (name[..name.len() - digits.len()].to_string(), digits.parse().unwrap_or(0))
}

/// `(pid, bytes)` for every process holding NVIDIA GPU memory.
async fn gpu_apps() -> Vec<(u32, i64)> {
    probe(
        "nvidia-smi",
        &["--query-compute-apps=pid,used_gpu_memory", "--format=csv,noheader,nounits"],
        Duration::from_secs(2),
    )
    .await
    .map(|s| parse_apps(&s))
    .unwrap_or_default()
}

fn parse_apps(csv: &str) -> Vec<(u32, i64)> {
    csv.lines()
        .filter_map(|l| {
            let (pid, mib) = l.split_once(',')?;
            let pid = pid.trim().parse().ok()?;
            let mib = mib.trim().parse::<f64>().ok().filter(|m| m.is_finite() && *m >= 0.0)?;
            Some((pid, (mib * 1048576.0) as i64))
        })
        .collect()
}

/// An AMD GPU's utilization, temperature and power from the amdgpu driver's sysfs
/// (a Strix Halo reports all three; its memory is the system's).
fn amdgpu(m: &mut Metrics) {
    for card in sorted("/sys/class/drm") {
        let dev = card.join("device");
        let Some(busy) = read_num(&dev.join("gpu_busy_percent")) else { continue };
        m.gpu_util = (busy / 100.0).clamp(0.0, 1.0);
        for hw in sorted(&dev.join("hwmon").to_string_lossy()) {
            m.gpu_temp = read_num(&hw.join("temp1_input")).map(|t| t / 1000.0).or(m.gpu_temp);
            m.gpu_power = read_num(&hw.join("power1_average"))
                .or_else(|| read_num(&hw.join("power1_input")))
                .map(|uw| uw / 1_000_000.0)
                .or(m.gpu_power);
        }
        return;
    }
}

fn read_num(p: &Path) -> Option<f64> {
    std::fs::read_to_string(p).ok()?.trim().parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0)
}

/// Used and total (used + available) bytes over the local block filesystems in
/// `/proc/mounts`, each device once.
fn disks() -> (i64, i64) {
    let Some(mounts) = read("/proc/mounts") else { return (0, 0) };
    let mut seen = HashSet::new();
    let (mut used, mut total) = (0i64, 0i64);
    for (dev, dir) in local_mounts(&mounts) {
        if !seen.insert(dev) {
            continue;
        }
        if let Some((u, a)) = statvfs(dir) {
            used += u;
            total += u + a;
        }
    }
    (used, total)
}

/// `(device, mountpoint)` for filesystems on a real block device: not a loop
/// image, not a boot or snap mount, not a read-only package format.
fn local_mounts(mounts: &str) -> Vec<(&str, &str)> {
    mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (dev, dir, fs) = (f.next()?, f.next()?, f.next()?);
            let real = dev.starts_with("/dev/") && !dev.starts_with("/dev/loop");
            let skip = dir.starts_with("/boot") || dir.starts_with("/snap") || matches!(fs, "squashfs" | "iso9660");
            (real && !skip).then_some((dev, dir))
        })
        .collect()
}

/// `(used, available)` bytes of the filesystem at `dir`.
#[cfg(unix)]
fn statvfs(dir: &str) -> Option<(i64, i64)> {
    let c = std::ffi::CString::new(dir.replace("\\040", " ")).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frag = st.f_frsize as i64;
    let used = (st.f_blocks as i64 - st.f_bfree as i64) * frag;
    Some((used.max(0), st.f_bavail as i64 * frag))
}

/// Windows has no statvfs, and no /proc/mounts to name a filesystem either.
#[cfg(not(unix))]
fn statvfs(_dir: &str) -> Option<(i64, i64)> {
    None
}

/// Whether a block device is hardware: the kernel links `device` only for those,
/// so loop, ram and device-mapper volumes are excluded without a name list.
fn physical_block(name: &str) -> bool {
    Path::new("/sys/block").join(name).join("device").exists()
}

/// Whether a network interface is hardware, by the same rule as [`physical_block`].
fn physical_net(name: &str) -> bool {
    Path::new("/sys/class/net").join(name).join("device").exists()
}

/// Summed `(read, written)` bytes over the devices `keep` accepts.
fn parse_diskstats(stats: &str, keep: impl Fn(&str) -> bool) -> (u64, u64) {
    stats.lines().fold((0, 0), |(r, w), l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 10 || !keep(f[2]) {
            return (r, w);
        }
        let n = |i: usize| f[i].parse::<u64>().unwrap_or(0) * 512;
        (r + n(5), w + n(9))
    })
}

/// Summed `(received, transmitted)` bytes over the interfaces `keep` accepts.
fn parse_netdev(dev: &str, keep: impl Fn(&str) -> bool) -> (u64, u64) {
    dev.lines().skip(2).fold((0, 0), |(rx, tx), l| {
        let Some((name, rest)) = l.split_once(':') else { return (rx, tx) };
        let f: Vec<u64> = rest.split_whitespace().filter_map(|n| n.parse().ok()).collect();
        if f.len() < 9 || !keep(name.trim()) {
            return (rx, tx);
        }
        (rx + f[0], tx + f[8])
    })
}

/// The TCP ports listened on by these processes or their parents: where the API
/// server of whatever holds the GPU is answering.
fn listeners(pids: &[u32]) -> Vec<u16> {
    let mut inodes = HashSet::new();
    let mut seen = HashSet::new();
    for &pid in pids {
        let mut cur = pid;
        for _ in 0..=ANCESTORS {
            if cur <= 1 || !seen.insert(cur) {
                break;
            }
            if let Ok(fds) = std::fs::read_dir(format!("/proc/{cur}/fd")) {
                for fd in fds.flatten() {
                    if let Some(i) = std::fs::read_link(fd.path()).ok().and_then(|l| socket_inode(&l.to_string_lossy())) {
                        inodes.insert(i);
                    }
                }
            }
            match read(&format!("/proc/{cur}/stat")).and_then(|s| parent(&s)) {
                Some(p) => cur = p,
                None => break,
            }
        }
    }
    if inodes.is_empty() {
        return Vec::new();
    }
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|p| read(p))
        .flat_map(|t| listening(&t))
        .filter(|(_, inode)| inodes.contains(inode))
        .map(|(port, _)| port)
        .collect()
}

fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

/// The parent pid from `/proc/<pid>/stat`, read after the command name's closing
/// parenthesis (a name may itself hold spaces and parentheses).
fn parent(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// `(port, inode)` of every LISTEN socket in a `/proc/net/tcp{,6}` table.
fn listening(table: &str) -> Vec<(u16, u64)> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 10 || f[3] != "0A" {
                return None;
            }
            let port = u16::from_str_radix(f[1].rsplit_once(':')?.1, 16).ok()?;
            Some((port, f[9].parse().ok()?))
        })
        .collect()
}

/// A vLLM or SGLang Prometheus exposition, summed across label sets, with the
/// served model from the `model_name` label. `None` for anything else.
pub(crate) fn parse_serve(text: &str) -> Option<Serve> {
    let mut sums: HashMap<&str, f64> = HashMap::new();
    let mut model = None;
    let mut known = false;
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.rsplit_once(' ') else { continue };
        let (name, labels) = key.split_once('{').unwrap_or((key, ""));
        let Some(bare) = name.strip_prefix("vllm:").or_else(|| name.strip_prefix("sglang:")) else { continue };
        known = true;
        if model.is_none() {
            model = label(labels, "model_name");
        }
        if let Ok(v) = value.trim().parse::<f64>() {
            if v.is_finite() {
                *sums.entry(bare).or_default() += v;
            }
        }
    }
    if !known {
        return None;
    }
    let get = |names: &[&str]| names.iter().find_map(|n| sums.get(n).copied());
    Some(Serve {
        model,
        generated: get(&["generation_tokens_total"]),
        prompt: get(&["prompt_tokens_total"]),
        steps: get(&["iteration_tokens_total_sum"]),
        ttft_sum: get(&["time_to_first_token_seconds_sum"]),
        ttft_count: get(&["time_to_first_token_seconds_count"]),
        running: get(&["num_requests_running", "num_running_reqs"]),
        waiting: get(&["num_requests_waiting", "num_queue_reqs"]),
        kv: get(&["kv_cache_usage_perc", "gpu_cache_usage_perc", "token_usage"]),
    })
}

fn label(labels: &str, key: &str) -> Option<String> {
    let rest = &labels[labels.find(&format!("{key}=\""))? + key.len() + 2..];
    let v = &rest[..rest.find('"')?];
    (!v.is_empty()).then(|| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VLLM: &str = r#"# HELP vllm:num_requests_running Number of requests in model execution batches.
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{engine="0",model_name="qwen3.8-flash-next"} 1.0
vllm:num_requests_waiting{engine="0",model_name="qwen3.8-flash-next"} 0.0
vllm:kv_cache_usage_perc{engine="0",model_name="qwen3.8-flash-next"} 0.12
vllm:prompt_tokens_total{engine="0",model_name="qwen3.8-flash-next"} 1000.0
vllm:generation_tokens_total{engine="0",model_name="qwen3.8-flash-next"} 500.0
vllm:iteration_tokens_total_sum{engine="0",model_name="qwen3.8-flash-next"} 1500.0
vllm:time_to_first_token_seconds_sum{engine="0",model_name="qwen3.8-flash-next"} 2.0
vllm:time_to_first_token_seconds_count{engine="0",model_name="qwen3.8-flash-next"} 4.0
process_resident_memory_bytes 1.9e9
"#;

    #[test]
    fn reads_a_vllm_exposition() {
        let s = parse_serve(VLLM).unwrap();
        assert_eq!(s.model.as_deref(), Some("qwen3.8-flash-next"));
        assert_eq!((s.generated, s.prompt, s.steps), (Some(500.0), Some(1000.0), Some(1500.0)));
        assert_eq!((s.running, s.waiting, s.kv), (Some(1.0), Some(0.0), Some(0.12)));
        assert_eq!((s.ttft_sum, s.ttft_count), (Some(2.0), Some(4.0)));
    }

    #[test]
    fn reads_sglang_names_and_refuses_anything_else() {
        let s = parse_serve("sglang:num_running_reqs{model_name=\"m\"} 3\nsglang:num_queue_reqs 2\nsglang:token_usage 0.5\n").unwrap();
        assert_eq!((s.running, s.waiting, s.kv, s.model.as_deref()), (Some(3.0), Some(2.0), Some(0.5), Some("m")));
        assert_eq!(parse_serve("process_cpu_seconds_total 1\nhttp_requests_total 9\n"), None);
    }

    #[test]
    fn serve_rates_match_sparkdash() {
        let p = parse_serve(VLLM).unwrap();
        let mut s = p.clone();
        s.generated = Some(600.0); // +100 over 2s: 50 tok/s
        s.steps = Some(1900.0); // +400 steps, 300 beyond generation: live prefill 150 tok/s
        s.prompt = Some(1300.0);
        s.ttft_sum = Some(2.5); // +0.5 s over +2 requests: 0.25 s
        s.ttft_count = Some(6.0);
        assert_eq!(serve_rates(&p, &s, 2.0), (Some(50.0), Some(150.0), Some(0.25)));

        // Speculative drafts alone: a small surplus is not prefill; prompt tokens
        // per second of TTFT stand in.
        s.steps = Some(1500.0 + 120.0);
        assert_eq!(serve_rates(&p, &s, 2.0).1, Some(300.0 / 0.5));

        // A restarted server's counters went backwards: no rate, not a negative one.
        let mut r = p.clone();
        r.generated = Some(10.0);
        r.steps = None;
        r.prompt = None;
        assert_eq!(serve_rates(&p, &r, 2.0), (None, None, None));
    }

    #[test]
    fn rates_fill_the_window_and_gauges_land_without_one() {
        let mut next = Counters { cpu: Some((1000, 600)), disk: Some((4096, 8192)), net: Some((100, 50)), ..Default::default() };
        next.serve.insert(18300, parse_serve(VLLM).unwrap());
        let mut m = Metrics::default();
        rates(&mut m, &next, None);
        assert_eq!(m.model.as_deref(), Some("qwen3.8-flash-next"));
        assert_eq!((m.running, m.waiting, m.kv_cache), (Some(1), Some(0), Some(0.12)));
        assert_eq!((m.cpu_util, m.decode, m.net_rx), (None, None, None), "no window, no rate");

        let prev = Counters { cpu: Some((800, 500)), disk: Some((0, 0)), net: Some((0, 0)), serve: next.serve.clone() };
        rates(&mut m, &next, Some((2.0, &prev)));
        assert_eq!(m.cpu_util, Some(0.5));
        assert_eq!((m.disk_read, m.disk_write), (Some(2048.0), Some(4096.0)));
        assert_eq!((m.net_rx, m.net_tx), (Some(50.0), Some(25.0)));
        assert_eq!((m.decode, m.prefill, m.ttft), (Some(0.0), Some(0.0), None), "idle server: zero, not unknown");
    }

    #[test]
    fn parses_proc_tables() {
        assert_eq!(parse_cpu("cpu  100 0 50 800 50 0 0 0 7 0\ncpu0 1 2 3 4\n"), Some((1000, 850)));
        assert_eq!(parse_cpu("intr 1 2\n"), None);

        let disk = "   8       0 sda 10 0 100 5 20 0 200 9 0 0 0\n   7       0 loop0 1 0 999 1 1 0 999 1 0 0 0\n";
        assert_eq!(parse_diskstats(disk, |n| n == "sda"), (100 * 512, 200 * 512));

        let net = "Inter-|   Receive\n face |bytes\n  eth0: 1000 5 0 0 0 0 0 0 2000 7 0 0 0 0 0 0\n    lo: 50 1 0 0 0 0 0 0 50 1 0 0 0 0 0 0\n";
        assert_eq!(parse_netdev(net, |n| n == "eth0"), (1000, 2000));

        let mounts = "/dev/nvme0n1p2 / ext4 rw 0 0\n/dev/nvme0n1p1 /boot/efi vfat rw 0 0\n/dev/loop3 /snap/core squashfs ro 0 0\ntmpfs /run tmpfs rw 0 0\n";
        assert_eq!(local_mounts(mounts), vec![("/dev/nvme0n1p2", "/")]);

        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000:4778 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 424242 1\n   1: 0100007F:1F90 0100007F:D2C4 01 00000000:00000000 00:00000000 00000000  1000        0 55 1\n";
        assert_eq!(listening(tcp), vec![(0x4778, 424242)]);
        assert_eq!(socket_inode("socket:[424242]"), Some(424242));
        assert_eq!(parent("1742827 (VLLM::Engine Core) S 1740799 1740799 0"), Some(1740799));
        assert_eq!(parse_apps("1742827, 73085\n"), vec![(1742827, 73085 * 1048576)]);
    }

    /// On this machine the sampler never fails, and a second read carries rates.
    #[tokio::test]
    async fn samples_this_machine_twice() {
        let mut s = Sampler::new();
        let first = s.machine().await;
        assert!(first.spec.cpus > 0);
        assert_eq!(first.metrics.cpu_util, None, "one reading has no window");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let second = s.machine().await;
        if cfg!(target_os = "linux") {
            assert!(second.metrics.cpu_util.is_some_and(|u| (0.0..=1.0).contains(&u)));
            assert!(second.metrics.net_rx.is_some());
        }
    }
}
