//! `hanzo __mcp -- <server> <args…>`: a lazy stdio proxy for the Hanzo toolset.
//!
//! A coding agent waits for every MCP server it is handed before its first model
//! call, and `hanzo-mcp` is a Python process whose import of the cloud client alone
//! takes 5 to 30 seconds. The proxy answers the handshake and the lists from the
//! last answer the real server gave, and starts the real server on the first call
//! that needs it (`tools/call`, `prompts/get`, `resources/read`, …). The first
//! session on a machine, or after the server changes, has no answer to give: the
//! proxy then starts the server at once, as the agent would have, and keeps what it
//! learns for the next one.
//!
//! The answer is kept in `~/.hanzo/mcp/<key>.json`, owner-only. `<key>` is the
//! server's resolved path, its modification time and size, its arguments less the
//! project directory, and the protocol version the agent speaks, so a new server
//! or a new client is never served an old answer. A server that does start is
//! asked again and the file rewritten if it differs.
//!
//! The proxy adds no capability: it forwards bytes between the agent and the one
//! command it was given, with the environment the agent gave it.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// A stored answer older than this is asked for again.
const TTL: u64 = 30 * 24 * 3600;
/// The real server gets this long to answer its handshake.
const HANDSHAKE: Duration = Duration::from_secs(180);
/// Bumped when the stored shape changes.
const FORMAT: u32 = 1;

/// What the real server last said, for one agent protocol version.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Manifest {
    key: String,
    at: u64,
    version: String,
    initialize: Value,
    tools: Vec<Value>,
    prompts: Option<Vec<Value>>,
    resources: Option<Vec<Value>>,
    templates: Option<Vec<Value>>,
}

/// One end of the real server: lines in, lines out, and what keeps it alive.
pub struct Link {
    pub tx: Box<dyn AsyncWrite + Send + Unpin>,
    pub rx: Box<dyn AsyncRead + Send + Unpin>,
    pub guard: Option<tokio::process::Child>,
}

pub type Spawn = Arc<dyn Fn() -> Result<Link> + Send + Sync>;

/// The method families that list what the server has, answered from the manifest.
fn listing(method: &str) -> Option<(&'static str, &'static str)> {
    match method {
        "tools/list" => Some(("tools", "tools")),
        "prompts/list" => Some(("prompts", "prompts")),
        "resources/list" => Some(("resources", "resources")),
        "resources/templates/list" => Some(("templates", "resourceTemplates")),
        _ => None,
    }
}

impl Manifest {
    fn list(&self, field: &str) -> Option<&Vec<Value>> {
        match field {
            "tools" => Some(&self.tools),
            "prompts" => self.prompts.as_ref(),
            "resources" => self.resources.as_ref(),
            _ => self.templates.as_ref(),
        }
    }
}

/// `~/.hanzo/mcp`.
pub fn cache_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".hanzo").join("mcp"))
}

/// The identity of an answer: which server, how it is run, and which protocol the agent speaks.
pub fn key(program: &str, args: &[String], version: &str) -> String {
    let mut h = Sha256::new();
    h.update(format!("hanzo-mcp-shim/{FORMAT}\0{version}\0").as_bytes());
    match which::which(program).ok().and_then(|p| std::fs::canonicalize(p).ok()) {
        Some(path) => {
            h.update(path.as_os_str().as_encoded_bytes());
            if let Ok(m) = std::fs::metadata(&path) {
                let secs = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
                h.update(format!("\0{secs}\0{}", m.len()).as_bytes());
            }
        }
        None => h.update(program.as_bytes()),
    }
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--project-dir" {
            skip = true;
            continue;
        }
        h.update(b"\0");
        h.update(a.as_bytes());
    }
    format!("{:x}", h.finalize())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn load(dir: &Path, key: &str) -> Option<Manifest> {
    let m: Manifest = serde_json::from_slice(&std::fs::read(dir.join(format!("{key}.json"))).ok()?).ok()?;
    (m.key == key && now().saturating_sub(m.at) < TTL).then_some(m)
}

fn store(dir: &Path, m: &Manifest) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let tmp = dir.join(format!("{}.{}.tmp", m.key, std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        std::io::Write::write_all(&mut f, &serde_json::to_vec(m)?)?;
    }
    std::fs::rename(&tmp, dir.join(format!("{}.json", m.key)))?;
    Ok(())
}

/// The real server as a command: its stdio piped, its stderr passed through, killed with the proxy.
pub fn command(program: &str, args: &[String]) -> Spawn {
    let (program, args) = (program.to_string(), args.to_vec());
    Arc::new(move || {
        let mut child = Command::new(&program)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {program}"))?;
        let tx = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let rx = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        Ok(Link { tx: Box::new(tx), rx: Box::new(rx), guard: Some(child) })
    })
}

