//! Bounded event stream parsing, interactive transcript tailing, and process spawning.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncSeekExt};

use crate::backend::{Backend, Launch, Route};
use crate::event::{Kind, Mapped, Status, Usage};
use crate::render::render_event;
use crate::root::Root;
use crate::session::SessionClient;
use crate::{claude, control};

#[derive(Debug, Default, Clone)]
pub(crate) struct Outcome {
    pub backend_session: Option<String>,
    pub usage: Usage,
    pub tool_calls: u64,
    pub saw_error: bool,
    pub final_summary: Option<String>,
    /// Assistant messages emitted during the run, in order.
    pub messages: Vec<String>,
    /// Structured tool, spawn, and log actions captured from the stream.
    pub actions: Vec<crate::embedded::EmbeddedAction>,
    /// How the newest turn's process ended when not cleanly ("exited with status
    /// 101"); the reason a failed run reports.
    pub exit: Option<String>,
    /// Cleaned tail of stderr from a failed headless turn, used when the backend
    /// exited non-zero without emitting a structured error event.
    pub stderr_tail: Option<String>,
}

/// How a finished process ended, when not cleanly.
pub(crate) fn how(status: &std::process::ExitStatus) -> Option<String> {
    if status.success() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return Some(match sig {
                1 => "was hung up on (SIGHUP)".to_string(),
                2 => "was interrupted (SIGINT)".to_string(),
                15 => "was terminated (SIGTERM)".to_string(),
                9 => "was killed (SIGKILL), which is usually the system running out of memory".to_string(),
                n => format!("was killed by signal {n}"),
            });
        }
    }
    Some(match status.code() {
        Some(c) => format!("exited with status {c}"),
        None => "ended without an exit status".to_string(),
    })
}

/// Whether a person ended the process (Ctrl-C, a hangup, or the shell's 130/143):
/// that is not a failure.
fn interrupted(status: &std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return matches!(sig, 1 | 2 | 15);
        }
    }
    matches!(status.code(), Some(130 | 143))
}

/// The longest reason a session in error carries.
pub(crate) const REASON_MAX: usize = 600;

/// Why a run ended in error: the launch that failed, or how the process ended and
/// the error it reported last (structured stream error, stderr tail, or summary).
/// Never empty.
pub(crate) fn reason_of(run: &Result<(Outcome, Status)>, label: &str) -> String {
    match run {
        Err(e) => cut(&format!("{e:#}"), REASON_MAX),
        Ok((o, _)) => {
            let how = o.exit.as_deref().unwrap_or("ended with an error");
            let summary = o.final_summary.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let stderr = o.stderr_tail.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let detail = if o.saw_error { summary.or(stderr) } else { stderr.or(summary) };
            match detail {
                Some(said) => cut(&format!("{label} {how}: {said}"), REASON_MAX),
                None => format!("{label} {how}"),
            }
        }
    }
}

/// Maximum raw stderr bytes retained in the rolling tail per headless turn.
const STDERR_CAPTURE_MAX: usize = 4096;

/// Drain a child's stderr pipe, forwarding raw bytes to this process's stderr
/// when `forward` is true, and return a clean, bounded tail for error reporting.
pub(crate) fn spawn_stderr_tee(
    mut stderr: tokio::process::ChildStderr,
    forward: bool,
) -> tokio::task::JoinHandle<Option<String>> {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut out = forward.then(tokio::io::stderr);
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let slice = &chunk[..n];
                    if let Some(sink) = out.as_mut() {
                        let _ = sink.write_all(slice).await;
                    }
                    raw.extend_from_slice(slice);
                    if raw.len() > STDERR_CAPTURE_MAX {
                        let drop_n = raw.len() - STDERR_CAPTURE_MAX;
                        raw.drain(..drop_n);
                    }
                }
            }
        }
        format_stderr_tail(&raw)
    })
}

