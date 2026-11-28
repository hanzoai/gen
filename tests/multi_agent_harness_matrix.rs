use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

const STANDIN_SRC: &str = r#####"
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

fn jesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn jstr_at(s: &str, start: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    if bytes.get(start) != Some(&b'"') { return None; }
    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                other => out.push(other as char),
            }
            i += 2;
        } else if bytes[i] == b'"' {
            return Some((out, i + 1));
        } else {
            let ch = s[i..].chars().next()?;
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    None
}

fn jarr(raw: &str) -> Vec<String> {
    let mut items = Vec::new();
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            if let Some((v, next)) = jstr_at(raw, i) {
                items.push(v);
                i = next;
                continue;
            }
        }
        i += 1;
    }
    items
}

fn json_field_str(doc: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let pos = doc.find(&pat)? + pat.len();
    let tail = doc[pos..].trim_start();
    jstr_at(tail, 0).map(|(v, _)| v)
}

fn json_field_arr(doc: &str, key: &str) -> Option<Vec<String>> {
    let pat = format!("\"{key}\":");
    let pos = doc.find(&pat)? + pat.len();
    let tail = doc[pos..].trim_start();
    if !tail.starts_with('[') { return None; }
    let end = tail.find(']')?;
    Some(jarr(&tail[..=end]))
}

fn invoke_mcp(cmd: &str, args: &[String]) -> (bool, String) {
    let mut child = match Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (false, e.to_string()),
    };
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"matrix","version":"1"}}}"#;
    let inited = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"delegate","arguments":{"task":"ping"}}}"#;
    let _ = writeln!(stdin, "{init}");
    let _ = stdin.flush();
    let mut line1 = String::new();
    let _ = stdout.read_line(&mut line1);
    let _ = writeln!(stdin, "{inited}");
    let _ = writeln!(stdin, "{call}");
    let _ = stdin.flush();
    let mut line2 = String::new();
    let _ = stdout.read_line(&mut line2);
    drop(stdin);
    let _ = child.wait();
    let ok = line1.contains("\"id\":1") && line2.contains("\"id\":2") && line2.contains("enso-pro");
    (ok, format!("{line1}|{line2}"))
}

