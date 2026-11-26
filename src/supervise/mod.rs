//! Headless turn execution, remote steering/stop control, and long-horizon supervisor loop.

use anyhow::{Context, Result};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::backend::{Backend, Launch, Spec};
use crate::control::{self, Act, Command};
use crate::event::Status;
use crate::session::SessionClient;
use crate::stream::{cut, how, run_stream_into, spawn, Outcome};

/// Rebuild a launch for another turn without losing the read-only recipe: a
/// steer or a continuation must not quietly become a writable run.
fn rebuild(backend: &dyn Backend, spec: &Spec, recipe: Option<crate::runner::readonly::Recipe>) -> Result<Launch> {
    let mut next = backend.build(spec)?;
    next.readonly = recipe;
    Ok(next)
}

/// What a harness's loop guard says when its model repeats a tool call. It ends the
/// turn with an error; the run is not over, the model is.
const LOOP_MARK: &str = "repeated identical tool-use cycle detected";

/// Context-window and output-token exhaustion markers that trigger automatic
/// compact continuation across multi-hour runs instead of aborting.
const CONTEXT_OVERFLOW_MARKERS: &[&str] = &[
    "context_length_exceeded",
    "maximum context length",
    "prompt is too long",
    "context window",
    "max_output_tokens",
    "token limit",
];

/// The longest call or error excerpt a course-correction message repeats.
const CALL_MAX: usize = 300;

/// How many times a run is nudged on before its end stands.
pub(crate) const MAX_NUDGES: u8 = 4;

/// Maximum automatic context-overflow / token-limit continuations across a multi-hour session.
pub(crate) const MAX_CONTINUATIONS: u8 = 6;

/// Compact handoff prompt injected when a turn hits context or output token limits.
const CONTINUE_COMPACT: &str = "The previous turn hit a context or output token limit before completing the task. \
Summarize your current git and workspace progress concisely (3-5 bullets), inspect `git status` / `git diff --stat` \
if needed without reading huge files whole, and immediately continue executing and verifying the remaining steps \
until the task is complete.";

/// Execution options for [`supervise_opts`].
pub(crate) struct SuperviseOpts {
    pub render: bool,
    pub null_stdio: bool,
    pub cancel: Option<tokio::sync::watch::Receiver<bool>>,
}

/// Terminal disposition returned by [`supervise_opts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SuperviseEnd {
    Status(Status),
    Interrupted,
}

/// Progressive escalation prompts when a headless turn stops on narration or an offer.
fn keep_going(attempt: u8) -> String {
    match attempt {
        1 => "Your turn ended without finishing the task, and nobody is here to answer. \
Carry on with your tools now, and end your turn only with the finished result."
            .to_string(),
        2 => "You stopped to announce a next step again, and nobody is here to answer. \
Stop narrating plans — invoke your tools right now to execute and verify the work, \
ending only when everything is complete."
            .to_string(),
        3 => format!(
            "Escalation (nudge 3/{MAX_NUDGES}): nobody is here to reply. Do not end your turn with \
             a colon, status update, or question. Run every remaining edit, build, and test tool call \
             now, and conclude only with the final verified result."
        ),
        _ => format!(
            "Final autonomous escalation (nudge {attempt}/{MAX_NUDGES}): nobody is here to answer. \
             Complete all remaining tool executions and verification immediately in this turn, then \
             output only the final summary."
        ),
    }
}

/// Whether `said` reports context-window or output-token exhaustion.
pub(crate) fn context_overflow(said: &str) -> bool {
    let lower = said.to_ascii_lowercase();
    CONTEXT_OVERFLOW_MARKERS.iter().any(|m| lower.contains(m))
}

