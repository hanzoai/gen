//! The session channel's INBOUND half — cloud steers, this supervisor obeys.
//!
//! `session.rs` is the OUT direction (events up). This is IN. The two are one
//! channel with one identity and one transport; they are separate files because
//! they are separate directions, not separate protocols.
//!
//! # One vocabulary, spelled once
//!
//! The op set is NOT invented here. Cloud already owns a closed steering set —
//! `pause` / `resume` / `stop` / `message` (`apps/agents/sessions.go`'s `Cmd*`
//! constants) — reachable at `POST /v1/agents/sessions/:id/{pause,resume,stop,
//! message}` and mirrored in [`super::event::Kind::Control`]. Naming a second
//! spelling here (`interrupt`, `steer`) would be two words for one verb, so the
//! wire words ARE the type: [`Command`] is cloud's set and nothing else.
//!
//! Read them at `GET /v1/agents/sessions/:id/control?after=<seq>`, which returns
//! `{commands, cursor}` oldest-first. The cursor is why this is a drain and not a
//! poll of state: an applied command is never redelivered, so a reconnect after
//! a network gap replays exactly the commands issued while we were away, in
//! order, once. Nothing is buffered locally and nothing needs to be — the durable
//! log IS the buffer, and our cursor is our place in it.
//!
//! # What each command does to the child, and why
//!
//! A headless run is `claude -p … --output-format stream-json`: ONE turn, then
//! the process exits. So "the session" and "the process" are not the same thing —
//! the session is the transcript plus its resume handle, and it outlives any
//! single turn. That distinction is what makes `pause` and `stop` differ:
//!
//! | Command   | Signal  | Claude's observed behaviour           | Session after |
//! |-----------|---------|---------------------------------------|---------------|
//! | `pause`   | SIGINT  | aborts the in-flight tool, writes      | `paused` —    |
//! |           |         | `[Request interrupted by user for tool | resumable,    |
//! |           |         | use]` + a final `result` with          | same id       |
//! |           |         | `terminal_reason:"aborted_tools"`,     |               |
//! |           |         | flushes the transcript, exits 0        |               |
//! | `stop`    | SIGTERM | same abort path, exits 143             | `done`        |
//! | `message` | SIGINT  | as `pause`, then we relaunch with      | stays running |
//! |           |         | `--resume <sid>` + the new prompt      |               |
//! | `resume`  | none    | nothing — already running              | unchanged     |
//!
//! Both signals are POSIX process control delivered to the child's own pid — not
//! synthetic keystrokes, and not a byte written to anyone's terminal. SIGINT is
//! the one Claude handles gracefully (it is the same disposition `^C` triggers,
//! but arriving as a signal rather than as tty input), which is why `pause` and
//! `message` use it: the transcript is flushed and the resume handle survives.
//! SIGTERM is for `stop` because a stop need not be graceful and its non-zero
//! exit is a truthful "this run did not finish on its own".
//!
//! We do NOT signal the process group. The child is left in our group on purpose:
//! putting it in its own would make it a background group against an inherited
//! tty and earn it a SIGTTIN the first time it read stdin. Signalling the direct
//! pid is sufficient because Claude tears down its own tool subprocesses on
//! abort — verified: no `sleep` survived an interrupted `Bash` tool call.
//!
//! # `resume` is honestly a no-op here
//!
//! Cloud's set has four verbs; a locally-running headless supervisor can act on
//! three. `resume` means "start working again", and a session that is running has
//! nothing to restart. A session that was PAUSED has no process at all — it is a
//! transcript on disk — so resuming it is `hanzo dev --resume <id>` on the
//! machine that holds it, which already exists. Rather than keep a daemon alive
//! after `pause` purely to give `resume` something to do, we drain it, record it
//! and move on. Saying so is better than a fourth code path that pretends.

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use super::event::Status;
use super::session::SessionClient;