fn run_mcp_server(cap_dir: &str) {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let id = line
            .find("\"id\":")
            .map(|i| {
                let rest = line[i + 5..].trim_start();
                if rest.starts_with('"') {
                    jstr_at(rest, 0).map(|(s, _)| jesc(&s)).unwrap_or_else(|| "null".into())
                } else {
                    rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>()
                }
            })
            .unwrap_or_else(|| "null".into());
        if line.contains("\"initialize\"") {
            let _ = writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"hanzo-mcp\",\"version\":\"0.1.0\"}}}}}}");
        } else if line.contains("\"tools/list\"") {
            let _ = writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"tools\":[{{\"name\":\"delegate\",\"description\":\"delegate\"}}]}}}}");
        } else if line.contains("\"tools/call\"") {
            if !cap_dir.is_empty() {
                let n = fs::read_dir(cap_dir).map(|r| r.count()).unwrap_or(0);
                let _ = fs::write(Path::new(cap_dir).join(format!("mcp-call-{n}.json")), &line);
            }
            let _ = writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{{\\\"model\\\":\\\"enso-pro\\\",\\\"usage\\\":{{\\\"input_tokens\\\":40,\\\"output_tokens\\\":20}}}}\"}}]}}}}");
        } else if id != "null" {
            let _ = writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}");
        }
        let _ = out.flush();
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let exe = args.first().map(String::as_str).unwrap_or("");
    let stem = Path::new(exe).file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if args.iter().skip(1).any(|a| a == "--version" || a == "-V") {
        println!("{stem} 1.0.0-matrix");
        return;
    }
    let cap_dir = env::var("HANZO_TEST_CAPTURE_DIR").unwrap_or_default();
    if stem == "hanzo-mcp" {
        run_mcp_server(&cap_dir);
        return;
    }
    if stem == "hanzo" {
        let sub: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
        if sub.starts_with(&["config", "profile"]) {
            let url = env::var("HANZO_TEST_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:9".into());
            println!("{{\"api\":{},\"local\":true,\"org\":null,\"provider\":null,\"identities\":[[\"test\",\"one\"]],\"link\":true,\"theme\":\"none\"}}", jesc(&url));
            return;
        }
        if sub.starts_with(&["auth", "token"]) {
            println!("eyJhbGciOiJub25lIn0.eyJvd25lciI6InRlc3QiLCJuYW1lIjoib25lIn0.x");
            return;
        }
    }

    let mut mcp_cmd = None;
    let mut mcp_args: Vec<String> = Vec::new();
    let mut mcp_cfg_body = String::new();
    for (i, a) in args.iter().enumerate() {
        if let Some(v) = a.strip_prefix("mcp_servers.hanzo.command=") {
            mcp_cmd = jstr_at(v, 0).map(|(s, _)| s);
        } else if let Some(v) = a.strip_prefix("mcp_servers.hanzo.args=") {
            mcp_args = jarr(v);
        } else if a == "--mcp-config" {
            if let Some(p) = args.get(i + 1) {
                mcp_cfg_body = fs::read_to_string(p).unwrap_or_default();
                let target = mcp_cfg_body.find("\"hanzo\"").map(|idx| &mcp_cfg_body[idx..]).unwrap_or(&mcp_cfg_body);
                mcp_cmd = json_field_str(target, "command");
                mcp_args = json_field_arr(target, "args").unwrap_or_default();
            }
        }
    }

    let (mcp_ok, mcp_diag) = match (&mcp_cmd, mcp_args.is_empty()) {
        (Some(cmd), false) => invoke_mcp(cmd, &mcp_args),
        _ => (false, "none".into()),
    };

    if !cap_dir.is_empty() {
        let argv_json: Vec<String> = args[1..].iter().map(|s| jesc(s)).collect();
        let keys = ["HANZO_USER_KEY", "HANZO_API_KEY", "HANZO_API_URL", "ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "OPENAI_BASE_URL", "OPENAI_API_KEY", "GEMINI_CLI_SYSTEM_SETTINGS_PATH"];
        let envs: Vec<String> = keys.iter().filter_map(|k| env::var(k).ok().map(|v| format!("{}:{}", jesc(k), jesc(&v)))).collect();
        let body = format!(
            "{{\"argv\":[{}],\"envs\":{{{}}},\"mcpConfig\":{},\"mcpOk\":{},\"mcpDiag\":{}}}",
            argv_json.join(","),
            envs.join(","),
            if mcp_cfg_body.is_empty() { "null".to_string() } else { mcp_cfg_body },
            mcp_ok,
            jesc(&mcp_diag)
        );
        let _ = fs::write(Path::new(&cap_dir).join(format!("{stem}.json")), body);
    }

    match stem {
        "claude" => {
            println!("{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"c-1\",\"model\":\"enso\"}}");
            println!("{{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"c-1\",\"is_error\":false,\"num_turns\":2,\"usage\":{{\"input_tokens\":100,\"cache_read_input_tokens\":20,\"cache_creation_input_tokens\":5,\"output_tokens\":50}}}}");
        }
        "gemini" => {
            println!("{{\"type\":\"init\",\"model\":\"gemini-2.5-pro\"}}");
            println!("{{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"ok\",\"delta\":false}}");
            println!("{{\"type\":\"result\",\"status\":\"success\",\"stats\":{{\"input_tokens\":80,\"cached\":10,\"output_tokens\":40,\"duration_ms\":100}}}}");
        }
        _ => {
            println!("{{\"type\":\"thread.started\",\"thread_id\":\"t-1\"}}");
            println!("{{\"type\":\"item.completed\",\"item\":{{\"id\":\"i1\",\"type\":\"agent_message\",\"text\":\"done\"}}}}");
            println!("{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":100,\"cached_input_tokens\":20,\"output_tokens\":50}}}}");
        }
    }
}
"#####;

fn standin_binary() -> &'static PathBuf {
    static BIN: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &BIN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("standin.rs");
        let exe = dir
            .path()
            .join(format!("standin{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&src, STANDIN_SRC).unwrap();
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let mut cmd = Command::new(&rustc);
        cmd.arg("--edition")
            .arg("2021")
            .arg(&src)
            .arg("-o")
            .arg(&exe);
        cmd.env("HOME", dir.path()).env("USERPROFILE", dir.path());
        if std::env::var_os("RUSTUP_HOME").is_none() {
            if let Some(real_home) = std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.join(".rustup").is_dir())
            {
                cmd.env("RUSTUP_HOME", real_home.join(".rustup"));
            }
        }
        let out = cmd.output().expect("compile stand-in helper with rustc");
        assert!(
            out.status.success(),
            "rustc failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        (dir, exe)
    })
    .1
}