/// Whether `said` ends a turn on an announcement or an offer instead of a result: a last
/// line that ends in a colon, or starts "let me", "now I'll", "would you like" and the like.
fn narrating(said: &str) -> bool {
    let s = said.trim();
    if s.ends_with(':') {
        return true;
    }
    let last = s
        .lines()
        .last()
        .unwrap_or("")
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    [
        "let me ",
        "let's ",
        "now let me",
        "now let's",
        "now i'll ",
        "now i will ",
        "next i'll ",
        "next i will ",
        "next, i'll ",
        "next, i will ",
        "i'll now ",
        "i will now ",
        "would you like",
        "do you want me to",
        "shall i ",
        "should i ",
    ]
    .iter()
    .any(|p| last.starts_with(p))
}

/// What a model stopped by the loop guard is told: the call it repeated and to change it.
fn change_course(said: &str) -> String {
    let call = said
        .split_once("duplicate tool calls:")
        .map(|(_, c)| c.trim())
        .unwrap_or("");
    let mut told = String::from(
        "You made the same tool call twice in a row and got the same result, so your turn was stopped.",
    );
    if !call.is_empty() {
        told.push_str(" The call was: ");
        told.push_str(&cut(call, CALL_MAX));
    }
    told.push_str(
        "\n\nDo not make it again. Use a different command or different arguments, and carry on with the task.",
    );
    told
}

/// Recovery prompt when a first nudged turn fails with an error summary.
fn recover_nudged_failure(err: &str) -> String {
    format!(
        "Your previous resumed turn failed with: {}\n\n\
         Inspect why it failed, fix the root cause with your tools, and carry on to finish the task.",
        cut(err.trim(), CALL_MAX)
    )
}

/// Targeted course-correction prompt when two consecutive nudged turns fail with the
/// exact same error summary (oscillation guard).
fn change_course_on_repeated_failure(err: &str) -> String {
    format!(
        "Course correction — you must change course: two consecutive turns failed with the exact same error:\n  {}\n\n\
         Do NOT repeat the failing command or approach. Diagnose the root cause by inspecting the relevant \
         files/state, switch to a different strategy, and complete the task.",
        cut(err.trim(), CALL_MAX)
    )
}