/// How often a live run drains its control queue. One GET per second per running
/// session: responsive enough that a dashboard click lands while the human still
/// has their hand on the mouse, cheap enough to leave running for hours.
const POLL: Duration = Duration::from_millis(1000);

/// The signal a command delivers. An enum rather than a raw `libc::c_int` so the
/// decision ([`Command::act`]) stays pure and testable on any platform, and only
/// [`send`] and [`hear`] touch libc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    /// Graceful abort — Claude flushes the transcript and exits 0.
    Int,
    /// Terminate — Claude still flushes, then exits 143.
    Term,
    /// The terminal went away.
    #[cfg(unix)]
    Hup,
}

/// Cloud's closed steering set, as the supervisor consumes it. `message` carries
/// the instruction to continue with; the rest carry nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Pause,
    Resume,
    Stop,
    Message(String),
}

/// What the supervisor does about a command. Separating this from [`Command`]
/// keeps the DECISION pure — a unit test asserts the whole policy table without
/// a process, a socket, or a signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Act {
    /// End the run: signal the child, then finalize the session with `status`.
    /// `status` comes from the COMMAND, never from the child's exit code — a
    /// commanded stop exits 143 and is still a clean `done`, not an `error`.
    End { signal: Signal, status: Status },
    /// Interrupt the current turn, then relaunch `--resume`d with a new prompt
    /// against the SAME cloud session and the SAME backend session.
    Steer { signal: Signal, prompt: String },
    /// Nothing to do — see the module note on `resume`.
    Ignore,
}

impl Act {
    /// The signal this act delivers, if any. `Ignore` delivers none — which is
    /// what makes it safe to drain a `resume` mid-run without disturbing it.
    pub(crate) fn signal(&self) -> Option<Signal> {
        match self {
            Act::End { signal, .. } | Act::Steer { signal, .. } => Some(*signal),
            Act::Ignore => None,
        }
    }

    /// True when this act ends the current TURN (as opposed to the session): both
    /// `End` and `Steer` interrupt the child, so both stop consuming further
    /// commands for this turn.
    pub(crate) fn ends_turn(&self) -> bool {
        !matches!(self, Act::Ignore)
    }
}

impl Command {
    /// Parse one drained command. `None` for a verb we do not know: cloud's set is
    /// closed, so an unknown word is a newer server talking to an older CLI, and
    /// ignoring it is the only safe reading — acting on a guess would be worse.
    pub(crate) fn from_wire(command: &str, message: &str, payload: &Value) -> Option<Command> {
        match command.trim() {
            "pause" => Some(Command::Pause),
            "resume" => Some(Command::Resume),
            "stop" => Some(Command::Stop),
            "message" => Some(Command::Message(steer_text(message, payload))),
            _ => None,
        }
    }

    /// The policy table from the module doc, as code. Total over the vocabulary.
    pub(crate) fn act(self) -> Act {
        match self {
            // Halt the turn, keep the session resumable under its own id.
            Command::Pause => Act::End { signal: Signal::Int, status: Status::Paused },
            // A commanded stop is a finished session, not a failed one.
            Command::Stop => Act::End { signal: Signal::Term, status: Status::Done },
            // Interrupt, then continue the SAME conversation with a new turn.
            Command::Message(prompt) => Act::Steer { signal: Signal::Int, prompt },
            Command::Resume => Act::Ignore,
        }
    }
}

