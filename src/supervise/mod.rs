//! Headless turn execution, remote steering/stop control, and supervisor loop.

use anyhow::{Context, Result};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::backend::{Backend, Launch, Spec};
use crate::control::{self, Act, Command};
use crate::event::Status;
use crate::session::SessionClient;
use crate::stream::{cut, how, run_stream, spawn, Outcome};

/// What a harness's loop guard says when its model repeats a tool call. It ends the
/// turn with an error; the run is not over, the model is.
const LOOP_MARK: &str = "repeated identical tool-use cycle detected";

/// The longest call a change-of-course message repeats.
const CALL_MAX: usize = 300;

/// How many times a run is nudged on before its end stands, whatever stopped it.
const MAX_NUDGES: u8 = 2;

/// What a task run is told when its turn ended announcing a next step, or asking a
/// question nobody is there to answer.
const KEEP_GOING: &str = "Your turn ended without finishing the task, and nobody is here to answer. \
Carry on with your tools now, and end your turn only with the finished result.";

/// Whether `said` ends a turn on an announcement or an offer instead of a result: a last
/// line that ends in a colon, or starts "let me", "now I'll", "would you like" and the like.
fn narrating(said: &str) -> bool {
    let s = said.trim();
    if s.ends_with(':') {
        return true;
    }
    let last = s.lines().last().unwrap_or("").trim_start_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    [
        "let me ", "let's ", "now let me", "now let's", "now i'll ", "now i will ", "next i'll ", "next i will ",
        "next, i'll ", "next, i will ", "i'll now ", "i will now ", "would you like", "do you want me to", "shall i ",
        "should i ",
    ]
    .iter()
    .any(|p| last.starts_with(p))
}

/// What a model stopped by the loop guard is told: the call it repeated and to change it.
fn change_course(said: &str) -> String {
    let call = said.split_once("duplicate tool calls:").map(|(_, c)| c.trim()).unwrap_or("");
    let mut told = String::from("You made the same tool call twice in a row and got the same result, so your turn was stopped.");
    if !call.is_empty() {
        told.push_str(" The call was: ");
        told.push_str(&cut(call, CALL_MAX));
    }
    told.push_str("\n\nDo not make it again. Use a different command or different arguments, and carry on with the task.");
    told
}


/// Run ONE headless turn, watching for a steering command while it streams.
///
/// The turn ends either because the backend finished (stdout hits EOF) or because
/// a command arrived and we signalled the child. In BOTH cases we keep reading to
/// EOF before returning: an interrupted Claude still writes `[Request interrupted
/// by user for tool use]` and a final `result`, and those are the most useful
/// events in the whole run — dropping them to exit a millisecond sooner would
/// throw away the record of what the interrupt actually stopped.
///
/// Only the FIRST actionable command is applied per turn (`acted.is_none()`), so
/// a double-click on the dashboard cannot signal a child twice.
pub(crate) async fn run_turn(
    backend: &dyn Backend,
    structured: bool,
    launch: Launch,
    client: Option<SessionClient>,
    session_id: Option<String>,
    control: Option<&mut tokio::sync::mpsc::Receiver<Command>>,
) -> Result<(Outcome, bool, Option<Act>)> {
    let Launch { mut command, cleanup } = launch;
    // A run given up on (its time limit, its folder revoked) takes its agent with it.
    command.stdin(Stdio::inherit()).stderr(Stdio::inherit()).kill_on_drop(true);
    if structured {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::inherit());
    }
    if control.is_some() {
        control::hear(&mut command);
    }
    let mut relay = control::relay();
    let mut child = spawn(&mut command, backend.label())?;
    let pid = child.id();
    relay.to(pid);
    let mut acted: Option<Act> = None;

    let mut outcome = match (structured, control) {
        (false, _) => Outcome::default(),
        (true, control) => {
            let stdout = child.stdout.take().expect("piped stdout");
            let reader = tokio::io::BufReader::new(stdout);
            let streaming = run_stream(backend, reader, client, session_id, true);
            tokio::pin!(streaming);
            match control {
                // Unlinked: nothing can steer this run, so there is nothing to
                // select on. The privacy gate is structural here too.
                None => streaming.await?,
                Some(rx) => loop {
                    tokio::select! {
                        out = &mut streaming => break out?,
                        Some(cmd) = rx.recv(), if acted.is_none() => {
                            let act = cmd.act();
                            if let Some(sig) = act.signal() {
                                // A signal we cannot deliver (Windows has no
                                // SIGINT for another process) must still stop the
                                // run — fall back to the platform's own kill
                                // rather than let a remote stop silently do
                                // nothing.
                                if let Some(p) = pid {
                                    if control::send(p, sig).is_err() {
                                        let _ = child.start_kill();
                                    }
                                }
                            }
                            if act.ends_turn() {
                                acted = Some(act);
                            }
                        }
                    }
                },
            }
        }
    };

    let status = child.wait().await.context("waiting for backend")?;
    drop(relay);
    drop(cleanup);
    outcome.exit = how(&status);
    Ok((outcome, status.success(), acted))
}

