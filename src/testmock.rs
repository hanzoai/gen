//! A tiny in-process HTTP mock of cloud's `/v1/agents/sessions` control plane,
//! used to prove the session client + orchestration end-to-end without a live
//! cloud. Hand-rolled over TCP (same approach as `iam::oauth`'s loopback test)
//! so no test-only HTTP dependency is pulled in.

#![cfg(test)]

use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One request the mock observed, captured for assertions.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<String> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == name)
            .map(|(_, v)| v.clone())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Clone)]
struct Config {
    /// If set, every request gets this status (to prove non-2xx handling).
    force_status: Option<u16>,
    /// Status string returned by GET /v1/agents/sessions/:id.
    get_status: String,
    /// When true, GET/PATCH on a target id returns 404 (target gone / other org),
    /// so the run-target sync's register fallback can be proven.
    targets_missing: bool,
    /// If set, GET /v1/agents/sessions/:id returns this status while register
    /// still succeeds — the shape of a real 403 (another org), 404 (gone) or 5xx.
    session_get_code: Option<u16>,
    /// Steering commands this mock hands back on the control drain, as
    /// `(seq, command, message)`. A drain returns those with `seq > after`,
    /// oldest first — the same cursor contract as cloud's `drainControl`.
    control: Vec<(i64, String, String)>,
    /// The ONE session id this mock admits to owning. `None` means "every id is
    /// ours" (the ordinary single-tenant fixture). `Some(id)` makes every OTHER
    /// id a 404 on both the detail read and the control drain — which is exactly
    /// what cloud does for a session belonging to another org, and what the
    /// cross-org test needs in order to be adversarial rather than decorative.
    owned: Option<String>,
    /// Withhold the control queue for this many polls before serving it. Real
    /// steering races the backend's start-up: a command that lands before the
    /// backend has disclosed its session id is applied to a run with no resume
    /// handle. Both orderings are legitimate and both are exercised — this knob
    /// pins WHICH one a given test asserts, so the suite cannot pass or fail on
    /// scheduler luck.
    control_delay_polls: usize,
    /// Polls observed so far, shared across connections.
    polls: Arc<std::sync::atomic::AtomicUsize>,
}

/// The ordinary control plane: everything succeeds. Each constructor below
/// states ONLY the one thing it varies.
impl Default for Config {
    fn default() -> Self {
        Config {
            force_status: None,
            get_status: "paused".into(),
            targets_missing: false,
            session_get_code: None,
            control: Vec::new(),
            owned: None,
            control_delay_polls: 0,
            polls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

pub struct MockCloud {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockCloud {
    pub async fn start() -> MockCloud {
        Self::with(Config::default()).await
    }

    /// A mock whose GET returns the given session status (resume tests).
    pub async fn start_get_status(status: &str) -> MockCloud {
        Self::with(Config { get_status: status.into(), ..Config::default() }).await
    }

    /// A mock that answers every request with `code` (error-path tests).
    pub async fn start_status(code: u16) -> MockCloud {
        Self::with(Config { force_status: Some(code), ..Config::default() }).await
    }

    /// A mock that 404s a target by id (register still works) — proves the
    /// run-target sync heartbeat→register fallback.
    pub async fn start_target_missing() -> MockCloud {
        Self::with(Config { targets_missing: true, ..Config::default() }).await
    }

    /// A mock that fails GET on a session id with `code` while register still
    /// works: exactly what the CLI sees for another org's session (403), a
    /// deleted one (404), or a control-plane blip (5xx).
    pub async fn start_session_get_failing(code: u16) -> MockCloud {
        Self::with(Config { session_get_code: Some(code), ..Config::default() }).await
    }

    /// A mock whose control drain hands back `cmds` as `(seq, command, message)`.
    pub async fn start_with_control(cmds: &[(i64, &str, &str)]) -> MockCloud {
        let control =
            cmds.iter().map(|(s, c, m)| (*s, (*c).to_string(), (*m).to_string())).collect();
        Self::with(Config { control, ..Config::default() }).await
    }

    /// A mock that withholds `cmds` until after `delay` polls — so the backend
    /// has certainly disclosed its session id before the command lands.
    pub async fn start_with_delayed_control(delay: usize, cmds: &[(i64, &str, &str)]) -> MockCloud {
        let control =
            cmds.iter().map(|(s, c, m)| (*s, (*c).to_string(), (*m).to_string())).collect();
        Self::with(Config { control, control_delay_polls: delay, ..Config::default() }).await
    }

    /// A mock that owns exactly ONE session id and 404s every other — cloud's
    /// cross-org behaviour, so a poller aimed at a foreign session gets nothing
    /// but a refusal.
    pub async fn start_org_scoped(owner: &str, cmds: &[(i64, &str, &str)]) -> MockCloud {
        let control =
            cmds.iter().map(|(s, c, m)| (*s, (*c).to_string(), (*m).to_string())).collect();
        Self::with(Config { control, owned: Some(owner.to_string()), ..Config::default() }).await
    }

    async fn with(cfg: Config) -> MockCloud {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let reqs = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let reqs = reqs.clone();
                let cfg = cfg.clone();
                tokio::spawn(async move { serve_conn(stream, reqs, cfg).await });
            }
        });
        MockCloud { port, requests }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve_conn(
    mut stream: tokio::net::TcpStream,
    reqs: Arc<Mutex<Vec<Recorded>>>,
    cfg: Config,
) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // Read until we have a full header block.
        let head_end = loop {
            if let Some(pos) = find(&buf, b"\r\n\r\n") {
                break pos;
            }
            let mut chunk = [0u8; 4096];
            match stream.read(&mut chunk).await {
                Ok(0) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => return,
            }
        };

        let header_text = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = header_text.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();

        let mut headers = Vec::new();
        let mut content_length = 0usize;
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_string();
                let v = v.trim().to_string();
                if k.eq_ignore_ascii_case("content-length") {
                    content_length = v.parse().unwrap_or(0);
                }
                headers.push((k, v));
            }
        }