/// The prompt a `message` command steers with. Cloud already guarantees at least
/// one of the two is non-empty, so this never yields "": the human-typed
/// `message` when there is one, else the payload verbatim — a third party that
/// steers with structured JSON still gets its instruction delivered rather than
/// silently dropped.
fn steer_text(message: &str, payload: &Value) -> String {
    let m = message.trim();
    if !m.is_empty() {
        return m.to_string();
    }
    match payload {
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One drained command, exactly as `drainControl` serialises it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Wire {
    pub seq: i64,
    pub command: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub payload: Value,
}

/// The cursor-carrying page `GET …/control?after=` returns.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Page {
    #[serde(default)]
    pub commands: Vec<Wire>,
    #[serde(default)]
    pub cursor: i64,
}

/// Drain commands for `session_id` into a channel until `stop` is set.
///
/// Best-effort by construction: a failed poll is a network hiccup, not a reason
/// to kill a developer's coding session, so it is swallowed and retried on the
/// next tick. The cursor only ever advances on a SUCCESSFUL page, so commands
/// issued during an outage are delivered — in order, once — when it clears.
///
/// The channel is the seam the tests drive: production fills it from this task,
/// a test fills it by hand. No trait, no mock object — a `Receiver<Command>` is
/// already the whole interface the supervisor needs.
pub(crate) fn drain(
    client: SessionClient,
    session_id: String,
    stop: Arc<AtomicBool>,
) -> mpsc::Receiver<Command> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut cursor: i64 = 0;
        while !stop.load(Ordering::Relaxed) {
            if let Ok(page) = client.drain_control(&session_id, cursor).await {
                cursor = page.cursor.max(cursor);
                for w in page.commands {
                    cursor = cursor.max(w.seq);
                    if let Some(cmd) = Command::from_wire(&w.command, &w.message, &w.payload) {
                        // A closed receiver means the run already ended; stop
                        // draining rather than spin against a dead channel.
                        if tx.send(cmd).await.is_err() {
                            return;
                        }
                    }
                }
            }
            tokio::time::sleep(POLL).await;
        }
    });
    rx
}