struct MockCloud {
    url: String,
    requests: Arc<Mutex<Vec<(String, String)>>>,
}

impl MockCloud {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let reqs_bg = Arc::clone(&requests);
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let reqs = Arc::clone(&reqs_bg);
                std::thread::spawn(move || handle_http(conn, reqs));
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }

    fn snapshot(&self) -> Vec<(String, String)> {
        self.requests.lock().unwrap().clone()
    }
}

fn handle_http(conn: std::net::TcpStream, reqs: Arc<Mutex<Vec<(String, String)>>>) {
    let mut out = conn.try_clone().unwrap();
    let mut r = BufReader::new(conn);
    loop {
        let mut head = String::new();
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 2 {
            head.push_str(&line);
            line.clear();
        }
        if head.is_empty() {
            return;
        }
        let len = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        let mut body = vec![0; len];
        if len > 0 && r.read_exact(&mut body).is_err() {
            return;
        }
        let first = head.lines().next().unwrap_or_default().to_string();
        let target = first
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        reqs.lock()
            .unwrap()
            .push((target.clone(), String::from_utf8_lossy(&body).into_owned()));
        let reply = if target == "GET /v1/models" {
            r#"{"data":[{"id":"enso","free":false,"pricing":{"prompt":"0.000003","completion":"0.000015","rates":{"input_cache_read":"0.0000003","input_cache_write":"0.00000375"}}}]}"#
        } else if target == "POST /v1/agents/sessions" {
            r#"{"id":"sess_matrix1"}"#
        } else if target.contains("/control") {
            r#"{"commands":[],"cursor":0}"#
        } else {
            r#"{"ok":true}"#
        };
        if write!(out, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{reply}", reply.len()).is_err() {
            return;
        }
    }
}

struct HarnessFixture {
    _root: tempfile::TempDir,
    bin_dir: PathBuf,
    repo_dir: PathBuf,
    home_dir: PathBuf,
}

impl HarnessFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin_dir = root.path().join("bin");
        let repo_dir = root.path().join("repo");
        let home_dir = root.path().join("home");
        let local_bin = home_dir.join(".local").join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&repo_dir).unwrap();
        std::fs::create_dir_all(&local_bin).unwrap();
        std::fs::write(repo_dir.join("README.md"), "# fixture\n").unwrap();
        let helper = standin_binary();
        for name in ["hanzo", "dev", "claude", "codex", "gemini", "hanzo-mcp"] {
            let dst = bin_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            std::fs::copy(helper, &dst).unwrap();
        }
        std::fs::copy(
            helper,
            local_bin.join(format!("hanzo-mcp{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
        Self {
            _root: root,
            bin_dir,
            repo_dir,
            home_dir,
        }
    }

    fn run_gen(&self, args: &[&str], capture_dir: &Path, mock_url: &str) -> std::process::Output {
        let mut path_entries = vec![self.bin_dir.clone()];
        #[cfg(windows)]
        {
            if let Some(sys) = std::env::var_os("SystemRoot").map(PathBuf::from) {
                path_entries.push(sys.join("System32"));
                path_entries.push(sys);
            }
        }
        #[cfg(unix)]
        {
            path_entries.push(PathBuf::from("/usr/bin"));
            path_entries.push(PathBuf::from("/bin"));
        }
        let path_val = std::env::join_paths(path_entries).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gen"));
        cmd.env_clear()
            .current_dir(&self.repo_dir)
            .args(args)
            .env("HOME", &self.home_dir)
            .env("USERPROFILE", &self.home_dir)
            .env("LOCALAPPDATA", self.home_dir.join("appdata"))
            .env("PATH", path_val)
            .env("NO_COLOR", "1")
            .env("HANZO_NO_AUTO_MCP", "1")
            .env("HANZO_TEST_CAPTURE_DIR", capture_dir)
            .env("HANZO_TEST_MOCK_URL", mock_url)
            .stdin(Stdio::null());
        #[cfg(windows)]
        {
            cmd.env(
                "PATHEXT",
                std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into()),
            );
            if let Some(sys) = std::env::var_os("SystemRoot") {
                cmd.env("SystemRoot", sys);
            }
        }
        cmd.output().expect("execute gen binary")
    }
}

fn read_json(path: &Path) -> Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("missing {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("invalid json {}: {e}\n{text}", path.display()))
}

fn ledgers_under(home: &Path) -> Vec<PathBuf> {
    let dir = hanzo_gen::ledger::path(&hanzo_gen::root::Root::at(home), "id")
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect()
}

