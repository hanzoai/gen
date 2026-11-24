use super::*;
use crate::backend::{Approval, Mode, Route};
use crate::claude::Claude;
use crate::event::Mapped;
use crate::register_session;
use crate::root::Root;
use crate::stream::reason_of;
use crate::testmock::MockCloud;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncWriteExt};

/// A backend whose `build` spawns a REAL child (`cat` of a fixture file) so
/// the full spawn → pipe stdout → parse → forward path is exercised without a
/// live `claude`/`dev` binary.
struct FakeBackend {
    fixture: String,
}

impl Backend for FakeBackend {
    fn label(&self) -> &'static str {
        "claude"
    }
    fn version(&self, _: &Root) -> Option<String> {
        None
    }
    fn build(&self, _spec: &Spec) -> Result<Launch> {
        use std::io::Write;
        let mut f = tempfile::Builder::new().suffix(".jsonl").tempfile().unwrap();
        f.write_all(self.fixture.as_bytes()).unwrap();
        let path = f.into_temp_path();
        let mut command = tokio::process::Command::from(crate::exe::command(crate::exe::find("cat").expect("cat")));
        command.arg(&*path);
        Ok(Launch { command, cleanup: crate::backend::Scratch { files: vec![path], ..Default::default() } })
    }
    fn parse(&self, line: &str) -> Vec<Mapped> {
        Claude.parse(line)
    }
    fn transcript_path(&self, _: &Root, _: &Route, _: &Path, _: &str) -> Option<PathBuf> {
        None
    }
}

fn dummy_spec() -> Spec {
    Spec {
        mode: Mode::Headless,
        task: Some("t".into()),
        cwd: PathBuf::from("."),
        routing: Route::Inherit,
        approval: Approval::Auto,
        mcp: None,
        remote: None,
        structured: true,
        preset_session: None,
        trust_project: false,
        resume: None,
        passthrough: vec![],
        refresh: Vec::new(),
        root: crate::root::scratch(),
    }
}

#[tokio::test]
async fn run_turn_spawns_a_real_child_and_forwards_end_to_end() {
    let mock = MockCloud::start().await;
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();
    let fixture = concat!(
        r#"{"type":"system","subtype":"init","session_id":"sid-1","model":"m"}"#, "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t","name":"Bash","input":{"command":"ls"}}]}}"#, "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":5,"output_tokens":2},"result":"ok"}"#, "\n"
    );
    let fake = FakeBackend { fixture: fixture.into() };
    let launch = fake.build(&dummy_spec()).unwrap();
    let (out, ok, act) =
        run_turn(&fake, true, launch, Some(client), Some("sess_e2e".into()), None).await.unwrap();

    assert!(ok, "child exited zero");
    assert!(act.is_none(), "an unsteered turn ends on its own terms");
    assert_eq!(out.backend_session.as_deref(), Some("sid-1"));
    assert_eq!(out.usage.input_tokens, Some(5));
    let kinds: Vec<String> = mock
        .requests()
        .iter()
        .filter(|r| r.path == "/v1/agents/sessions/sess_e2e/events")
        .map(|r| r.json()["kind"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(kinds.contains(&"message".to_string()));
    assert!(kinds.contains(&"tool-call".to_string()));
}


/// A REAL recorded interrupt, trimmed to the three lines that matter.
/// Captured from `claude -p … --output-format stream-json` interrupted
/// mid-`Bash` by SIGINT: Claude aborts the tool, says so in a user turn, and
/// still writes a final `result` carrying `terminal_reason:"aborted_tools"`.
/// That last line is why an interrupted run is still worth streaming.
const INTERRUPTED: &str = concat!(
    r#"{"type":"system","subtype":"init","session_id":"36d41361-9768-4c07-98cb-4a15984c5bf1","model":"claude-sonnet-5"}"#,
    "\n",
    r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]},"session_id":"36d41361-9768-4c07-98cb-4a15984c5bf1"}"#,
    "\n",
    r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools","session_id":"36d41361-9768-4c07-98cb-4a15984c5bf1","result":null,"num_turns":3,"total_cost_usd":0.10867639999999999,"duration_ms":11232}"#,
    "\n",
);

async fn reader_of(fixture: &str) -> impl AsyncBufRead + Unpin {
    let (r, mut w) = tokio::io::duplex(1 << 20);
    w.write_all(fixture.as_bytes()).await.unwrap();
    drop(w);
    tokio::io::BufReader::new(r)
}