/// Make a child hear what [`send`] delivers. An ignored signal survives `exec`,
/// so a supervisor started with SIGINT ignored — POSIX gives every background
/// job of a non-interactive shell exactly that — would pass it on, and a
/// `pause` would land on a child that cannot hear it. Both signals go back to
/// their default in the child alone, between fork and exec.
#[cfg(unix)]
pub(crate) fn hear(command: &mut tokio::process::Command) {
    // SAFETY: signal(2) is async-signal-safe, which is all pre_exec asks.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn hear(_command: &mut tokio::process::Command) {}

/// Hold this process up while a child runs: a signal that would have ended it —
/// SIGTERM from a `kill` or a service manager, SIGHUP from a terminal that went
/// away, SIGINT — ends the CHILD instead, and this process ends when the child
/// does, the normal way: its session closed out, its run homes removed.
///
/// TERM and HUP are passed on, since a `kill` names this process alone. INT is
/// passed on too, except while this process is in its terminal's foreground
/// group: a Ctrl-C reaches that whole group, the child already heard it, and a
/// second would cut short the abort the first began. A signal this process was
/// started ignoring (`nohup`, a background job of a non-interactive shell) stays
/// ignored.
///
/// Make it before the child starts, so a signal that comes meanwhile waits for
/// the child rather than ending this process, then name the child with
/// [`Relay::to`]. Drop it once the child has been waited for: each signal goes
/// back to what this process started with, so one that comes while the session
/// closes out ends it at once.
pub(crate) fn relay() -> Relay {
    #[cfg(unix)]
    let held = disposition::listen();
    #[cfg(not(unix))]
    let held = tokio::signal::windows::ctrl_c().ok();
    Relay { held: Some(held), task: None }
}

/// The signals that would end this process, held for a child.
pub(crate) struct Relay {
    #[cfg(unix)]
    held: Option<[Option<tokio::signal::unix::Signal>; 3]>,
    #[cfg(not(unix))]
    held: Option<Option<tokio::signal::windows::CtrlC>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Relay {
    /// Pass what is held on to the child `pid`.
    #[cfg(unix)]
    pub(crate) fn to(&mut self, pid: Option<u32>) {
        let (Some(pid), Some([mut int, mut term, mut hup])) = (pid, self.held.take()) else { return };
        self.task = Some(tokio::spawn(async move {
            async fn next(s: &mut Option<tokio::signal::unix::Signal>) {
                match s {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending().await,
                }
            }
            loop {
                tokio::select! {
                    _ = next(&mut int) => if !disposition::foreground() { let _ = send(pid, Signal::Int); },
                    _ = next(&mut term) => { let _ = send(pid, Signal::Term); }
                    _ = next(&mut hup) => { let _ = send(pid, Signal::Hup); }
                }
            }
        }));
    }

    /// Windows delivers Ctrl-C to the whole console, the child with it; holding it
    /// here is what keeps this process up to close the session out. Only Ctrl-C is
    /// held: closing the console, logging off or shutting down still end it.
    #[cfg(not(unix))]
    pub(crate) fn to(&mut self, _pid: Option<u32>) {
        let Some(Some(mut c)) = self.held.take() else { return };
        self.task = Some(tokio::spawn(async move { while c.recv().await.is_some() {} }));
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(t) = &self.task {
            t.abort();
        }
        #[cfg(unix)]
        disposition::restore();
    }
}

/// The actions of the three relayed signals, which tokio cannot give back: once it
/// installs a handler it keeps it, and a signal with no listener is swallowed.
#[cfg(unix)]
mod disposition {
    use std::sync::Mutex;
    use tokio::signal::unix::{signal, Signal, SignalKind};

    const RELAYED: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    /// Per signal, the action this process started with and tokio's once it has
    /// one; and how many relays are listening.
    struct Actions {
        start: [Option<libc::sigaction>; 3],
        tokio: [Option<libc::sigaction>; 3],
        live: usize,
    }

    static ACTIONS: Mutex<Actions> = Mutex::new(Actions { start: [None; 3], tokio: [None; 3], live: 0 });

    fn get(sig: libc::c_int) -> libc::sigaction {
        // SAFETY: a null new action only reads the current one into `old`.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig, std::ptr::null(), &mut old);
            old
        }
    }

    fn set(sig: libc::c_int, act: &libc::sigaction) {
        // SAFETY: `act` is an action the kernel gave back for this signal.
        unsafe {
            libc::sigaction(sig, act, std::ptr::null_mut());
        }
    }

    /// A listener for each signal this process was not started ignoring, with
    /// tokio's handler back in place if [`restore`] took it out.
    pub(super) fn listen() -> [Option<Signal>; 3] {
        let mut a = ACTIONS.lock().unwrap_or_else(|e| e.into_inner());
        a.live += 1;
        std::array::from_fn(|i| {
            let sig = RELAYED[i];
            if a.start[i].get_or_insert_with(|| get(sig)).sa_sigaction == libc::SIG_IGN {
                return None;
            }
            if let Some(t) = &a.tokio[i] {
                set(sig, t);
            }
            let s = signal(SignalKind::from_raw(sig)).ok()?;
            a.tokio[i].get_or_insert_with(|| get(sig));
            Some(s)
        })
    }

    /// Once no relay is listening, put back the action each signal had when this
    /// process started.
    pub(super) fn restore() {
        let mut a = ACTIONS.lock().unwrap_or_else(|e| e.into_inner());
        a.live = a.live.saturating_sub(1);
        if a.live == 0 {
            for (sig, start) in RELAYED.iter().zip(a.start.iter()) {
                if let Some(start) = start {
                    set(*sig, start);
                }
            }
        }
    }

    /// Whether this process is in its terminal's foreground group, which a Ctrl-C
    /// reaches whole.
    pub(super) fn foreground() -> bool {
        // SAFETY: getpgrp cannot fail; tcgetpgrp only reads, and fails off a terminal.
        let own = unsafe { libc::getpgrp() };
        [0, 1, 2].into_iter().any(|fd| unsafe { libc::tcgetpgrp(fd) } == own)
    }
}

/// Deliver `sig` to `pid`. With [`hear`] and [`relay`], the only place this
/// module touches the OS.
#[cfg(unix)]
pub(crate) fn send(pid: u32, sig: Signal) -> Result<()> {
    let raw = match sig {
        Signal::Int => libc::SIGINT,
        Signal::Term => libc::SIGTERM,
        Signal::Hup => libc::SIGHUP,
    };
    if unsafe { libc::kill(pid as i32, raw) } != 0 {
        // ESRCH is the ordinary race: the child finished on its own between the
        // command arriving and us signalling it. The run is over either way.
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            anyhow::bail!("signalling pid {pid}: {e}");
        }
    }
    Ok(())
}

