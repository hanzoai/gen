//! What a signal sent to `gen` does, end to end: the built binary, a stand-in
//! agent that writes down what it heard, a stand-in `hanzo`, and a home of its
//! own. `gen` runs in a process group of its own, so no terminal's Ctrl-C is in
//! play and every signal here is one a `kill`, a `timeout` or an IDE sends.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A session's world: `bin/` with the stand-ins, `home/`, and the agent's log.
struct World {
    dir: tempfile::TempDir,
}

impl World {
    /// `profile` is what `hanzo config profile` answers.
    fn new(profile: &str) -> World {
        let w = World { dir: tempfile::tempdir().unwrap() };
        for d in ["bin", "home", "cwd"] {
            std::fs::create_dir(w.at(d)).unwrap();
        }
        let log = w.at("log");
        // The one turn the agent says, as pi's `--mode json` prints it: it is what
        // the run forwards to the cloud, and what a refused cloud retries.
        let say = w.at("say");
        std::fs::write(
            &say,
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}
"#,
        )
        .unwrap();
        w.program(
            "pi",
            &format!(
                r#"[ "$1" = --version ] && {{ echo "pi 0.0.1"; exit 0; }}
trap 'echo INT >> {log}; exit 130' INT
trap 'echo TERM >> {log}; exit 143' TERM
trap 'echo HUP >> {log}; exit 129' HUP
echo $$ > {log}.pid
echo start >> {log}
[ -n "$QUICK" ] && exit 0
cat {say}
i=0; while [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done
echo end >> {log}
"#,
                log = log.display(),
                say = say.display()
            ),
        );
        w.program(
            "hanzo",
            &format!(
                r#"case "$1 $2" in
 "config profile") printf '%s' '{profile}' ;;
 "auth token") printf '%s\n' 'eyJhbGciOiJub25lIn0.eyJvd25lciI6ICJ0ZXN0IiwgIm5hbWUiOiAib25lIn0.x' ;;
 *) echo "Error: not here" >&2; exit 2 ;;
esac
"#
            ),
        );
        w
    }

    fn at(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn program(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = self.at("bin").join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `gen` on `args`, with `ignored` ignored from the start (`nohup`'s shape).
    fn gen(&self, args: &[&str], ignored: Option<libc::c_int>, quick: bool) -> Child {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gen"));
        cmd.args(args)
            .current_dir(self.at("cwd"))
            .env_clear()
            .env("HOME", self.at("home"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.at("bin").display()))
            .env("TERM", "dumb")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if let Some(t) = std::env::var_os("TMPDIR") {
            cmd.env("TMPDIR", t);
        }
        if quick {
            cmd.env("QUICK", "1");
        }
        // SAFETY: signal(2) is async-signal-safe, which is all pre_exec asks.
        unsafe {
            cmd.pre_exec(move || {
                // Start from the defaults: a CI runner starts its jobs with
                // SIGINT ignored, and gen rightly keeps an ignore it inherits.
                for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                if let Some(sig) = ignored {
                    libc::signal(sig, libc::SIG_IGN);
                }
                Ok(())
            });
        }
        let gen = cmd.spawn().unwrap();
        self.until("start");
        gen
    }

    fn heard(&self) -> String {
        std::fs::read_to_string(self.at("log")).unwrap_or_default()
    }

    fn until(&self, word: &str) {
        let by = Instant::now() + Duration::from_secs(20);
        while !self.heard().lines().any(|l| l == word) {
            assert!(Instant::now() < by, "the agent never wrote {word:?}: {:?}", self.heard());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn kill(child: &Child, sig: libc::c_int) {
    // SAFETY: kill(2) on a child this test started and has not reaped.
    assert_eq!(unsafe { libc::kill(child.id() as i32, sig) }, 0);
}

/// How `child` ended, within `patience`; `None` if it is still running.
fn ended(child: &mut Child, patience: Duration) -> Option<ExitStatus> {
    let by = Instant::now() + patience;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return Some(s);
        }
        if Instant::now() > by {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

const UNLINKED: &str = r#"{"api": "http://127.0.0.1:9", "local": true, "org": null, "provider": "hanzo", "identities": [], "link": false, "theme": "auto"}"#;
const TASK: [&str; 5] = ["--backend", "pi", "--no-link", "--no-mcp", "x"];

/// INT, TERM and HUP sent to `gen` alone each reach the agent, which ends the
/// run then and there, and a run its agent did not finish is a failed one.
#[test]
fn a_signal_to_gen_ends_its_agent() {
    for (sig, word) in [(libc::SIGINT, "INT"), (libc::SIGTERM, "TERM"), (libc::SIGHUP, "HUP")] {
        let w = World::new(UNLINKED);
        let mut gen = w.gen(&TASK, None, false);
        kill(&gen, sig);
        let status = ended(&mut gen, Duration::from_secs(10)).unwrap_or_else(|| panic!("gen outlived SIG{word}"));
        let heard = w.heard();
        assert!(heard.lines().any(|l| l == word), "the agent never heard SIG{word}: {heard:?}");
        assert!(!heard.contains("end"), "the agent ran to its end through SIG{word}: {heard:?}");
        assert!(!status.success(), "a run cut short by SIG{word} said it succeeded");
    }
}

/// A signal `gen` was started ignoring (`nohup`) stays ignored, for it and for
/// the agent, which carries on.
#[test]
fn a_signal_gen_was_started_ignoring_stays_ignored() {
    let w = World::new(UNLINKED);
    let mut gen = w.gen(&TASK, Some(libc::SIGHUP), false);
    kill(&gen, libc::SIGHUP);
    assert!(ended(&mut gen, Duration::from_millis(600)).is_none(), "an ignored SIGHUP ended gen");
    kill(&gen, libc::SIGTERM);
    ended(&mut gen, Duration::from_secs(10)).expect("gen outlived SIGTERM");
    let heard = w.heard();
    assert!(!heard.contains("HUP"), "the agent heard a SIGHUP gen was told to ignore: {heard:?}");
    assert!(heard.contains("TERM"), "{heard:?}");
}

/// Once the agent has ended, a signal ends `gen` at once, even while it waits on
/// a cloud that does not answer: nothing is left for it to hold up for.
#[test]
fn a_signal_once_the_agent_has_ended_ends_gen() {
    let cloud = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = cloud.local_addr().unwrap().port();
    let profile = format!(
        r#"{{"api": "http://127.0.0.1:{port}", "local": true, "org": null, "provider": "hanzo", "identities": [["test", "one"]], "link": true, "theme": "auto"}}"#
    );
    let w = World::new(&profile);
    let log = w.at("log");
    let (stalled, stall) = mpsc::channel();
    let state = Arc::new(Cloud { stall_when_reaped: true, ..Cloud::default() });
    std::thread::spawn(move || {
        for conn in cloud.incoming().flatten() {
            let (log, stalled, state) = (log.clone(), stalled.clone(), state.clone());
            std::thread::spawn(move || answer(conn, &log, &stalled, &state));
        }
    });
    let mut gen = w.gen(&["--backend", "pi", "--link", "--no-mcp", "x"], None, true);
    stall.recv_timeout(Duration::from_secs(20)).expect("gen never closed the session out");
    std::thread::sleep(Duration::from_millis(200));
    let sent = Instant::now();
    kill(&gen, libc::SIGTERM);
    let status = ended(&mut gen, Duration::from_secs(5)).expect("gen held on through SIGTERM after its agent ended");
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
    assert!(sent.elapsed() < Duration::from_secs(3), "{:?}", sent.elapsed());
}

/// A transcript turn refused mid-run is still ONE request when SIGINT ends the
/// run: every recorded `/events` body with the same payload carries the SAME
/// `requestId`, so the platform stores the turn once. A fresh name on each try
/// would store the turn twice — exactly the duplicate a burst is named to prevent
/// — and the agent heard INT rather than reaching its own end.
#[test]
fn a_signal_inside_a_retry_window_keeps_a_turn_named_once() {
    let stand = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = stand.local_addr().unwrap().port();
    let profile = format!(
        r#"{{"api": "http://127.0.0.1:{port}", "local": true, "org": null, "provider": "hanzo", "identities": [["test", "one"]], "link": true, "theme": "auto"}}"#
    );
    let w = World::new(&profile);
    let log = w.at("log");
    // Serve the run's opening status, then refuse every turn: the agent's first
    // message is inside the wire's retry window when the signal arrives.
    let state = Arc::new(Cloud { refuse_after: Some(1), ..Cloud::default() });
    let (stalled, _stall) = mpsc::channel();
    {
        let (log, stalled, state) = (log.clone(), stalled.clone(), state.clone());
        std::thread::spawn(move || {
            for conn in stand.incoming().flatten() {
                let (log, stalled, state) = (log.clone(), stalled.clone(), state.clone());
                std::thread::spawn(move || answer(conn, &log, &stalled, &state));
            }
        });
    }
    let mut gen = w.gen(&["--backend", "pi", "--link", "--no-mcp", "x"], None, false);
    // The opening status is one event; the refused turn's first try is the next.
    let by = Instant::now() + Duration::from_secs(20);
    while state.events.lock().unwrap().len() < 2 {
        assert!(Instant::now() < by, "the turn never reached the cloud: {:?}", w.heard());
        assert!(gen.try_wait().unwrap().is_none(), "gen ended before its turn was refused");
        std::thread::sleep(Duration::from_millis(20));
    }
    kill(&gen, libc::SIGINT);
    let status = ended(&mut gen, Duration::from_secs(20)).expect("gen outlived SIGINT");
    let heard = w.heard();
    assert!(!status.success(), "a run cut short by SIGINT said it succeeded");
    assert!(heard.lines().any(|l| l == "INT"), "the agent never heard SIGINT: {heard:?}");
    assert!(!heard.contains("end"), "the agent ran to its end through SIGINT: {heard:?}");

    // Group the recorded bodies by what they say; the id must be the group's,
    // not the attempt's.
    let bodies = state.events.lock().unwrap().clone();
    let mut groups: Vec<(serde_json::Value, Vec<String>)> = Vec::new();
    for b in &bodies {
        let key = serde_json::json!({ "kind": b["kind"], "payload": b["payload"] });
        let id = b["requestId"].as_str().unwrap_or_default().to_string();
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, ids)) => ids.push(id),
            None => groups.push((key, vec![id])),
        }
    }
    let retried = groups.iter().find(|(_, ids)| ids.len() > 1).unwrap_or_else(|| {
        panic!("no turn was tried twice before the signal: {bodies:?}")
    });
    assert!(!retried.1[0].is_empty(), "the retried turn was sent unnamed: {bodies:?}");
    for (key, ids) in &groups {
        assert!(
            ids.iter().all(|id| *id == ids[0]),
            "{key} was sent under two request ids {ids:?}"
        );
    }
}

/// What the stand-in cloud remembers across connections: every `/events` body it
/// was sent, and how many it has refused.
#[derive(Default)]
struct Cloud {
    events: Mutex<Vec<serde_json::Value>>,
    refused: AtomicUsize,
    /// Serve this many event POSTs, then answer 503 to the rest. `None` serves
    /// every one — a cloud with no burst in it.
    refuse_after: Option<usize>,
    /// Go silent once the agent that wrote `log` has been reaped: a cloud that
    /// stopped answering mid-close-out.
    stall_when_reaped: bool,
}

/// One connection to the stand-in cloud: every request is answered until gen has
/// reaped its agent, and from then on none is, as a cloud that has stalled.
fn answer(conn: std::net::TcpStream, log: &Path, stalled: &mpsc::Sender<()>, cloud: &Cloud) {
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
            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
            .unwrap_or(0);
        let mut body = vec![0; len];
        r.read_exact(&mut body).unwrap();
        let first = head.lines().next().unwrap_or_default();
        if cloud.stall_when_reaped && reaped(log) {
            let _ = stalled.send(());
            std::thread::sleep(Duration::from_secs(60));
            return;
        }
        let is_event = first
            .strip_prefix("POST /v1/agents/sessions/")
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|p| p.ends_with("/events"));
        let refused = if is_event {
            cloud.events.lock().unwrap().push(serde_json::from_slice(&body).unwrap_or_default());
            let n = cloud.refused.fetch_add(1, Ordering::SeqCst);
            cloud.refuse_after.is_some_and(|after| n >= after)
        } else {
            false
        };
        let (status, reply) = if refused {
            ("503 Service Unavailable", r#"{"error":"not ready"}"#)
        } else if first.starts_with("POST /v1/agents/sessions ") {
            ("200 OK", r#"{"id":"sess_t"}"#)
        } else if first.contains("/control") {
            ("200 OK", r#"{"commands":[],"cursor":0}"#)
        } else {
            ("200 OK", "{}")
        };
        let _ = write!(out, "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{reply}", reply.len());
    }
}

/// Whether the agent that wrote its pid beside `log` is gone, reaped and all.
fn reaped(log: &Path) -> bool {
    let pid = std::fs::read_to_string(log.with_extension("pid")).ok().and_then(|p| p.trim().parse::<i32>().ok());
    // SAFETY: signal 0 delivers nothing; it asks whether the pid exists.
    pid.is_some_and(|p| unsafe { libc::kill(p, 0) } != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
}