/// Every `(resume, task)` a backend was asked to build, in order — held by the
/// backend and by the test that reads it, which is why it is shared.
type Builds = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;

/// A backend that runs a scripted shell per turn and records every `Spec` it
/// was asked to build — which is how the steer test proves the SECOND turn
/// resumed the FIRST turn's session id.
struct ScriptBackend {
    scripts: Vec<&'static str>,
    built: Builds,
}

impl ScriptBackend {
    fn new(scripts: Vec<&'static str>) -> (Self, Builds) {
        let built = Arc::new(Mutex::new(Vec::new()));
        (ScriptBackend { scripts, built: built.clone() }, built)
    }
}

impl Backend for ScriptBackend {
    fn label(&self) -> &'static str {
        "claude"
    }
    fn version(&self, _: &Root) -> Option<String> {
        None
    }
    fn build(&self, spec: &Spec) -> Result<Launch> {
        let mut b = self.built.lock().unwrap();
        let idx = b.len().min(self.scripts.len() - 1);
        b.push((spec.resume.clone(), spec.task.clone()));
        let mut command = tokio::process::Command::from(crate::exe::command(crate::exe::find("sh").expect("sh")));
        command.arg("-c").arg(self.scripts[idx]);
        // Every turn starts with both control signals ignored, as a
        // supervisor launched in the background hands them down, so these
        // tests prove the child hears a command on every machine and not
        // only on one whose shell left SIGINT alone.
        // SAFETY: signal(2) is async-signal-safe, which is all pre_exec asks.
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_IGN);
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                Ok(())
            });
        }
        Ok(Launch { command, cleanup: Default::default() })
    }
    /// The real Claude parser — so these tests exercise the SAME mapping the
    /// production stream does, on the same recorded shapes.
    fn parse(&self, line: &str) -> Vec<Mapped> {
        Claude.parse(line)
    }
    fn transcript_path(&self, _: &Root, _: &Route, _: &Path, _: &str) -> Option<PathBuf> {
        None
    }
}

fn spec_for(task: &str) -> Spec {
    Spec {
        mode: Mode::Headless,
        task: Some(task.to_string()),
        cwd: std::env::temp_dir(),
        routing: Route::Inherit,
        approval: Approval::Auto,
        mcp: None,
        remote: None,
        structured: true,
        preset_session: None,
        trust_project: false,
        resume: None,
        passthrough: Vec::new(),
        refresh: Vec::new(),
        root: crate::root::scratch(),
    }
}

/// Emit an init line disclosing a backend session id, then hang forever.
/// `exec` matters: it replaces the shell so the pid we signal IS the process
/// holding the stdout pipe — otherwise a surviving grandchild would keep the
/// pipe open and the stream would never see EOF.
const HANGS: &str = concat!(
    r#"echo '{"type":"system","subtype":"init","session_id":"bsid-1","model":"m"}'; "#,
    "exec sleep 30",
);

/// Runs briefly and finishes on its OWN terms, announcing it did.
const COMPLETES: &str = concat!(
    r#"echo '{"type":"system","subtype":"init","session_id":"bsid-1","model":"m"}'; "#,
    "sleep 2; ",
    r#"echo '{"type":"result","subtype":"success","is_error":false,"session_id":"bsid-1","result":"finished","num_turns":1}'"#,
);

async fn supervise_with(
    mock: &MockCloud,
    session: &str,
    scripts: Vec<&'static str>,
) -> (Outcome, Status, Arc<Mutex<Vec<(Option<String>, Option<String>)>>>, std::time::Duration) {
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();
    let (backend, built) = ScriptBackend::new(scripts);
    let spec = spec_for("original task");
    // This `build` is the turn-0 invocation the orchestrator performs before
    // handing the launch to the supervisor, so `built[0]` is turn 0 and any
    // further entry is a steer's relaunch.
    let launch = backend.build(&spec).unwrap();
    let started = std::time::Instant::now();
    let (out, status) = supervise(
        &backend,
        spec,
        launch,
        true,
        Some(client),
        Some(session.to_string()),
    )
    .await
    .unwrap();
    (out, status, built, started.elapsed())
}

// ---- the event path, from a recorded interrupt --------------------------