/// Windows has no SIGINT to deliver to another process, so a remote steer there
/// ends the run the only way the platform offers — see `Supervisor::signal`.
#[cfg(not(unix))]
pub(crate) fn send(_pid: u32, _sig: Signal) -> Result<()> {
    anyhow::bail!("signalling a child is not supported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The wire words ARE the vocabulary — this is the one place the CLI's
    /// spelling is pinned against cloud's `Cmd*` constants.
    #[test]
    fn wire_words_are_clouds_closed_set() {
        let n = &Value::Null;
        assert_eq!(Command::from_wire("pause", "", n), Some(Command::Pause));
        assert_eq!(Command::from_wire("resume", "", n), Some(Command::Resume));
        assert_eq!(Command::from_wire("stop", "", n), Some(Command::Stop));
        assert_eq!(
            Command::from_wire("message", "do the thing", n),
            Some(Command::Message("do the thing".into()))
        );
    }

    #[test]
    fn unknown_verb_is_ignored_not_guessed() {
        assert_eq!(Command::from_wire("interrupt", "", &Value::Null), None);
        assert_eq!(Command::from_wire("steer", "", &Value::Null), None);
        assert_eq!(Command::from_wire("", "", &Value::Null), None);
    }

    /// `pause` keeps the session alive; `stop` ends it. Both signal, and the
    /// status comes from the COMMAND — never from the child's exit code.
    #[test]
    fn policy_table_is_total_and_correct() {
        assert_eq!(
            Command::Pause.act(),
            Act::End { signal: Signal::Int, status: Status::Paused }
        );
        assert_eq!(
            Command::Stop.act(),
            Act::End { signal: Signal::Term, status: Status::Done }
        );
        assert_eq!(
            Command::Message("go on".into()).act(),
            Act::Steer { signal: Signal::Int, prompt: "go on".into() }
        );
        assert_eq!(Command::Resume.act(), Act::Ignore);
    }

    /// A commanded stop is a FINISHED session, not a failed one — the child's
    /// 143 must never surface as `error`.
    #[test]
    fn stop_finalizes_done_not_error() {
        let Act::End { status, .. } = Command::Stop.act() else {
            panic!("stop must end the run");
        };
        assert_eq!(status, Status::Done);
        assert_ne!(status, Status::Error);
    }

    /// A payload-only steer still delivers its instruction rather than steering
    /// with an empty prompt.
    #[test]
    fn payload_only_steer_is_not_dropped() {
        let cmd = Command::from_wire("message", "  ", &json!({"do": "this"})).unwrap();
        let Command::Message(text) = cmd else { panic!("expected message") };
        assert!(text.contains("\"do\""), "got: {text}");
        assert!(!text.trim().is_empty());
    }

    #[test]
    fn typed_message_wins_over_payload() {
        let cmd = Command::from_wire("message", "human words", &json!({"ignored": 1})).unwrap();
        assert_eq!(cmd, Command::Message("human words".into()));
    }

    #[test]
    fn page_parses_the_drain_shape() {
        let p: Page = serde_json::from_value(json!({
            "commands": [{"seq": 7, "command": "stop"}],
            "cursor": 7
        }))
        .unwrap();
        assert_eq!(p.cursor, 7);
        assert_eq!(p.commands[0].seq, 7);
        assert_eq!(p.commands[0].command, "stop");
    }
}