        // Read the body (exactly content_length bytes past the header block).
        let body_start = head_end + 4;
        while buf.len() < body_start + content_length {
            let mut chunk = [0u8; 4096];
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => return,
            }
        }
        let body = String::from_utf8_lossy(&buf[body_start..(body_start + content_length).min(buf.len())])
            .to_string();

        reqs.lock().unwrap().push(Recorded {
            method: method.clone(),
            path: path.clone(),
            headers,
            body,
        });

        let (status, payload) = respond(&cfg, &method, &path);
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{payload}",
            payload.len()
        );
        if stream.write_all(resp.as_bytes()).await.is_err() {
            return;
        }
        let _ = stream.flush().await;

        // Drop the consumed request; keep any pipelined remainder.
        let consumed = body_start + content_length;
        buf.drain(..consumed.min(buf.len()));
    }
}

fn respond(cfg: &Config, method: &str, path: &str) -> (String, String) {
    if let Some(code) = cfg.force_status {
        return (format!("{code} Error"), r#"{"error":"forced"}"#.to_string());
    }
    // register -> 201 with a minted id
    if method == "POST" && path == "/v1/agents/sessions" {
        let id = "sess_mock";
        return (
            "201 Created".into(),
            format!(r#"{{"id":"{id}","rootSessionId":"{id}","status":"running"}}"#),
        );
    }
    // run-target register (upsert-by-host) -> a targetView with an id.
    if method == "POST" && path == "/v1/agents/targets" {
        return (
            "201 Created".into(),
            r#"{"id":"tgt_mock","label":"evo","kind":"gpu","status":"online","sessions":0,"running":0}"#.to_string(),
        );
    }
    // run-target detail / heartbeat -> 404 when the target is "gone", else echo id.
    if (method == "PATCH" || method == "GET") && path.starts_with("/v1/agents/targets/") {
        if cfg.targets_missing {
            return ("404 Not Found".into(), r#"{"error":"target not found"}"#.to_string());
        }
        let id = path.trim_start_matches("/v1/agents/targets/");
        return (
            "200 OK".into(),
            format!(r#"{{"id":"{id}","label":"evo","kind":"gpu","status":"online","sessions":0,"running":0}}"#),
        );
    }
    // Control drain -> the commands newer than ?after=, oldest first, plus the
    // cursor to poll from next. MUST precede the detail branch below: this path
    // also starts with "/v1/agents/sessions/".
    if method == "GET" && path.contains("/control") && path.starts_with("/v1/agents/sessions/") {
        let (target, query) = path.split_once('?').unwrap_or((path, ""));
        let id = target
            .trim_start_matches("/v1/agents/sessions/")
            .trim_end_matches("/control");
        // Org scoping: a session this tenant does not own is a 404 — never a 200
        // carrying another org's commands.
        if let Some(owner) = &cfg.owned {
            if owner != id {
                return ("404 Not Found".into(), r#"{"error":"session not found"}"#.to_string());
            }
        }
        // Withhold the queue for the configured number of polls.
        let seen = cfg.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let after_raw: i64 = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("after="))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if seen < cfg.control_delay_polls {
            return ("200 OK".into(), format!(r#"{{"commands":[],"cursor":{after_raw}}}"#));
        }
        let after: i64 = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("after="))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut cursor = after;
        let mut items: Vec<String> = Vec::new();
        for (seq, command, message) in cfg.control.iter().filter(|(s, _, _)| *s > after) {
            cursor = cursor.max(*seq);
            items.push(format!(
                r#"{{"seq":{seq},"command":"{command}","message":"{message}"}}"#
            ));
        }
        return (
            "200 OK".into(),
            format!(r#"{{"commands":[{}],"cursor":{cursor}}}"#, items.join(",")),
        );
    }
    // GET detail -> configured status
    if method == "GET" && path.starts_with("/v1/agents/sessions/") {
        // Same org gate as the drain: a foreign id is never readable.
        if let Some(owner) = &cfg.owned {
            let id = path.trim_start_matches("/v1/agents/sessions/");
            if owner != id {
                return ("404 Not Found".into(), r#"{"error":"session not found"}"#.to_string());
            }
        }
        if let Some(code) = cfg.session_get_code {
            let line = match code {
                403 => "403 Forbidden",
                404 => "404 Not Found",
                _ => "500 Internal Server Error",
            };
            return (line.into(), r#"{"error":"refused"}"#.to_string());
        }
        let id = path.trim_start_matches("/v1/agents/sessions/");
        return (
            "200 OK".into(),
            format!(
                r#"{{"id":"{id}","status":"{}","rootSessionId":"{id}"}}"#,
                cfg.get_status
            ),
        );
    }
    // ---- the KMS secret plane: /v1/kms/orgs/{org}/secrets ------------------
    // Enough shape to prove the wire contract (org in path, bearer, env in
    // query, value out of argv); the real store is cloud's.
    if let Some(rest) = kms_secrets(path) {
        let (target, query) = match rest.split_once('?') {
            Some((t, q)) => (t, q),
            None => (rest, ""),
        };
        let env = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("env="))
            .unwrap_or("default")
            .to_string();
        if method == "POST" {
            return ("200 OK".into(), r#"{"stored":true,"name":"mock","env":"prod"}"#.to_string());
        }
        if method == "DELETE" {
            return ("200 OK".into(), r#"{"deleted":true,"name":"mock","env":"prod"}"#.to_string());
        }
        if target.is_empty() {
            // list: metadata only — this response CANNOT carry a value.
            return (
                "200 OK".into(),
                format!(
                    r#"{{"secrets":[{{"path":"/orgs/hanzo","name":"DB","env":"{env}","version":1}},{{"path":"/orgs/hanzo/ci","name":"TOKEN","env":"{env}","version":2}}],"total":2}}"#
                ),
            );
        }
        let name = target.rsplit('/').next().unwrap_or(target);
        return (
            "200 OK".into(),
            format!(r#"{{"name":"{name}","env":"{env}","value":"s3cr3t"}}"#),
        );
    }

    // ---- the money plane -------------------------------------------------
    // GET balance -> the {balance,holds,available} CENTS wire cloud answers
    // (clients/billing/billing.go `balance`; co-resident it carries no currency).
    if method == "GET" && path.starts_with("/v1/billing/balance") {
        return (
            "200 OK".into(),
            r#"{"balance":125000,"holds":0,"available":125000}"#.to_string(),
        );
    }
    // events -> 201, patch/control -> 200
    if method == "POST" && path.ends_with("/events") {
        return ("201 Created".into(), r#"{"id":"evt_mock"}"#.to_string());
    }
    ("200 OK".into(), "{}".to_string())
}

/// The `secrets` sub-path (+ query) of a /v1/kms/orgs/{org}/secrets route, or
/// None when the path is not one. Mirrors the route's own shape so a request
/// that misses the org segment simply does not match.
fn kms_secrets(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/v1/kms/orgs/")?;
    let (_org, rest) = rest.split_once('/')?;
    let rest = rest.strip_prefix("secrets")?;
    Some(rest.strip_prefix('/').unwrap_or(rest))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