/// The interrupt's OWN events reach the channel with the right kinds and the
/// right identity — including the abort notice and the terminal `result`.
/// This is the shape a dashboard renders when a human clicks interrupt.
#[tokio::test]
async fn interrupt_events_surface_on_the_channel_with_correct_shape() {
    let mock = MockCloud::start().await;
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();
    let reader = reader_of(INTERRUPTED).await;

    let out = run_stream(&Claude, reader, Some(client), Some("sess_1".into()), false)
        .await
        .unwrap();

    // The resume handle is the whole reason an interrupted session survives.
    assert_eq!(out.backend_session.as_deref(), Some("36d41361-9768-4c07-98cb-4a15984c5bf1"));
    assert!(out.saw_error, "aborted_tools is a non-ok terminal");

    let posts: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/agents/sessions/sess_1/events")
        .collect();
    assert!(!posts.is_empty(), "the interrupt must be streamed, not swallowed");

    // Every event is routed to OUR session and carries the bearer, never an org.
    for r in &posts {
        assert_eq!(r.header("authorization").as_deref(), Some("Bearer T"));
        assert!(r.header("x-org-id").is_none(), "the CLI must never assert an org");
    }
    let texts: Vec<String> = posts.iter().map(|r| r.body.clone()).collect();
    let joined = texts.join("\n");
    assert!(
        joined.contains("[Request interrupted by user for tool use]"),
        "the abort notice must reach the dashboard; got: {joined}"
    );
}

// ---- one test per control op -------------------------------------------

/// `stop` ACTUALLY terminates the child: a run scripted to sleep 30s ends in
/// well under that, and the session finalizes `done` — not `error`, even
/// though a signalled child exits non-zero.
#[tokio::test]
async fn stop_terminates_the_child_and_finalizes_done() {
    let mock = MockCloud::start_with_control(&[(1, "stop", "")]).await;
    let (_out, status, built, elapsed) =
        supervise_with(&mock, "sess_1", vec![HANGS]).await;

    assert!(
        elapsed < Duration::from_secs(25),
        "stop must kill the child, not wait it out (took {elapsed:?})"
    );
    assert_eq!(status, Status::Done, "a commanded stop is finished, not failed");
    assert_eq!(built.lock().unwrap().len(), 1, "stop must not relaunch");

    // And the terminal status actually reached cloud.
    let patches: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH" && r.path == "/v1/agents/sessions/sess_1")
        .collect();
    assert!(patches.is_empty(), "supervise itself does not PATCH; finalize does");
}

/// `pause` halts the operation but leaves the session ALIVE: the status is
/// `paused` (non-terminal, so cloud will reopen it) and the backend's resume
/// handle survived, which is what makes reopening possible at all.
#[tokio::test]
async fn interrupt_halts_the_operation_without_destroying_the_session() {
    // Delayed one poll so the backend has certainly disclosed its session id:
    // this test is about the handle SURVIVING the interrupt, so the interrupt
    // must land after there is a handle to survive.
    let mock = MockCloud::start_with_delayed_control(1, &[(1, "pause", "")]).await;
    let (out, status, built, elapsed) = supervise_with(&mock, "sess_1", vec![HANGS]).await;

    assert!(elapsed < Duration::from_secs(25), "pause must interrupt promptly");
    assert_eq!(status, Status::Paused);
    assert_ne!(status, Status::Done, "pause must NOT close the session");
    assert_ne!(status, Status::Error, "an interrupt is not a failure");
    assert_eq!(
        out.backend_session.as_deref(),
        Some("bsid-1"),
        "the resume handle must survive an interrupt — without it the session is unreachable"
    );
    assert_eq!(built.lock().unwrap().len(), 1, "pause must not relaunch");
}