fn assert_mcp_args(argv: &[String], expected_repo: &Path) {
    let args_entry = argv
        .iter()
        .find(|a| a.starts_with("mcp_servers.hanzo.args="))
        .unwrap_or_else(|| panic!("missing mcp_servers.hanzo.args in {argv:?}"));
    assert!(
        args_entry.contains("__mcp")
            && args_entry.contains("--ledger")
            && args_entry.contains("hanzo-mcp"),
        "{args_entry}"
    );
    assert!(args_entry.contains("--project-dir"), "{args_entry}");
    let norm_repo = expected_repo.file_name().unwrap().to_str().unwrap();
    assert!(args_entry.contains(norm_repo), "{args_entry}");
}

#[test]
fn headless_matrix_attaches_mcp_streams_cloud_and_prints_handoff_summary() {
    let cloud = MockCloud::start();
    let fixture = HarnessFixture::new();
    let cases: [(&str, &[&str], bool); 4] = [
        ("dev", &["inspect repo"], true),
        ("claude", &["--claude", "inspect repo"], true),
        ("codex", &["--codex", "inspect repo"], true),
        ("gemini", &["--gemini", "inspect repo"], false),
    ];

    for (stem, cli_args, expects_mcp) in cases {
        let cap_dir = fixture._root.path().join(format!("cap-{stem}"));
        std::fs::create_dir_all(&cap_dir).unwrap();
        let before_reqs = cloud.snapshot().len();
        let before_ledgers = ledgers_under(&fixture.home_dir).len();

        let out = fixture.run_gen(cli_args, &cap_dir, &cloud.url);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "{stem} failed: stdout={stdout}\nstderr={stderr}"
        );

        let cap = read_json(&cap_dir.join(format!("{stem}.json")));
        let argv: Vec<String> = cap["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();

        if stem == "claude" {
            assert!(argv.iter().any(|a| a == "--strict-mcp-config"), "{argv:?}");
            assert_eq!(
                cap["mcpConfig"]["mcpServers"]["hanzo"]["type"].as_str(),
                Some("stdio")
            );
            let margs = cap["mcpConfig"]["mcpServers"]["hanzo"]["args"].to_string();
            assert!(
                margs.contains("__mcp")
                    && margs.contains("--ledger")
                    && margs.contains("hanzo-mcp"),
                "{margs}"
            );
        } else if expects_mcp {
            assert_mcp_args(&argv, &fixture.repo_dir);
            assert!(
                argv.iter()
                    .any(|a| a.starts_with("mcp_servers.hanzo.command=")),
                "{argv:?}"
            );
        } else {
            assert!(!argv.iter().any(|a| a.contains("mcp_servers")), "{argv:?}");
            assert!(
                cap["envs"].get("GEMINI_CLI_SYSTEM_SETTINGS_PATH").is_none(),
                "{cap}"
            );
        }

        if expects_mcp {
            assert_eq!(
                cap["mcpOk"].as_bool(),
                Some(true),
                "{stem} mcp handshake failed: {cap}"
            );
            let ledgers = ledgers_under(&fixture.home_dir);
            assert_eq!(
                ledgers.len(),
                before_ledgers + 1,
                "expected new ledger for {stem}"
            );
            let newest = ledgers
                .iter()
                .max_by_key(|p| std::fs::metadata(p).unwrap().modified().unwrap())
                .unwrap();
            let entries = hanzo_gen::ledger::read(newest);
            assert_eq!(entries.len(), 1, "{entries:?}");
            assert_eq!(
                (
                    entries[0].tool.as_str(),
                    entries[0].ok,
                    entries[0].model.as_deref()
                ),
                ("delegate", true, Some("enso-pro"))
            );
            assert_eq!((entries[0].input, entries[0].output), (Some(40), Some(20)));
        }

        assert!(
            stdout.contains("hanzo dev · session summary"),
            "{stem}: {stdout}"
        );
        assert!(
            stdout.contains(&format!("(reported by {stem})")),
            "{stem}: {stdout}"
        );
        assert!(
            stdout.contains("https://hanzo.ai/tabs/sess_matrix1"),
            "{stem}: {stdout}"
        );
        if expects_mcp {
            assert!(
                stdout.contains("hanzo tools") && stdout.contains("1 call"),
                "{stem}: {stdout}"
            );
            assert!(
                stdout.contains("1 call handed to enso-pro"),
                "{stem}: {stdout}"
            );
        } else {
            assert!(
                stdout.contains("gemini takes no MCP server for one run"),
                "{stdout}"
            );
            assert!(stdout.contains("gemini ran on its own account"), "{stdout}");
        }

        let after_reqs = &cloud.snapshot()[before_reqs..];
        let paths: Vec<&str> = after_reqs.iter().map(|(t, _)| t.as_str()).collect();
        assert!(
            paths.contains(&"POST /v1/agents/sessions"),
            "{stem}: {paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|p| p == &"POST /v1/agents/sessions/sess_matrix1/events"),
            "{stem}: {paths:?}"
        );
        assert!(
            paths.contains(&"PATCH /v1/agents/sessions/sess_matrix1"),
            "{stem}: {paths:?}"
        );
        let patch_body = after_reqs
            .iter()
            .rev()
            .find(|(t, _)| t == "PATCH /v1/agents/sessions/sess_matrix1")
            .unwrap()
            .1
            .clone();
        let patch_json: Value = serde_json::from_str(&patch_body).unwrap();
        assert_eq!(patch_json["status"].as_str(), Some("done"), "{patch_json}");
    }
}