/// Strip ANSI escapes/control chars, redact inline credentials, and return the
/// trailing non-empty lines bounded to [`REASON_MAX`].
pub(crate) fn format_stderr_tail(raw: &[u8]) -> Option<String> {
    let text = strip_ansi(&String::from_utf8_lossy(raw));
    let lines: Vec<String> = text
        .lines()
        .map(|l| redact_tokens(l.trim()))
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let mut picked: Vec<&str> = Vec::new();
    let mut len = 0usize;
    for line in lines.iter().rev() {
        let sep = if picked.is_empty() { 0 } else { 3 };
        if !picked.is_empty() && len + sep + line.len() > REASON_MAX {
            break;
        }
        picked.push(line.as_str());
        len += sep + line.len();
    }
    picked.reverse();
    Some(cut(&picked.join(" | "), REASON_MAX))
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for nc in chars.by_ref() {
                        if ('@'..='~').contains(&nc) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(nc) = chars.next() {
                        if nc == '\x07' || (nc == '\x1b' && chars.peek() == Some(&'\\')) {
                            if nc == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if !c.is_control() || matches!(c, '\n' | '\t') {
            out.push(c);
        }
    }
    out
}

fn redact_tokens(line: &str) -> String {
    let mut prev_bearer = false;
    let mut out = Vec::new();
    for word in line.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        let trimmed = word.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']'));
        let secret = prev_bearer
            || trimmed.starts_with("hk-") && trimmed.len() >= 11
            || trimmed.starts_with("sk-") && trimmed.len() >= 11
            || trimmed.starts_with("hz-") && trimmed.len() >= 11
            || trimmed.starts_with("eyJ") && trimmed.matches('.').count() >= 2;
        if secret {
            out.push("<redacted>");
            prev_bearer = false;
        } else {
            prev_bearer = lower == "bearer" || lower.ends_with("bearer:");
            out.push(word);
        }
    }
    out.join(" ")
}


/// `s` cut to at most `n` bytes at a character boundary.
pub(crate) fn cut(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// The forward+render sink for a structured event stream. Forwarding fires ONLY
/// when a cloud client AND session id are present — the privacy gate is
/// structural: an unlinked run cannot reach the network from here.
struct Sink<'a> {
    client: Option<SessionClient>,
    session_id: Option<String>,
    render: bool,
    out: &'a mut Outcome,
}

impl Sink<'_> {
    async fn handle(&mut self, m: Mapped) {
        match m {
            Mapped::Event { kind, payload } => {
                if kind == Kind::ToolCall {
                    self.out.tool_calls = self.out.tool_calls.saturating_add(1);
                }
                if kind == Kind::Message && payload.get("role").and_then(serde_json::Value::as_str) == Some("assistant") {
                    if let Some(t) = payload
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        self.out.messages.push(t.to_string());
                        self.out.final_summary = Some(t.to_string());
                    }
                }
                if let Some(action) = crate::embedded::action_from_event(kind, &payload) {
                    self.out.actions.push(action);
                }
                if self.render {
                    render_event(kind, &payload);
                }
                if let (Some(c), Some(id)) = (&self.client, &self.session_id) {
                    if let Err(e) = c.event(id, kind, payload).await {
                        crate::warn(&format!("stream event dropped: {e}"));
                    }
                }
            }
            Mapped::BackendSession(id) => {
                self.out.backend_session.get_or_insert(id);
            }
            Mapped::Usage(u) => self.out.usage.merge(u),
            Mapped::Terminal { ok, summary } => {
                if !ok {
                    self.out.saw_error = true;
                    if summary.is_none() {
                        self.out.final_summary = None;
                    }
                }
                if summary.is_some() {
                    self.out.final_summary = summary;
                }
            }
        }
    }
}

/// The largest single pre-parse line we will buffer. Cloud caps an event payload
/// at 48 KiB (`event::PAYLOAD_BUDGET`), so any legitimate stream/transcript line
/// is far smaller; a line beyond this is garbage or hostile and is dropped rather
/// than accumulated, so a backend (or MCP output it relays) can never OOM the
/// wrapper with one unbounded, newline-free line.
const MAX_LINE: usize = 1024 * 1024;

/// The most transcript we ingest per poll while tailing, so a single large
/// append is spread across polls instead of being read into memory whole.
const MAX_TAIL_CHUNK: u64 = 8 * 1024 * 1024;