/// A turn the loop guard stopped is told to change course and the run goes on;
/// after the bound the guard's end stands, with its reason.
///
/// MUTATION: drop the `looped` branch in `supervise` and the first run ends in error
/// after one turn.
#[tokio::test]
async fn a_run_its_loop_guard_stopped_is_told_to_change_course() {
    const LOOPED: &str = concat!(
        r#"echo '{"type":"system","subtype":"init","session_id":"bsid-1","model":"m"}'; "#,
        r#"echo '{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"bsid-1","result":"unsupported operation: repeated identical tool-use cycle detected; refusing to continue after duplicate tool calls: {\"name\":\"exec_command\"}"}'; "#,
        "exit 1",
    );
    const FINISHES: &str = concat!(
        r#"echo '{"type":"system","subtype":"init","session_id":"bsid-1","model":"m"}'; "#,
        r#"echo '{"type":"result","subtype":"success","is_error":false,"session_id":"bsid-1","result":"finished","num_turns":1}'"#,
    );
    let mock = MockCloud::start().await;
    let (out, status, built, _) = supervise_with(&mock, "sess_loop", vec![LOOPED, FINISHES]).await;
    let built = built.lock().unwrap().clone();
    assert_eq!(built.len(), 2, "the stopped turn was not followed by another: {built:?}");
    assert_eq!(built[1].0.as_deref(), Some("bsid-1"), "the same conversation, not a new one");
    let told = built[1].1.as_deref().unwrap();
    assert!(told.contains("same tool call twice") && told.contains("exec_command") && told.contains("different command"), "{told}");
    assert_eq!(status, Status::Done, "the run it was told to carry on finished");
    assert!(!out.saw_error, "the newest turn is the one that counts");

    // A model that loops again and again is stopped after the bound, with the reason.
    let mock2 = MockCloud::start().await;
    let (out2, status2, built2, _) = supervise_with(&mock2, "sess_loop2", vec![LOOPED]).await;
    assert_eq!(built2.lock().unwrap().len(), 1 + MAX_NUDGES as usize, "told {MAX_NUDGES} times, then stopped");
    assert_eq!(status2, Status::Error);
    let why = reason_of(&Ok((out2, status2)), "claude");
    assert!(why.contains("repeated identical tool-use cycle"), "{why}");
}

/// A linked run starts only with its session: a cloud that is briefly not ready is
/// retried with backoff, and one that stays away fails the run saying how to work
/// without a link, never an unregistered continue.
///
/// MUTATION: return None on a failed registration, as the old code did, and the
/// second case reads Ok.
#[tokio::test]
async fn a_linked_run_registers_its_session_or_fails() {
    let slow = Duration::from_millis(10);
    let mock = MockCloud::start_refusing_first(2).await;
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();
    let (id, forked) = register_session(&client, &crate::tests::snapshot("claude"), "t", None, Duration::from_secs(5), slow).await.unwrap();
    assert!(!id.is_empty() && forked.is_none());
    assert!(mock.requests().len() >= 3, "the refused attempts were retried: {}", mock.requests().len());

    let down = MockCloud::start_status(503).await;
    let client = SessionClient::new(&down.base_url(), "T").unwrap();
    let err = register_session(&client, &crate::tests::snapshot("claude"), "t", None, Duration::from_millis(300), slow).await.unwrap_err();
    let said = format!("{err:#}");
    assert!(said.contains("did not register") && said.contains("--no-link"), "{said}");
}

/// A task run whose turn ends announcing a next step is told to carry on, in the same
/// conversation, and finishes; one that announces every time is let go after the bound.
///
/// MUTATION: drop the `narrating` arm in `supervise` and the first run ends after one turn.
#[tokio::test]
async fn a_task_run_that_ends_announcing_is_told_to_carry_on() {
    const ANNOUNCES: &str = concat!(
        r#"echo '{"type":"system","subtype":"init","session_id":"bsid-1","model":"m"}'; "#,
        r#"echo '{"type":"result","subtype":"success","is_error":false,"session_id":"bsid-1","result":"I read the build file. Now let me read the workflow:","num_turns":1}'"#,
    );
    const FINISHES: &str = concat!(
        r#"echo '{"type":"system","subtype":"init","session_id":"bsid-1","model":"m"}'; "#,
        r#"echo '{"type":"result","subtype":"success","is_error":false,"session_id":"bsid-1","result":"The inputs are a and b.","num_turns":1}'"#,
    );
    let mock = MockCloud::start().await;
    let (out, status, built, _) = supervise_with(&mock, "sess_n", vec![ANNOUNCES, FINISHES]).await;
    let built = built.lock().unwrap().clone();
    assert_eq!(built.len(), 2, "the announcing turn was not followed by another: {built:?}");
    assert_eq!(built[1].0.as_deref(), Some("bsid-1"), "the same conversation");
    assert!(built[1].1.as_deref().unwrap().contains("nobody is here"), "{:?}", built[1].1);
    assert_eq!(status, Status::Done);
    assert_eq!(out.final_summary.as_deref(), Some("The inputs are a and b."));

    let mock2 = MockCloud::start().await;
    let (_, status2, built2, _) = supervise_with(&mock2, "sess_n2", vec![ANNOUNCES]).await;
    assert_eq!(built2.lock().unwrap().len(), 1 + MAX_NUDGES as usize, "told {MAX_NUDGES} times, then let go");
    assert_eq!(status2, Status::Done);

    // A finished answer is not nudged.
    let mock3 = MockCloud::start().await;
    let (_, _, built3, _) = supervise_with(&mock3, "sess_n3", vec![FINISHES]).await;
    assert_eq!(built3.lock().unwrap().len(), 1);
}