/// Entry point of `hanzo __mcp -- <server> <args…>` over the process's stdio.
pub async fn main(argv: &[String]) -> Result<()> {
    let rest = argv.iter().position(|a| a == "--").map_or(argv, |i| &argv[i + 1..]);
    let (program, args) = rest.split_first().ok_or_else(|| anyhow!("usage: hanzo __mcp -- <server> [args…]"))?;
    let spawn = command(program, args);
    serve(tokio::io::stdin(), tokio::io::stdout(), spawn, program, args, cache_dir()).await
}

/// One message to the agent.
fn line(v: &Value) -> String {
    let mut s = v.to_string();
    s.push('\n');
    s
}

fn result(id: &Value, r: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": r })
}

fn failure(id: &Value, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": message } })
}

/// What the proxy has learned of the real server this session, to be stored.
#[derive(Default)]
struct Learn {
    initialize: Option<Value>,
    tools: Option<Vec<Value>>,
    prompts: Option<Vec<Value>>,
    resources: Option<Vec<Value>>,
    templates: Option<Vec<Value>>,
    asked: HashSet<&'static str>,
    done: HashSet<&'static str>,
}

struct Backend {
    tx: Box<dyn AsyncWrite + Send + Unpin>,
    rx: mpsc::UnboundedReceiver<Option<String>>,
    _guard: Option<tokio::process::Child>,
    inflight: HashSet<String>,
}

/// Reads the real server's lines on a task of their own, so a slow one never stalls the agent.
fn pump(rx: Box<dyn AsyncRead + Send + Unpin>) -> mpsc::UnboundedReceiver<Option<String>> {
    let (tx, out) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(rx).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if tx.send(Some(l)).is_err() {
                return;
            }
        }
        let _ = tx.send(None);
    });
    out
}

fn key_of(id: &Value) -> String {
    id.to_string()
}