#[test]
fn cli_flag_matrix_parses_headless_single_prompt_variants() {
    use clap::{CommandFactory, FromArgMatches, Parser};
    use hanzo_gen::args::{expand, Args};
    use hanzo_gen::backend::BackendKind;
    use hanzo_gen::root::Root;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    struct FlagCase<'a> {
        argv: &'a [&'a str],
        backend: BackendKind,
        mcp: bool,
        project_mcp: bool,
        no_link: bool,
        ask: bool,
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = Root::at(tmp.path());
    let gemini = BackendKind::parse("gemini").unwrap();
    let cases = [
        FlagCase {
            argv: &["gen", "audit"],
            backend: BackendKind::Dev,
            mcp: true,
            project_mcp: false,
            no_link: false,
            ask: false,
        },
        FlagCase {
            argv: &["gen", "--claude", "audit"],
            backend: BackendKind::Claude,
            mcp: true,
            project_mcp: false,
            no_link: false,
            ask: false,
        },
        FlagCase {
            argv: &["gen", "--codex", "audit"],
            backend: BackendKind::Codex,
            mcp: true,
            project_mcp: false,
            no_link: false,
            ask: false,
        },
        FlagCase {
            argv: &["gen", "--gemini", "audit"],
            backend: gemini,
            mcp: true,
            project_mcp: false,
            no_link: false,
            ask: false,
        },
        FlagCase {
            argv: &["gen", "--no-mcp", "audit"],
            backend: BackendKind::Dev,
            mcp: false,
            project_mcp: false,
            no_link: false,
            ask: false,
        },
        FlagCase {
            argv: &["gen", "--claude", "--project-mcp", "audit"],
            backend: BackendKind::Claude,
            mcp: true,
            project_mcp: true,
            no_link: false,
            ask: false,
        },
        FlagCase {
            argv: &["gen", "--no-link", "audit"],
            backend: BackendKind::Dev,
            mcp: true,
            project_mcp: false,
            no_link: true,
            ask: false,
        },
        FlagCase {
            argv: &["gen", "--ask", "audit"],
            backend: BackendKind::Dev,
            mcp: true,
            project_mcp: false,
            no_link: false,
            ask: true,
        },
    ];

    let expected: BTreeMap<&str, &str> = BTreeMap::from([("task", "audit")]);
    for c in cases {
        let raw: Vec<String> = c.argv.iter().map(|s| (*s).to_string()).collect();
        let expanded = expand(&Cli::command(), raw);
        let matches = Cli::command()
            .try_get_matches_from(expanded)
            .unwrap_or_else(|e| panic!("{:?}: {e}", c.argv));
        let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| panic!("{:?}: {e}", c.argv));
        let opts = cli.args.into_options(root.clone()).unwrap();
        assert_eq!(opts.backend, c.backend, "{:?}", c.argv);
        assert_eq!(opts.task.as_deref(), Some(expected["task"]), "{:?}", c.argv);
        assert_eq!(opts.mcp, c.mcp, "{:?}", c.argv);
        assert_eq!(opts.project_mcp, c.project_mcp, "{:?}", c.argv);
        assert_eq!(opts.no_link, c.no_link, "{:?}", c.argv);
        assert_eq!(opts.ask, c.ask, "{:?}", c.argv);
    }
}