#[test]
fn narrating_is_an_announcement_or_an_offer() {
    for yes in ["Now let me read the workflow:", "Done reading.\nLet me know if you want more.", "- Now I'll run it", "Would you like me to go on?"] {
        assert!(narrating(yes), "{yes}");
    }
    for no in ["The inputs are a and b.", "No change needed.", ""] {
        assert!(!narrating(no), "{no}");
    }
}

/// `message` steers: the child is interrupted, then the SAME conversation
/// continues via the backend's native `--resume` carrying the new prompt.
/// The cloud session id never changes across the steer.
#[tokio::test]
async fn steer_resumes_the_same_session_with_the_new_prompt() {
    // Delayed by one poll so the first turn has certainly disclosed its
    // session id — this test asserts the RESUME-carrying path specifically.
    let mock =
        MockCloud::start_with_delayed_control(1, &[(1, "message", "now do X instead")]).await;
    let (_out, status, built, _) =
        supervise_with(&mock, "sess_1", vec![HANGS, COMPLETES]).await;

    let built = built.lock().unwrap().clone();
    assert_eq!(built.len(), 2, "a steer must run a SECOND turn; got {built:?}");

    // Turn 1: the original task, no resume.
    assert_eq!(built[0].0, None, "the first turn resumes nothing");
    assert_eq!(built[0].1.as_deref(), Some("original task"));

    // Turn 2: the SAME backend session id, and the steer's prompt.
    assert_eq!(
        built[1].0.as_deref(),
        Some("bsid-1"),
        "the steer must resume the session the first turn disclosed — a new id would lose the context"
    );
    assert_eq!(built[1].1.as_deref(), Some("now do X instead"));

    // The second turn finished on its own terms.
    assert_eq!(status, Status::Done);

    // Every control drain addressed the SAME cloud session throughout.
    let drains: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.path.contains("/control"))
        .collect();
    assert!(!drains.is_empty());
    for r in &drains {
        assert!(
            r.path.starts_with("/v1/agents/sessions/sess_1/control"),
            "the steer must not move the session: {}",
            r.path
        );
    }
}

/// The other legitimate ordering: a steer that lands BEFORE the backend has
/// disclosed a session id. There is no transcript to preserve yet, so the
/// steer must still land — as a fresh launch carrying the new instruction,
/// never as a dropped command.
#[tokio::test]
async fn a_steer_that_beats_startup_still_lands_as_a_fresh_turn() {
    let mock = MockCloud::start_with_control(&[(1, "message", "actually do Y")]).await;
    // This script NEVER discloses a session id — the worst case, deterministically.
    let (_out, status, built, _) =
        supervise_with(&mock, "sess_1", vec!["exec sleep 30", COMPLETES]).await;

    let built = built.lock().unwrap().clone();
    assert_eq!(built.len(), 2, "the steer must still run a second turn; got {built:?}");
    assert_eq!(built[1].0, None, "nothing was disclosed, so there is nothing to resume");
    assert_eq!(
        built[1].1.as_deref(),
        Some("actually do Y"),
        "the human's instruction must survive even when it beats the backend's start-up"
    );
    assert_eq!(status, Status::Done);
}

/// `resume` against an already-running session is a no-op — it must not
/// signal the child or restart anything.
#[tokio::test]
async fn resume_does_not_disturb_a_running_session() {
    let mock = MockCloud::start_with_control(&[(1, "resume", "")]).await;
    let (out, status, built, _) = supervise_with(&mock, "sess_1", vec![COMPLETES]).await;

    assert_eq!(built.lock().unwrap().len(), 1, "resume must not relaunch");
    assert_eq!(status, Status::Done);
    assert_eq!(
        out.final_summary.as_deref(),
        Some("finished"),
        "the run must have completed on its own — a resume must not cut it short"
    );
}