async fn wait_cancel(rx: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    let Some(rx) = rx.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    if *rx.borrow() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

async fn recv_control(rx: &mut Option<&mut tokio::sync::mpsc::Receiver<Command>>) -> Option<Command> {
    match rx.as_mut() {
        Some(ch) => ch.recv().await,
        None => std::future::pending().await,
    }
}

async fn wait_grace(grace: &mut Option<Pin<Box<tokio::time::Sleep>>>) {
    match grace.as_mut() {
        Some(s) => s.await,
        None => std::future::pending().await,
    }
}

/// Run ONE headless turn, watching for a steering command while it streams.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn run_turn(
    backend: &dyn Backend,
    structured: bool,
    launch: Launch,
    client: Option<SessionClient>,
    session_id: Option<String>,
    control: Option<&mut tokio::sync::mpsc::Receiver<Command>>,
) -> Result<(Outcome, bool, Option<Act>)> {
    let (out, ok, act, _) = run_turn_opts(
        backend,
        structured,
        launch,
        client,
        session_id,
        control,
        true,
        false,
        &mut None,
    )
    .await?;
    Ok((out, ok, act))
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_opts(
    backend: &dyn Backend,
    structured: bool,
    launch: Launch,
    client: Option<SessionClient>,
    session_id: Option<String>,
    mut control: Option<&mut tokio::sync::mpsc::Receiver<Command>>,
    render: bool,
    null_stdio: bool,
    cancel: &mut Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<(Outcome, bool, Option<Act>, bool)> {
    if cancel.as_ref().is_some_and(|rx| *rx.borrow()) {
        return Ok((Outcome::default(), false, None, true));
    }

    // A read-only routed run gets a real OS-enforced workspace guard on EVERY
    // turn (including steers and continuations), never just the first one.
    let mut launch = launch;
    if let Some(recipe) = launch.readonly.clone() {
        recipe.guard(&mut launch)?;
    }
    let Launch { mut command, cleanup, .. } = launch;
    if null_stdio {
        command.stdin(Stdio::null());
    } else {
        command.stdin(Stdio::inherit());
    }
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    if structured {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::inherit());
    }
    if control.is_some() || cancel.is_some() {
        control::hear(&mut command);
    }
    let mut relay = (!null_stdio).then(control::relay);
    let mut child = spawn(&mut command, backend.label())?;
    let stderr_task = child
        .stderr
        .take()
        .map(|err| crate::stream::spawn_stderr_tee(err, !null_stdio));
    let pid = child.id();
    if let Some(r) = &mut relay {
        r.to(pid);
    }

    let mut acted: Option<Act> = None;
    let mut interrupted = false;
    let mut outcome = Outcome::default();

    if structured {
        let stdout = child.stdout.take().expect("piped stdout");
        let reader = tokio::io::BufReader::new(stdout);
        let mut grace: Option<Pin<Box<tokio::time::Sleep>>> = None;
        {
            let streaming = run_stream_into(backend, reader, client, session_id, render, &mut outcome);
            tokio::pin!(streaming);
            loop {
                tokio::select! {
                    res = &mut streaming => {
                        res?;
                        break;
                    }
                    () = wait_cancel(cancel), if !interrupted && acted.is_none() => {
                        interrupted = true;
                        if let Some(p) = pid {
                            if control::send(p, control::Signal::Int).is_err() {
                                let _ = child.start_kill();
                            }
                        } else {
                            let _ = child.start_kill();
                        }
                        grace = Some(Box::pin(tokio::time::sleep(Duration::from_millis(250))));
                    }
                    () = wait_grace(&mut grace), if grace.is_some() => {
                        let _ = child.start_kill();
                        if let Ok(res) = tokio::time::timeout(Duration::from_millis(100), &mut streaming).await {
                            res?;
                        }
                        break;
                    }
                    Some(cmd) = recv_control(&mut control), if acted.is_none() && !interrupted => {
                        let act = cmd.act();
                        if let Some(sig) = act.signal() {
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
            }
        }
    }

    let status = child.wait().await.context("waiting for backend")?;
    if let Some(handle) = stderr_task {
        let abort = handle.abort_handle();
        if let Ok(Ok(tail)) = tokio::time::timeout(Duration::from_millis(250), handle).await {
            if !status.success() {
                outcome.stderr_tail = tail;
            }
        } else {
            abort.abort();
        }
    }
    drop(relay);
    drop(cleanup);
    outcome.exit = how(&status);
    Ok((outcome, status.success(), acted, interrupted))
}

/// Drive a headless run to completion under remote control, returning the
/// accumulated outcome and the status to finalize the cloud session with.
pub(crate) async fn supervise(
    backend: &dyn Backend,
    spec: Spec,
    launch: Launch,
    structured: bool,
    client: Option<SessionClient>,
    session_id: Option<String>,
) -> Result<(Outcome, Status)> {
    let (out, end) = supervise_opts(
        backend,
        spec,
        launch,
        structured,
        client,
        session_id,
        SuperviseOpts { render: true, null_stdio: false, cancel: None },
    )
    .await?;
    let status = match end {
        SuperviseEnd::Status(s) => s,
        SuperviseEnd::Interrupted => Status::Paused,
    };
    Ok((out, status))
}

/// Drive a headless run with configurable rendering, stdio confinement, and
/// caller watch-channel cancellation (used by both CLI `run` and [`crate::embedded`]).
pub(crate) async fn supervise_opts(
    backend: &dyn Backend,
    mut spec: Spec,
    mut launch: Launch,
    structured: bool,
    client: Option<SessionClient>,
    session_id: Option<String>,
    mut opts: SuperviseOpts,
) -> Result<(Outcome, SuperviseEnd)> {
    let stop = Arc::new(AtomicBool::new(false));
    let mut rx = match (&client, &session_id) {
        (Some(c), Some(id)) => Some(control::drain(c.clone(), id.clone(), stop.clone())),
        _ => None,
    };

    let mut acc = Outcome::default();
    let mut nudged = 0u8;
    let mut continuations = 0u8;
    let mut was_nudged = false;
    let mut last_nudged_error: Option<String> = None;
    let mut readonly_gate = spec
        .task
        .as_deref()
        .is_some_and(crate::readonly::requests_readonly_or_confirmation);

    let recipe = launch.readonly.clone();
    let end = loop {
        let (out, ok, act, interrupted) = run_turn_opts(
            backend,
            structured,
            launch,
            client.clone(),
            session_id.clone(),
            rx.as_mut(),
            opts.render,
            opts.null_stdio,
            &mut opts.cancel,
        )
        .await?;
        let said = out.final_summary.clone();

        acc.backend_session = acc.backend_session.or(out.backend_session);
        acc.usage.merge(out.usage);
        acc.tool_calls = acc.tool_calls.saturating_add(out.tool_calls);
        acc.saw_error = out.saw_error;
        acc.exit = out.exit;
        acc.stderr_tail = out.stderr_tail;
        acc.messages.extend(out.messages);
        acc.actions.extend(out.actions);
        if out.final_summary.is_some() {
            acc.final_summary = out.final_summary;
        }

        if interrupted {
            break SuperviseEnd::Interrupted;
        }

        match act {
            Some(Act::End { status, .. }) => break SuperviseEnd::Status(status),
            Some(Act::Steer { prompt, .. }) => {
                was_nudged = false;
                last_nudged_error = None;
                if crate::readonly::requests_readonly_or_confirmation(&prompt) {
                    readonly_gate = true;
                } else if readonly_gate && crate::readonly::confirms_proceeding(&prompt) {
                    readonly_gate = false;
                }
                spec.resume = acc.backend_session.clone();
                spec.task = Some(prompt);
                if spec.resume.is_some() {
                    spec.preset_session = None;
                }
                launch = rebuild(backend, &spec, recipe.clone())?;
            }
            None | Some(Act::Ignore) => {
                if said.as_deref().is_some_and(context_overflow) {
                    if continuations < MAX_CONTINUATIONS && acc.backend_session.is_some() {
                        continuations += 1;
                        crate::warn(&format!(
                            "{} hit context/token limit; auto-continuing ({continuations}/{MAX_CONTINUATIONS})",
                            backend.label()
                        ));
                        spec.resume = acc.backend_session.clone();
                        spec.task = Some(CONTINUE_COMPACT.to_string());
                        spec.preset_session = None;
                        launch = rebuild(backend, &spec, recipe.clone())?;
                        continue;
                    }
                    break SuperviseEnd::Status(Status::of(ok));
                }

                let err_trimmed = (!ok)
                    .then(|| said.as_deref().map(str::trim).filter(|s| !s.is_empty()))
                    .flatten();
                let repeated_nudged_error =
                    was_nudged && err_trimmed.is_some() && last_nudged_error.as_deref() == err_trimmed;

                let nudge = if repeated_nudged_error {
                    Some(change_course_on_repeated_failure(err_trimmed.unwrap()))
                } else if let Some(m) = err_trimmed.filter(|m| m.contains(LOOP_MARK)) {
                    Some(change_course(m))
                } else if let Some(err) = err_trimmed.filter(|_| was_nudged) {
                    Some(recover_nudged_failure(err))
                } else if ok && !readonly_gate && said.as_deref().is_some_and(narrating) {
                    Some(keep_going(nudged + 1))
                } else {
                    None
                };

                if let Some(told) = nudge.filter(|_| nudged < MAX_NUDGES && acc.backend_session.is_some()) {
                    nudged += 1;
                    last_nudged_error = err_trimmed.map(String::from);
                    was_nudged = true;
                    crate::warn(&format!(
                        "{} stopped short; nudging it on ({nudged}/{MAX_NUDGES})",
                        backend.label()
                    ));
                    spec.resume = acc.backend_session.clone();
                    spec.task = Some(told);
                    spec.preset_session = None;
                    launch = rebuild(backend, &spec, recipe.clone())?;
                    continue;
                }
                break SuperviseEnd::Status(Status::of(ok));
            }
        }
    };

    stop.store(true, Ordering::Relaxed);
    Ok((acc, end))
}

#[cfg(test)]
mod tests;
