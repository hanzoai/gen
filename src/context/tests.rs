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
    let root = crate::root::scratch();
    let rec = ResumeRecord {
        cloud_session_id: id.clone(),
        identity: "hanzo/z".into(),
        backend: "claude".into(),
        backend_session_id: "claude-sid".into(),
        cwd: "/tmp".into(),
        api: "https://api.hanzo.ai".into(),
        machine_id: machine_id(&root),
        repo: Repo::default(),
        transcript_path: Some("/tmp/x.jsonl".into()),
        created_at: 123,
    };
    rec.save(&root).unwrap();
    let loaded = ResumeRecord::load(&root, &id).unwrap().unwrap();
    assert_eq!(loaded, rec);
    let _ = std::fs::remove_file(record_path(&root, &id));
}

// ---- naming the machine ----

/// A blank is not a name. This is the whole bug behind the console's "unknown
/// machine" bucket: a probe that succeeds and says nothing looks like an
/// answer, and every row that inherits it becomes ungroupable.
#[test]
fn a_blank_probe_is_not_a_name() {
    assert_eq!(named("evo"), Some("evo".into()));
    assert_eq!(named(" evo.local \n"), Some("evo.local".into()));
    for blank in ["", " ", "\n", "\t\r\n"] {
        assert_eq!(named(blank), None, "{blank:?} is not a machine name");
    }
}

/// Whatever the machine can or cannot tell us about itself, it ends up with a
/// name — the last resort being the stable per-install id, which groups even
/// though it does not describe. An empty host is the one answer that must be
/// unreachable, so the invariant is asserted on the real function.
#[test]
fn a_machine_always_names_itself() {
    let root = crate::root::scratch();
    let h = hostname(&root);
    assert!(!h.trim().is_empty(), "a session with no host cannot be grouped");
    assert_eq!(h, h.trim(), "a padded host is a different string every time");
    assert!(!Snapshot::capture(&root, Path::new("/"), "claude", None).host.is_empty());
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

    let m = Metrics { load1: 1.5, load5: 1.2, load15: 0.9, mem_used: 42, mem_free: 7, gpu_util: 0.4, ..Default::default() };
    let mv = serde_json::to_value(&m).unwrap();
    let mkeys: std::collections::HashSet<&str> = mv.as_object().unwrap().keys().map(String::as_str).collect();
    // The camelCase names the server reads — and NO `at` (server-stamped only).
    // An unknown reading is absent, never a zero.
    assert_eq!(mkeys, ["load1", "load5", "load15", "memUsed", "memFree", "gpuUtil"].into_iter().collect());
    assert!(mv.get("at").is_none(), "client must never send the metrics timestamp");
    assert_eq!(mv["memUsed"], serde_json::json!(42));
    assert_eq!(mv["gpuUtil"], serde_json::json!(0.4));

    let full = Metrics {
        cpu_util: Some(0.5), cpu_temp: Some(80.0), gpu_temp: Some(54.0), gpu_power: Some(16.5),
        gpu_power_limit: Some(120.0), gpu_mem_used: Some(1), gpu_mem_total: Some(2), disk_used: Some(3),
        disk_total: Some(4), disk_read: Some(5.0), disk_write: Some(6.0), net_rx: Some(7.0), net_tx: Some(8.0),
        model: Some("m".into()), decode: Some(40.0), prefill: Some(900.0), ttft: Some(0.2),
        running: Some(1), waiting: Some(0), kv_cache: Some(0.1), ..m.clone()
    };
    let fv = serde_json::to_value(&full).unwrap();
    let fkeys: std::collections::HashSet<&str> = fv.as_object().unwrap().keys().map(String::as_str).collect();
    let wire = [
        "load1", "load5", "load15", "memUsed", "memFree", "gpuUtil", "cpuUtil", "cpuTemp", "gpuTemp",
        "gpuPower", "gpuPowerLimit", "gpuMemUsed", "gpuMemTotal", "diskUsed", "diskTotal", "diskRead",
        "diskWrite", "netRx", "netTx", "model", "decode", "prefill", "ttft", "running", "waiting", "kvCache",
    ];
    assert_eq!(fkeys, wire.into_iter().collect());
    // What the cloud hands back reads into the same type, `at` and all.
    let back: Metrics = serde_json::from_value(serde_json::json!({"memUsed": 9, "gpuTemp": 61.5, "at": 1789444766})).unwrap();
    assert_eq!((back.mem_used, back.gpu_temp, back.decode), (9, Some(61.5), None));
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
    let (g, u, b) = parse_nvidia("");
    assert!(g.is_empty() && u == 0.0);
    assert_eq!(b, Board::default());

    // Two discrete GPUs: MiB memory, percent util, "NVIDIA " prefix trimmed,
    // the hottest temperature, summed power and summed VRAM.
    let (g, u, b) = parse_nvidia(
        "NVIDIA RTX 4090, 24564, 40, 61, 210.5, 450.00, 12000\nNVIDIA RTX 4090, 24564, 60, 70, 300, 450, 4000",
    );
    assert_eq!(g.len(), 2);
    assert_eq!(g[0].vendor, "nvidia");
    assert_eq!(g[0].model, "RTX 4090");
    assert_eq!(g[0].memory, 24564 * 1048576);
    assert!((u - 0.5).abs() < 1e-9, "mean of 40% and 60% is 0.5, got {u}");
    assert_eq!(b.temp, Some(70.0));
    assert_eq!(b.power, Some(510.5));
    assert_eq!(b.limit, Some(900.0));
    assert_eq!(b.vram_total, Some(2 * 24564 * 1048576));
    assert_eq!(b.vram_used, Some(16000 * 1048576));

    // A GB10 shares system memory: no memory.total, no memory.used, no limit.
    let (g, u, b) = parse_nvidia("NVIDIA GB10, [N/A], 15, 54, 16.52, [N/A], [N/A]");
    assert_eq!((g[0].model.as_str(), g[0].memory), ("GB10", 0));
    assert!((u - 0.15).abs() < 1e-9);
    assert_eq!((b.temp, b.power, b.limit), (Some(54.0), Some(16.52), None));
    assert_eq!((b.vram_total, b.vram_used), (None, None), "unified: no VRAM to report");

    // Garbage never panics; a missing memory/util degrades to 0 / unknown.
    let (g, _, b) = parse_nvidia("weird-line-no-commas\n, , ");
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].memory, 0);
    assert_eq!(b, Board::default());
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
    let root = crate::root::scratch();
    rec.save(&root).unwrap();
    assert_eq!(TargetRecord::load(&root, &machine).unwrap().unwrap(), rec);
    let _ = std::fs::remove_file(target_path(&root, &machine));
    assert!(TargetRecord::load(&root, &machine).unwrap().is_none());
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