/// Drive a headless run to completion under remote control, returning the
/// accumulated outcome and the status to finalize the cloud session with.
///
/// A `message` command makes this loop: the child is interrupted, the spec is
/// rebuilt with the backend's OWN resume handle plus the new instruction, and the
/// next turn continues the SAME conversation — native `--resume`, never injected
/// keystrokes. The cloud session id never changes across a steer, so the
/// dashboard watches one unbroken session while the turns underneath it restart.
pub(crate) async fn supervise(
    backend: &dyn Backend,
    mut spec: Spec,
    mut launch: Launch,
    structured: bool,
    client: Option<SessionClient>,
    session_id: Option<String>,
) -> Result<(Outcome, Status)> {
    // The drain runs only for a linked run — same structural auth gate as the
    // event stream. An unlinked run holds no client, so nothing can steer it.
    let stop = Arc::new(AtomicBool::new(false));
    let mut rx = match (&client, &session_id) {
        (Some(c), Some(id)) => Some(control::drain(c.clone(), id.clone(), stop.clone())),
        _ => None,
    };

    let mut acc = Outcome::default();
    let mut nudged = 0u8;
    let status = loop {
        let (out, ok, act) =
            run_turn(backend, structured, launch, client.clone(), session_id.clone(), rx.as_mut())
                .await?;
        let said = out.final_summary.clone();

        // Fold this turn into the run. The backend session id is set once — it is
        // the SAME conversation across every steer — while usage accumulates and
        // the newest summary wins.
        acc.backend_session = acc.backend_session.or(out.backend_session);
        acc.usage.merge(out.usage);
        acc.tool_calls = acc.tool_calls.saturating_add(out.tool_calls);
        acc.saw_error = out.saw_error;
        acc.exit = out.exit;
        if out.final_summary.is_some() {
            acc.final_summary = out.final_summary;
        }

        match act {
            // Commanded end: the COMMAND decides the status, not the exit code —
            // a stop exits 143 and is still a clean `done`, never an `error`.
            Some(Act::End { status, .. }) => break status,
            Some(Act::Steer { prompt, .. }) => {
                // Resume the conversation the run has BUILT UP — unless there
                // isn't one yet. A turn that never disclosed a session id was
                // interrupted before the backend finished starting, so there is
                // no transcript to preserve and a fresh launch loses nothing;
                // relaunching either way is what makes a steer land whenever the
                // human clicks it, rather than only after start-up completes.
                spec.resume = acc.backend_session.clone();
                spec.task = Some(prompt);
                // `--resume` and `--session-id` are mutually exclusive, so a
                // resumed turn drops the pre-set id.
                if spec.resume.is_some() {
                    spec.preset_session = None;
                }
                launch = backend.build(&spec)?;
            }
            // Uncommanded end: the backend finished (or failed) on its own.
            //
            // A turn the loop guard stopped, or one that ended announcing a next step,
            // is nudged in the same conversation, up to MAX_NUDGES times; only then
            // does its end stand.
            None | Some(Act::Ignore) => {
                let nudge = match said.as_deref() {
                    Some(m) if !ok && m.contains(LOOP_MARK) => Some(change_course(m)),
                    Some(m) if ok && narrating(m) => Some(KEEP_GOING.to_string()),
                    _ => None,
                };
                if let Some(told) = nudge.filter(|_| nudged < MAX_NUDGES && acc.backend_session.is_some()) {
                    nudged += 1;
                    crate::warn(&format!("{} stopped short; nudging it on ({nudged}/{MAX_NUDGES})", backend.label()));
                    spec.resume = acc.backend_session.clone();
                    spec.task = Some(told);
                    spec.preset_session = None;
                    launch = backend.build(&spec)?;
                    continue;
                }
                break Status::of(ok);
            }
        }
    };

    stop.store(true, Ordering::Relaxed);
    Ok((acc, status))
}


#[cfg(test)]
mod tests;