/// Read the next newline-delimited line from `reader`, bounded to `cap` bytes.
/// A line longer than `cap` is discarded — its bytes are still consumed through
/// the terminating newline so the stream stays aligned — and reading resumes at
/// the next line. `Ok(None)` signals EOF. Memory is bounded by `cap` (plus one
/// buffered chunk) regardless of adversarial input.
async fn next_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut overflow = false; // this line already passed `cap`; skip to newline
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            // EOF: yield a final unterminated line only if it fit within `cap`.
            return Ok((!buf.is_empty()).then(|| String::from_utf8_lossy(&buf).into_owned()));
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => {
                if !overflow {
                    buf.extend_from_slice(&chunk[..i]);
                }
                reader.consume(i + 1);
                if overflow {
                    overflow = false;
                    buf.clear(); // dropped the over-long line; start the next one
                    continue;
                }
                return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
            }
            None => {
                let n = chunk.len();
                if !overflow {
                    buf.extend_from_slice(chunk);
                    if buf.len() > cap {
                        overflow = true;
                        buf.clear(); // release memory; keep skipping to '\n'
                    }
                }
                reader.consume(n);
            }
        }
    }
}

/// Drive a backend's structured line stream into an existing [`Outcome`].
pub(crate) async fn run_stream_into<R: AsyncBufRead + Unpin>(
    backend: &dyn Backend,
    mut reader: R,
    client: Option<SessionClient>,
    session_id: Option<String>,
    render: bool,
    out: &mut Outcome,
) -> Result<()> {
    let mut sink = Sink { client, session_id, render, out };
    while let Some(line) = next_bounded_line(&mut reader, MAX_LINE).await.context("reading backend stream")? {
        for m in backend.parse(&line) {
            sink.handle(m).await;
        }
    }
    Ok(())
}

/// Drive a backend's structured line stream through parse → forward/render.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn run_stream<R: AsyncBufRead + Unpin>(
    backend: &dyn Backend,
    reader: R,
    client: Option<SessionClient>,
    session_id: Option<String>,
    render: bool,
) -> Result<Outcome> {
    let mut out = Outcome::default();
    run_stream_into(backend, reader, client, session_id, render, &mut out).await?;
    Ok(out)
}


#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_interactive(
    backend: &dyn Backend,
    launch: Launch,
    client: Option<SessionClient>,
    session_id: Option<String>,
    root: &Root,
    // The route this run launched with — it names the config home the transcript
    // is written in, so the tail must resolve against the same one.
    route: &Route,
    cwd: &Path,
    watch_sid: Option<String>,
) -> Result<bool> {
    let Launch { mut command, cleanup } = launch;
    command.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let mut relay = control::relay();
    let mut child = spawn(&mut command, backend.label())?;
    relay.to(child.id());

    // Linked interactive per-event streaming rides the backend transcript tail.
    let stop = Arc::new(AtomicBool::new(false));
    let tail = match (&client, &session_id, &watch_sid) {
        (Some(c), Some(id), Some(sid)) => backend.transcript_path(root, route, cwd, sid).map(|path| {
            tokio::spawn(tail_transcript(path, c.clone(), id.clone(), stop.clone()))
        }),
        _ => None,
    };

    let status = child.wait().await.context("waiting for backend")?;
    drop(relay);
    stop.store(true, Ordering::Relaxed);
    if let Some(h) = tail {
        let _ = h.await;
    }
    drop(cleanup);
    if status.success() || interrupted(&status) {
        return Ok(true);
    }
    Err(anyhow!(
        "{} {}",
        backend.label(),
        how(&status).unwrap_or_else(|| "exited with an error".to_string())
    ))
}

/// Follow a Claude transcript JSONL, forwarding newly-appended events to the
/// linked session. Best-effort: parse/forward failures are ignored so the live
/// TUI is never disturbed.
async fn tail_transcript(path: PathBuf, client: SessionClient, session_id: String, stop: Arc<AtomicBool>) {
    let mut pos: u64 = 0;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if let Ok(mut f) = tokio::fs::File::open(&path).await {
            let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
            if len > pos && f.seek(std::io::SeekFrom::Start(pos)).await.is_ok() {
                // Read a bounded slice per poll so a huge single append can't be
                // slurped whole; advance `pos` by what we actually read.
                let want = (len - pos).min(MAX_TAIL_CHUNK);
                let mut chunk: Vec<u8> = Vec::new();
                if (&mut f).take(want).read_to_end(&mut chunk).await.is_ok() {
                    pos += chunk.len() as u64;
                    buf.extend_from_slice(&chunk);
                    while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=nl).collect();
                        let text = String::from_utf8_lossy(&line[..line.len() - 1]);
                        for m in claude::Claude.parse(text.trim_end()) {
                            if let Mapped::Event { kind, payload } = m {
                                let _ = client.event(&session_id, kind, payload).await;
                            }
                        }
                    }
                    // Drop an over-long, newline-free line so a hostile or corrupt
                    // transcript can't grow the buffer without bound.
                    if buf.len() > MAX_LINE {
                        buf.clear();
                    }
                }
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}


