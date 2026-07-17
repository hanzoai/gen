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
    /// When true, POST /v1/billing/deposit answers commerce's VERBATIM
    /// PlatformOnly refusal — the deposit-403 incident, reproduced.
    deposit_refused: bool,
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
            deposit_refused: false,
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

    /// A mock that refuses a deposit exactly as commerce's `PlatformOnly` gate
    /// does for a caller who is neither the internal service token nor a
    /// SuperAdmin — the incident, on the wire.
    pub async fn start_deposit_refused() -> MockCloud {
        Self::with(Config { deposit_refused: true, ..Config::default() }).await
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
    // GET detail -> configured status
    if method == "GET" && path.starts_with("/v1/agents/sessions/") {
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
    // POST deposit -> commerce's 201 receipt (api/billing/deposit.go), or the
    // VERBATIM PlatformOnly 403 envelope (middleware/platformonly.go →
    // util/json/http.Fail, which nests the message under `error`).
    if method == "POST" && path == "/v1/billing/deposit" {
        if cfg.deposit_refused {
            return (
                "403 Forbidden".into(),
                r#"{"error":{"type":"api-error","message":"This operation requires platform-administrator or internal-service credentials."}}"#
                    .to_string(),
            );
        }
        return (
            "201 Created".into(),
            r#"{"transactionId":"txn_mock","user":"hanzo","amount":5000,"currency":"usd","type":"deposit","tags":""}"#.to_string(),
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