/// Serves the agent on `input`/`output` until it closes `input`.
pub async fn serve<R, W>(input: R, mut output: W, spawn: Spawn, program: &str, args: &[String], dir: Option<PathBuf>) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut client = BufReader::new(input).lines();
    let mut manifest: Option<Manifest> = None;
    let mut init_params = Value::Null;
    let mut version = String::new();
    let mut backend: Option<Backend> = None;
    let mut learn = Learn::default();
    let mut dirty = false;

    loop {
        // A line from the agent or from the real server, whichever comes first.
        let from_backend = async {
            match backend.as_mut() {
                Some(b) => b.rx.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            l = client.next_line() => {
                let Some(l) = l? else { break };
                if l.trim().is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(&l) else {
                    // Not JSON: pass it on if there is anyone to pass it to.
                    if let Some(b) = backend.as_mut() {
                        b.tx.write_all(l.as_bytes()).await?;
                        b.tx.write_all(b"\n").await?;
                        b.tx.flush().await?;
                    }
                    continue;
                };
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                let id = msg.get("id").cloned();
                match (method, &id) {
                    ("initialize", Some(id)) => {
                        init_params = msg.get("params").cloned().unwrap_or(Value::Null);
                        version = init_params.get("protocolVersion").and_then(Value::as_str).unwrap_or("").to_string();
                        manifest = dir.as_deref().and_then(|d| load(d, &key(program, args, &version)));
                        match &manifest {
                            Some(m) => {
                                output.write_all(line(&result(id, m.initialize.clone())).as_bytes()).await?;
                                output.flush().await?;
                            }
                            None => {
                                let b = start(&spawn, &init_params, &mut learn).await?;
                                backend = Some(b);
                                dirty = true;
                                let init = learn.initialize.clone().unwrap_or(Value::Null);
                                output.write_all(line(&result(id, init)).as_bytes()).await?;
                                output.flush().await?;
                            }
                        }
                    }
                    ("notifications/initialized", None) => {}
                    ("ping", Some(id)) if backend.is_none() => {
                        output.write_all(line(&result(id, json!({}))).as_bytes()).await?;
                        output.flush().await?;
                    }
                    (m, Some(id)) if listing(m).is_some() && manifest.as_ref().is_some_and(|x| x.list(listing(m).unwrap().0).is_some()) => {
                        let (field, wire) = listing(m).unwrap();
                        let items = manifest.as_ref().and_then(|x| x.list(field)).cloned().unwrap_or_default();
                        let mut r = Map::new();
                        r.insert(wire.into(), Value::Array(items));
                        output.write_all(line(&result(id, Value::Object(r))).as_bytes()).await?;
                        output.flush().await?;
                    }
                    _ => {
                        if backend.is_none() {
                            if method.starts_with("notifications/") {
                                continue;
                            }
                            // The first call that needs the real server: start it now.
                            let b = start(&spawn, &init_params, &mut learn).await?;
                            backend = Some(b);
                            dirty = true;
                        }
                        let b = backend.as_mut().expect("started above");
                        if let Some(id) = &id {
                            if !method.is_empty() {
                                b.inflight.insert(key_of(id));
                            }
                        }
                        b.tx.write_all(l.as_bytes()).await?;
                        b.tx.write_all(b"\n").await?;
                        b.tx.flush().await?;
                    }
                }
                if dirty && backend.is_some() {
                    dirty = false;
                    if let Some(b) = backend.as_mut() {
                        ask(b, &mut learn).await?;
                    }
                }
            }
            m = from_backend => {
                match m {
                    Some(Some(l)) => {
                        let msg = serde_json::from_str::<Value>(&l).ok();
                        let id = msg.as_ref().and_then(|m| m.get("id")).cloned();
                        let is_response = msg.as_ref().is_some_and(|m| m.get("method").is_none());
                        match (&id, is_response) {
                            (Some(Value::String(s)), true) if s.starts_with("shim:") => {
                                if let Some(m) = &msg {
                                    absorb(&mut learn, s, m);
                                    if let Some(b) = backend.as_mut() {
                                        follow(b, s, m).await?;
                                    }
                                    if let (Some(d), Some(full)) = (dir.as_deref(), assemble(&learn, program, args, &version)) {
                                        if manifest.as_ref().map(|x| (&x.initialize, &x.tools, &x.prompts, &x.resources, &x.templates))
                                            != Some((&full.initialize, &full.tools, &full.prompts, &full.resources, &full.templates))
                                        {
                                            if store(d, &full).is_ok() {
                                                manifest = Some(full);
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {
                                if let (Some(id), true, Some(b)) = (&id, is_response, backend.as_mut()) {
                                    b.inflight.remove(&key_of(id));
                                }
                                output.write_all(l.as_bytes()).await?;
                                output.write_all(b"\n").await?;
                                output.flush().await?;
                            }
                        }
                    }
                    _ => {
                        // The real server is gone: answer what it still owed, and start it again on demand.
                        if let Some(b) = backend.take() {
                            for id in b.inflight {
                                if let Ok(id) = serde_json::from_str::<Value>(&id) {
                                    output.write_all(line(&failure(&id, "the MCP server exited")).as_bytes()).await?;
                                }
                            }
                            output.flush().await?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Starts the real server and completes its handshake with the agent's own `initialize` parameters.
async fn start(spawn: &Spawn, params: &Value, learn: &mut Learn) -> Result<Backend> {
    let Link { mut tx, rx, guard } = spawn()?;
    let mut rx = pump(rx);
    let init = json!({ "jsonrpc": "2.0", "id": "shim:init", "method": "initialize", "params": params });
    tx.write_all(line(&init).as_bytes()).await?;
    tx.flush().await?;
    let reply = tokio::time::timeout(HANDSHAKE, async {
        loop {
            match rx.recv().await {
                Some(Some(l)) => {
                    let Ok(m) = serde_json::from_str::<Value>(&l) else { continue };
                    if m.get("id").and_then(Value::as_str) == Some("shim:init") {
                        return Ok(m);
                    }
                    // A notification before the handshake ends: the agent has not asked yet, so drop it.
                }
                _ => return Err(anyhow!("the MCP server exited before it answered")),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("the MCP server did not answer in {}s", HANDSHAKE.as_secs()))??;
    learn.initialize = reply.get("result").cloned();
    tx.write_all(line(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).as_bytes()).await?;
    tx.flush().await?;
    Ok(Backend { tx, rx, _guard: guard, inflight: HashSet::new() })
}

/// Asks the real server what it has, under ids of the proxy's own.
async fn ask(b: &mut Backend, learn: &mut Learn) -> Result<()> {
    let caps = learn.initialize.as_ref().and_then(|i| i.get("capabilities")).cloned().unwrap_or(Value::Null);
    let mut want = vec![("tools", "tools/list")];
    if caps.get("prompts").is_some() {
        want.push(("prompts", "prompts/list"));
    }
    if caps.get("resources").is_some() {
        want.push(("resources", "resources/list"));
        want.push(("templates", "resources/templates/list"));
    }
    for (name, method) in want {
        if learn.asked.insert(name) {
            let req = json!({ "jsonrpc": "2.0", "id": format!("shim:{name}:0"), "method": method });
            b.tx.write_all(line(&req).as_bytes()).await?;
        }
    }
    b.tx.flush().await?;
    Ok(())
}

/// Keeps the items of one list page, and notes the list whole when no page follows.
fn absorb(learn: &mut Learn, id: &str, m: &Value) {
    let name = id.split(':').nth(1).unwrap_or("");
    let (slot, wire, tag) = match name {
        "tools" => (&mut learn.tools, "tools", "tools"),
        "prompts" => (&mut learn.prompts, "prompts", "prompts"),
        "resources" => (&mut learn.resources, "resources", "resources"),
        "templates" => (&mut learn.templates, "resourceTemplates", "templates"),
        _ => return,
    };
    if let Some(items) = m.pointer(&format!("/result/{wire}")).and_then(Value::as_array) {
        slot.get_or_insert_with(Vec::new).extend(items.iter().cloned());
        if m.pointer("/result/nextCursor").and_then(Value::as_str).is_none() {
            learn.done.insert(tag);
        }
    } else if m.get("error").is_some() {
        slot.get_or_insert_with(Vec::new);
        learn.done.insert(tag);
    }
}

/// Asks for the next page of a list when the server paginates.
async fn follow(b: &mut Backend, id: &str, m: &Value) -> Result<()> {
    let mut parts = id.split(':').skip(1);
    let (name, page) = (parts.next().unwrap_or(""), parts.next().and_then(|p| p.parse::<u32>().ok()).unwrap_or(0));
    let Some(cursor) = m.pointer("/result/nextCursor").and_then(Value::as_str) else { return Ok(()) };
    let method = match name {
        "tools" => "tools/list",
        "prompts" => "prompts/list",
        "resources" => "resources/list",
        "templates" => "resources/templates/list",
        _ => return Ok(()),
    };
    if page >= 50 {
        return Ok(());
    }
    let req = json!({ "jsonrpc": "2.0", "id": format!("shim:{name}:{}", page + 1), "method": method, "params": { "cursor": cursor } });
    b.tx.write_all(line(&req).as_bytes()).await?;
    b.tx.flush().await?;
    Ok(())
}

/// A manifest once every list the server advertises has arrived whole.
fn assemble(learn: &Learn, program: &str, args: &[String], version: &str) -> Option<Manifest> {
    let initialize = learn.initialize.clone()?;
    let caps = initialize.get("capabilities").cloned().unwrap_or(Value::Null);
    let tools = learn.tools.clone()?;
    let wants = |k: &str| caps.get(k).is_some();
    let whole = |k: &str| learn.done.contains(k);
    if !whole("tools") || (wants("prompts") && !whole("prompts")) || (wants("resources") && !(whole("resources") && whole("templates"))) {
        return None;
    }
    Some(Manifest {
        key: key(program, args, version),
        at: now(),
        version: version.to_string(),
        initialize,
        tools,
        prompts: learn.prompts.clone(),
        resources: learn.resources.clone(),
        templates: learn.templates.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{duplex, split};

    /// A server that answers like hanzo-mcp's shape: two tools, no prompts or resources.
    fn fake(started: Arc<AtomicUsize>) -> Spawn {
        Arc::new(move || {
            started.fetch_add(1, Ordering::SeqCst);
            let (theirs, ours) = duplex(1 << 16);
            let (r, w) = split(ours);
            tokio::spawn(async move {
                let mut lines = BufReader::new(r).lines();
                let mut w = w;
                while let Ok(Some(l)) = lines.next_line().await {
                    let m: Value = serde_json::from_str(&l).unwrap();
                    let Some(id) = m.get("id") else { continue };
                    let out = match m["method"].as_str().unwrap() {
                        "initialize" => result(id, json!({ "protocolVersion": m["params"]["protocolVersion"], "capabilities": { "tools": {} }, "serverInfo": { "name": "fake", "version": "1" } })),
                        "tools/list" => result(id, json!({ "tools": [{ "name": "read" }, { "name": "write" }] })),
                        "tools/call" => result(id, json!({ "content": [{ "type": "text", "text": format!("ran {}", m["params"]["name"].as_str().unwrap()) }] })),
                        _ => failure(id, "unknown"),
                    };
                    w.write_all(line(&out).as_bytes()).await.unwrap();
                }
            });
            let (rx, tx) = split(theirs);
            Ok(Link { tx: Box::new(tx), rx: Box::new(rx), guard: None })
        })
    }

    /// Drives the proxy with a script of agent lines and returns every line it wrote back, in order.
    async fn session(spawn: Spawn, dir: &Path, script: &[Value]) -> Vec<Value> {
        let (mut agent_w, proxy_r) = duplex(1 << 16);
        let (proxy_w, agent_r) = duplex(1 << 16);
        let dir = dir.to_path_buf();
        let h = tokio::spawn(async move { serve(proxy_r, proxy_w, spawn, "fake-server", &["--project-dir".into(), "/x".into()], Some(dir)).await });
        let mut out = Vec::new();
        let mut replies = BufReader::new(agent_r).lines();
        for m in script {
            agent_w.write_all(line(m).as_bytes()).await.unwrap();
            if m.get("id").is_some() {
                let l = tokio::time::timeout(Duration::from_secs(10), replies.next_line()).await.expect("a reply").unwrap().expect("a line");
                out.push(serde_json::from_str(&l).unwrap());
            }
        }
        // Give a background fill time to land before the agent leaves.
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(agent_w);
        h.await.unwrap().unwrap();
        out
    }

    fn init() -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } })
    }

    fn initialized() -> Value {
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    }

    fn list() -> Value {
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })
    }

    fn call() -> Value {
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "read", "arguments": {} } })
    }

    #[tokio::test]
    async fn a_first_session_starts_the_server_and_a_second_never_does_until_a_call() {
        let dir = tempfile::tempdir().unwrap();
        let started = Arc::new(AtomicUsize::new(0));
        let spawn = fake(started.clone());

        // Cold: the server starts at the handshake, the lists reach the agent, and the answer is kept.
        let out = session(spawn.clone(), dir.path(), &[init(), initialized(), list()]).await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(out[0]["result"]["serverInfo"]["name"], "fake");
        assert_eq!(out[1]["result"]["tools"].as_array().unwrap().len(), 2);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "one answer kept");

        // Warm: the handshake and the list come from the file and nothing starts.
        let out = session(spawn.clone(), dir.path(), &[init(), initialized(), list()]).await;
        assert_eq!(started.load(Ordering::SeqCst), 1, "a warm session starts no server");
        assert_eq!(out[0]["result"]["serverInfo"]["name"], "fake");
        assert_eq!(out[1]["result"]["tools"][1]["name"], "write");

        // A call starts it, and the answer is the server's own.
        let out = session(spawn, dir.path(), &[init(), initialized(), list(), call()]).await;
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(out[2]["id"], 3);
        assert_eq!(out[2]["result"]["content"][0]["text"], "ran read");
    }

    #[tokio::test]
    async fn another_protocol_version_is_not_served_an_old_answer() {
        let dir = tempfile::tempdir().unwrap();
        let started = Arc::new(AtomicUsize::new(0));
        let spawn = fake(started.clone());
        session(spawn.clone(), dir.path(), &[init(), initialized(), list()]).await;
        let mut other = init();
        other["params"]["protocolVersion"] = json!("2024-11-05");
        let out = session(spawn, dir.path(), &[other, initialized(), list()]).await;
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(out[0]["result"]["protocolVersion"], "2024-11-05");
    }

    #[tokio::test]
    async fn an_old_answer_is_asked_for_again() {
        let dir = tempfile::tempdir().unwrap();
        let started = Arc::new(AtomicUsize::new(0));
        let spawn = fake(started.clone());
        session(spawn.clone(), dir.path(), &[init(), initialized(), list()]).await;
        let file = std::fs::read_dir(dir.path()).unwrap().next().unwrap().unwrap().path();
        let mut m: Manifest = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        m.at = now() - TTL - 10;
        std::fs::write(&file, serde_json::to_vec(&m).unwrap()).unwrap();
        session(spawn, dir.path(), &[init(), initialized(), list()]).await;
        assert_eq!(started.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_key_ignores_the_project_directory_and_follows_the_server() {
        let a = key("/bin/sh", &["--project-dir".into(), "/a".into(), "--x".into()], "v");
        let b = key("/bin/sh", &["--project-dir".into(), "/b".into(), "--x".into()], "v");
        assert_eq!(a, b);
        assert_ne!(a, key("/bin/sh", &["--x".into(), "--y".into()], "v"));
        assert_ne!(a, key("/bin/sh", &["--project-dir".into(), "/a".into(), "--x".into()], "w"));
        assert_ne!(a, key("/bin/ls", &["--project-dir".into(), "/a".into(), "--x".into()], "v"));
    }

    #[test]
    fn a_kept_answer_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let m = Manifest { key: "k".into(), at: now(), version: "v".into(), initialize: json!({}), tools: vec![], prompts: None, resources: None, templates: None };
        let sub = dir.path().join("mcp");
        store(&sub, &m).unwrap();
        assert_eq!(std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(sub.join("k.json")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(load(&sub, "k"), Some(m));
    }
}