/// Start a launch's program, which runs by its full path or not at all: a bare
/// name would be looked up through every PATH entry, a relative one included,
/// and a session starts in a repository it has not read ([`exe`]).
pub(crate) fn spawn(command: &mut tokio::process::Command, label: &str) -> Result<tokio::process::Child> {
    if !Path::new(command.as_std().get_program()).is_absolute() {
        return Err(spawn_err(label)(std::io::ErrorKind::NotFound.into()));
    }
    command.spawn().map_err(spawn_err(label))
}

/// A backend that is not installed is the ONE failure a user hits before any of
/// their work starts, so it names the program and how to pick another rather
/// than reporting a bare OS error.
fn spawn_err(program: &str) -> impl Fn(std::io::Error) -> anyhow::Error + '_ {
    move |e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!(
                "the `{program}` coding agent is not installed (not on PATH) — \
                 install it (`curl -fsSL hanzo.sh | sh` installs `dev`), or name \
                 another tool: `hanzo dev`, \
                 `hanzo dev --claude`, `--codex`, `--agy` or `--cursor`"
            )
        } else {
            anyhow!("failed to launch the `{program}` coding agent: {e}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::Claude;
    use crate::dev::Agent;
    use crate::testmock::MockCloud;
    use tokio::io::AsyncWriteExt;

    async fn reader_of(fixture: &str) -> impl AsyncBufRead + Unpin {
        let (r, mut w) = tokio::io::duplex(1 << 20);
        w.write_all(fixture.as_bytes()).await.unwrap();
        drop(w); // EOF
        tokio::io::BufReader::new(r)
    }


    /// The privacy property that link-by-default must NOT weaken: with no cloud
    /// client (the state an UNAUTHENTICATED run lands in — `run` sets the client
    /// to `None` when there is no bearer), the stream reaches cloud with nothing,
    /// even though a cloud endpoint is live. The gate is structural, not a flag.
    #[tokio::test]
    async fn no_auth_means_no_stream_even_with_cloud_live() {
        let mock = MockCloud::start().await;
        let fixture = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sid","model":"m"}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"private code"}]}}"#, "\n"
        );
        let reader = reader_of(fixture).await;
        // client == None models the unauthenticated run (no bearer -> no client).
        let out = run_stream(&Claude, reader, None, None, false).await.unwrap();
        assert_eq!(out.backend_session.as_deref(), Some("sid")); // parsed locally
        assert!(mock.requests().is_empty(), "no auth -> nothing reaches cloud");
    }

    #[tokio::test]
    async fn linked_stream_forwards_mapped_events_with_correct_kinds() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let fixture = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sid-1","model":"m"}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t","name":"Bash","input":{"command":"ls"}}]}}"#, "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":5,"output_tokens":2},"total_cost_usd":0.01,"result":"ok"}"#, "\n"
        );
        let reader = reader_of(fixture).await;
        let out = run_stream(&Claude, reader, Some(client), Some("sess_1".into()), false).await.unwrap();

        assert_eq!(out.backend_session.as_deref(), Some("sid-1"));
        assert_eq!(out.usage.input_tokens, Some(5));
        assert!(!out.saw_error);

        let kinds: Vec<String> = mock
            .requests()
            .iter()
            .filter(|r| r.path == "/v1/agents/sessions/sess_1/events")
            .map(|r| r.json()["kind"].as_str().unwrap_or("").to_string())
            .collect();
        // session-start log, assistant message, tool-call.
        assert!(kinds.contains(&"log".to_string()));
        assert!(kinds.contains(&"message".to_string()));
        assert!(kinds.contains(&"tool-call".to_string()));
    }

    #[tokio::test]
    async fn unlinked_stream_forwards_nothing_even_with_cloud_available() {
        // A cloud IS listening, but with no client the stream cannot reach it.
        let mock = MockCloud::start().await;
        let fixture = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sid","model":"m"}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"secret code here"}]}}"#, "\n"
        );
        let reader = reader_of(fixture).await;
        let out = run_stream(&Claude, reader, None, None, false).await.unwrap();
        assert_eq!(out.backend_session.as_deref(), Some("sid")); // parsed locally
        assert!(mock.requests().is_empty(), "unlinked run must not send anything to cloud");
    }

    #[tokio::test]
    async fn dev_stream_maps_and_forwards() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let fixture = concat!(
            r#"{"type":"thread.started","thread_id":"th-9"}"#, "\n",
            r#"{"type":"item.completed","item":{"id":"i","type":"command_execution","command":"go build","aggregated_output":"ok","exit_code":0,"status":"completed"}}"#, "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":1,"cached_input_tokens":0}}"#, "\n"
        );
        let reader = reader_of(fixture).await;
        let out = run_stream(&Agent::DEV, reader, Some(client), Some("sess_2".into()), false).await.unwrap();
        assert_eq!(out.backend_session.as_deref(), Some("th-9"));
        assert_eq!(out.usage.output_tokens, Some(1));
        let has_toolcall = mock
            .requests()
            .iter()
            .any(|r| r.path.ends_with("/events") && r.json()["kind"] == "tool-call");
        assert!(has_toolcall);
    }


    /// How a process ended is said in words, and a person's interrupt is told
    /// apart from a failure: Ctrl-C at an interactive session ends it, it does not
    /// break it.
    #[cfg(unix)]
    #[test]
    fn how_a_process_ended_is_said_and_an_interrupt_is_not_a_failure() {
        use std::os::unix::process::ExitStatusExt;
        // A wait status: the exit code in the second byte, or the signal in the low bits.
        let code = |c: i32| std::process::ExitStatus::from_raw(c << 8);
        let signal = |n: i32| std::process::ExitStatus::from_raw(n);

        assert_eq!(how(&code(0)), None);
        assert!(!interrupted(&code(0)));

        let failed = code(101);
        assert_eq!(how(&failed).as_deref(), Some("exited with status 101"));
        assert!(!interrupted(&failed), "a crash is not an interrupt");

        let ctrl_c = signal(2);
        assert_eq!(how(&ctrl_c).as_deref(), Some("was interrupted (SIGINT)"));
        assert!(interrupted(&ctrl_c));
        assert!(interrupted(&signal(15)) && interrupted(&signal(1)), "a terminate and a hangup are a person's too");
        assert!(interrupted(&code(130)) && interrupted(&code(143)), "the shell's spelling of them");

        let killed = signal(9);
        assert!(how(&killed).unwrap().contains("SIGKILL"), "{:?}", how(&killed));
        assert!(!interrupted(&killed), "a kill is not a person's interrupt");
        assert_eq!(how(&signal(11)).as_deref(), Some("was killed by signal 11"));
    }

    /// A headless run that ended in error reports why: the launch that failed, or
    /// how the process ended and the last error it reported. Never empty.
    #[test]
    fn a_failed_headless_run_reports_a_reason() {
        let launch_failed: Result<(Outcome, Status)> = Err(anyhow!("the `dev` coding agent is not installed (not on PATH)"));
        assert!(reason_of(&launch_failed, "dev").contains("not installed"));

        let crashed: Result<(Outcome, Status)> = Ok((Outcome { exit: Some("exited with status 101".into()), ..Default::default() }, Status::Error));
        assert_eq!(reason_of(&crashed, "dev"), "dev exited with status 101");

        let said: Result<(Outcome, Status)> = Ok((
            Outcome { exit: Some("exited with status 1".into()), final_summary: Some("stream disconnected before completion".into()), ..Default::default() },
            Status::Error,
        ));
        assert_eq!(reason_of(&said, "dev"), "dev exited with status 1: stream disconnected before completion");

        let nothing: Result<(Outcome, Status)> = Ok((Outcome::default(), Status::Error));
        assert_eq!(reason_of(&nothing, "dev"), "dev ended with an error");

        let long: Result<(Outcome, Status)> = Ok((Outcome { final_summary: Some("é".repeat(2000)), ..Default::default() }, Status::Error));
        assert!(reason_of(&long, "dev").len() <= REASON_MAX, "a reason is bounded, at a character boundary");

        // When a process crashes after emitting a normal assistant message (saw_error == false),
        // stderr carries the crash reason rather than the stale assistant message.
        let crashed_with_stderr: Result<(Outcome, Status)> = Ok((
            Outcome {
                exit: Some("exited with status 101".into()),
                final_summary: Some("Let me check the files.".into()),
                stderr_tail: Some("thread 'main' panicked at src/main.rs:9: boom".into()),
                saw_error: false,
                ..Default::default()
            },
            Status::Error,
        ));
        assert_eq!(
            reason_of(&crashed_with_stderr, "dev"),
            "dev exited with status 101: thread 'main' panicked at src/main.rs:9: boom"
        );

        // When the backend emitted a structured error event (saw_error == true), prefer it over noisy stderr.
        let structured_err: Result<(Outcome, Status)> = Ok((
            Outcome {
                exit: Some("exited with status 1".into()),
                final_summary: Some("401 Unauthorized".into()),
                stderr_tail: Some("WARN noisy diagnostic".into()),
                saw_error: true,
                ..Default::default()
            },
            Status::Error,
        ));
        assert_eq!(reason_of(&structured_err, "dev"), "dev exited with status 1: 401 Unauthorized");
    }

    #[test]
    fn format_stderr_tail_strips_ansi_bounds_length_and_redacts_secrets() {
        assert_eq!(format_stderr_tail(b"   \n\t\n"), None);
        let raw = b"\x1b[31merror:\x1b[0m auth failed with Bearer eyJhbGciOi.payload.sig and hk-1234567890abcdef\n";
        let got = format_stderr_tail(raw).unwrap();
        assert_eq!(got, "error: auth failed with Bearer <redacted> and <redacted>");
        assert!(!got.contains("eyJ") && !got.contains("hk-123"));

        let huge = (0..100).map(|i| format!("line {i} diagnostic output\n")).collect::<String>();
        let bounded = format_stderr_tail(huge.as_bytes()).unwrap();
        assert!(bounded.len() <= REASON_MAX);
        assert!(bounded.contains("line 99"), "should preserve trailing lines: {bounded}");
    }


    /// MEDIUM-1: one giant newline-free line must NOT be buffered whole (OOM);
    /// it is dropped and the stream recovers, still forwarding the next line.
    #[tokio::test]
    async fn oversize_line_is_dropped_and_stream_recovers() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();

        // 3 MiB with no newline (over MAX_LINE = 1 MiB), then a valid event.
        let mut fixture = "x".repeat(3 * 1024 * 1024);
        fixture.push('\n');
        fixture
            .push_str(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"after"}]}}"#);
        fixture.push('\n');

        // Feed from a spawned writer over a small duplex so the reader drains as
        // it goes (a blocking write_all of 3 MiB into a small buffer would hang).
        let (r, mut w) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = w.write_all(fixture.as_bytes()).await;
        });
        let reader = tokio::io::BufReader::new(r);

        let out = run_stream(&Claude, reader, Some(client), Some("sess_big".into()), false)
            .await
            .unwrap();
        assert!(!out.saw_error);
        // The valid line AFTER the oversize one was still parsed and forwarded.
        assert!(
            mock.requests().iter().any(|r| r.path == "/v1/agents/sessions/sess_big/events"
                && r.json()["kind"] == "message"
                && r.json()["payload"]["text"] == "after"),
            "stream must recover and forward the line after the oversize one"
        );
    }

    #[tokio::test]
    async fn tail_forwards_appended_transcript_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "").unwrap();
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(tail_transcript(path.clone(), client, "sess_t".into(), stop.clone()));

        tokio::time::sleep(Duration::from_millis(120)).await;
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(
                f,
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"live"}}]}}}}"#
            )
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(950)).await;
        stop.store(true, Ordering::Relaxed);
        let _ = handle.await;

        assert!(mock
            .requests()
            .iter()
            .any(|r| r.path == "/v1/agents/sessions/sess_t/events" && r.json()["kind"] == "message"));
    }
}