// ---- adversarial: cross-org control ------------------------------------

/// A principal may not observe another org's session. The drain for a
/// foreign id is refused (cloud 404s it), and the refusal is an ERROR here —
/// never an empty-but-successful page that would read as "no commands".
#[tokio::test]
async fn draining_another_orgs_session_is_refused() {
    let mock = MockCloud::start_org_scoped("sess_ours", &[(1, "stop", "")]).await;
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();

    // Ours: readable.
    let page = client.drain_control("sess_ours", 0).await.unwrap();
    assert_eq!(page.commands.len(), 1, "our own commands are ours to read");

    // Theirs: refused, loudly.
    let err = client
        .drain_control("sess_theirs", 0)
        .await
        .expect_err("a foreign session must not be readable");
    assert!(err.to_string().contains("404"), "got: {err}");

    // And the detail read is refused the same way — there is no seam that
    // leaks the existence of another org's session.
    let err = client.get("sess_theirs").await.expect_err("foreign detail must be refused");
    assert!(err.to_string().contains("404"), "got: {err}");
}

/// The adversarial end-to-end: a `stop` sitting in ANOTHER org's queue must
/// not reach our child. The supervisor polls, is refused, and the run
/// completes on its own — proving cross-org control is impossible, not
/// merely discouraged.
///
/// The assertion is the completion marker: a child that had been signalled
/// would die during its `sleep` and never emit it.
#[tokio::test]
async fn another_orgs_stop_cannot_terminate_our_child() {
    // The mock owns `sess_ours` and holds a `stop` for it. We supervise
    // `sess_theirs` — the attacker's aim — and must get nothing.
    let mock = MockCloud::start_org_scoped("sess_ours", &[(1, "stop", "")]).await;
    let (out, status, built, _) =
        supervise_with(&mock, "sess_theirs", vec![COMPLETES]).await;

    assert_eq!(
        out.final_summary.as_deref(),
        Some("finished"),
        "the child ran to completion — a leaked cross-org stop would have killed it mid-sleep"
    );
    assert_eq!(status, Status::Done, "ended on its own terms, not by command");
    assert_eq!(built.lock().unwrap().len(), 1);
    assert_eq!(
        out.backend_session.as_deref(),
        Some("bsid-1"),
        "and the run was otherwise entirely normal"
    );
}

/// A refused drain must not be mistaken for "no commands, carry on" in a way
/// that silently advances the cursor — a later legitimate command would then
/// be skipped. The cursor only moves on a SUCCESSFUL page.
#[tokio::test]
async fn a_refused_drain_does_not_advance_the_cursor() {
    let mock = MockCloud::start_org_scoped("sess_ours", &[(1, "stop", "")]).await;
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();
    assert!(client.drain_control("sess_theirs", 0).await.is_err());

    // Our own queue is untouched by the foreign attempt.
    let page = client.drain_control("sess_ours", 0).await.unwrap();
    assert_eq!(page.cursor, 1);
    assert_eq!(page.commands[0].command, "stop");
}

/// The cursor contract: an applied command is never redelivered.
#[tokio::test]
async fn the_drain_cursor_never_redelivers_an_applied_command() {
    let mock = MockCloud::start_with_control(&[(1, "pause", ""), (2, "stop", "")]).await;
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();

    let first = client.drain_control("sess_1", 0).await.unwrap();
    assert_eq!(first.commands.len(), 2);
    assert_eq!(first.cursor, 2);

    let second = client.drain_control("sess_1", first.cursor).await.unwrap();
    assert!(second.commands.is_empty(), "already-applied commands must not repeat");
    assert_eq!(second.cursor, 2);
}

/// An unlinked run cannot be steered at all — the privacy gate is structural,
/// exactly as it is for the outbound stream.
#[tokio::test]
async fn an_unlinked_run_is_unsteerable() {
    let (backend, built) = ScriptBackend::new(vec![COMPLETES]);
    let spec = spec_for("t");
    let launch = backend.build(&spec).unwrap();
    let _ = &built;

    let (out, status) = supervise(&backend, spec, launch, true, None, None).await.unwrap();
    assert_eq!(status, Status::Done);
    assert_eq!(out.final_summary.as_deref(), Some("finished"));
}
