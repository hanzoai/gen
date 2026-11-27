use super::*;
use crate::testmock::MockCloud;

/// The "where it runs" value every linked session carries. One fixture, so a
/// test cannot accidentally assert against a snapshot with no machine in it.
pub(crate) fn snapshot(backend: &str) -> Snapshot {
    Snapshot {
        machine_id: "m".into(),
        host: "evo".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        cwd: "/w".into(),
        backend: backend.into(),
        backend_version: None,
        gen_version: Some(env!("CARGO_PKG_VERSION").into()),
        repo: Default::default(),
    }
}

fn id(s: &str) -> Account {
    let (owner, name) = s.split_once('/').unwrap();
    Account { owner: owner.into(), name: name.into(), token: "T".into() }
}

/// Same identity: the cloud session is ours, re-attach silently.
#[test]
fn resume_as_the_same_identity_reattaches_without_a_note() {
    assert!(cloud_resume_block("hanzo/z", Some(&id("hanzo/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").is_none());
    assert!(cloud_resume_block("admin/z", Some(&id("admin/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").is_none());
}

/// Different org: the cloud session CANNOT move (the gateway refuses it, and
/// that refusal is tenant isolation working). The local conversation carries,
/// a new cloud session is registered, and we SAY so — never silently.
#[test]
fn resume_across_an_org_boundary_registers_fresh_and_says_so() {
    let note = cloud_resume_block("hanzo/z", Some(&id("admin/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").expect("must warn");
    assert!(note.contains("hanzo/z") && note.contains("admin/z"));
    assert!(note.contains("NEW cloud session"), "{note}");
    // Billing is stated plainly — it moves to the active identity's org.
    assert!(note.contains("billed to admin"), "{note}");
    // And the way back is offered rather than done for them.
    assert!(note.contains("hanzo auth use hanzo/z"), "{note}");
}

/// Same human, same username, DIFFERENT org — the exact `admin/z` vs
/// `hanzo/z` case. `owner` alone decides; a name match must not re-attach.
#[test]
fn the_same_username_in_another_org_is_still_cross_org() {
    assert!(cloud_resume_block("hanzo/z", Some(&id("admin/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").is_some());
    assert!(cloud_resume_block("admin/z", Some(&id("hanzo/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").is_some());
}

/// A record predating identity tracking has unknown provenance. It cannot be
/// PROVEN ours, so it is treated exactly like a cross-org resume: fail closed
/// on the cloud id, keep the local conversation, and explain.
#[test]
fn a_record_of_unknown_provenance_does_not_reattach() {
    let note = cloud_resume_block("", Some(&id("admin/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").expect("must warn");
    assert!(note.contains("predates identity tracking"), "{note}");
    assert!(note.contains("NEW cloud session"), "{note}");
}

/// Unlinked run: no bearer, so nothing reaches cloud and there is no session
/// to own. No note — there is nothing to tell the user about.
#[test]
fn an_unauthenticated_resume_has_no_cloud_session_to_reason_about() {
    assert!(cloud_resume_block("hanzo/z", None, "https://api.hanzo.ai", "https://api.hanzo.ai").is_none());
    assert!(cloud_resume_block("", None, "https://api.hanzo.ai", "https://api.hanzo.ai").is_none());
}

#[test]
fn link_gate_no_link_wins_then_link_then_persisted() {
    // `--no-link` always wins — over `--link` AND over a persisted `true`
    // (the new default), so the opt-out is absolute.
    assert!(!effective_link(true, true, true)); // --no-link beats --link
    assert!(!effective_link(false, true, true)); // --no-link beats persisted true
    // `--link` forces on when there is no `--no-link`.
    assert!(effective_link(true, false, false));
    // No flags: the persisted default decides — ON by default now, and a
    // persisted `link = false` is the opt-out.
    assert!(effective_link(false, false, true)); // persisted default (on)
    assert!(!effective_link(false, false, false)); // persisted opt-out
}

/// The run-target register uses the SAME structural auth gate as the session
/// link: an unauthenticated run (no bearer) never builds a cloud request, and
/// `--no-link` suppresses it even when signed in.
#[test]
fn unauthenticated_run_registers_no_target() {
    assert!(!links_target(true, false)); // signed out, link intended -> no target
    assert!(!links_target(false, true)); // signed in, --no-link -> no target
    assert!(links_target(true, true)); // signed in + link -> register
}


#[tokio::test]
async fn resolve_fresh_registers_a_new_session() {
    let mock = MockCloud::start().await;
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();
    let (id, forked) = resolve_cloud_session(&client, &snapshot("claude"), "t", None).await.unwrap();
    assert_eq!(id, "sess_mock");
    assert!(forked.is_none());
    assert!(mock.requests().iter().any(|r| r.method == "POST" && r.path == "/v1/agents/sessions"));
}

#[tokio::test]
async fn resume_nonterminal_reuses_same_id_without_registering() {
    let mock = MockCloud::start_get_status("paused").await;
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();
    let (id, forked) = resolve_cloud_session(&client, &snapshot("claude"), "t", Some("sess_old")).await.unwrap();
    assert_eq!(id, "sess_old", "must re-attach the SAME id");
    assert!(forked.is_none());
    let reqs = mock.requests();
    assert!(reqs.iter().any(|r| r.method == "GET" && r.path == "/v1/agents/sessions/sess_old"));
    assert!(reqs.iter().any(|r| r.method == "PATCH" && r.json()["status"] == "running"));
    assert!(!reqs.iter().any(|r| r.method == "POST" && r.path == "/v1/agents/sessions"));
}

#[tokio::test]
async fn resume_terminal_forks_a_new_session_with_lineage() {
    let mock = MockCloud::start_get_status("done").await;
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();
    let (id, forked) = resolve_cloud_session(&client, &snapshot("claude"), "t", Some("sess_old")).await.unwrap();
    assert_eq!(id, "sess_mock");
    assert_eq!(forked.as_deref(), Some("sess_old"));
    assert!(mock.requests().iter().any(|r| r.method == "POST" && r.path == "/v1/agents/sessions"));
}

/// Lineage is only written for a session we VERIFIED.
///
/// Every `GET` failure — 403 (another org), 404 (gone), 5xx/timeout/DNS —
/// leaves us unable to say the id is ours or even real, so we must register
/// with NO `resumedFrom` rather than record a pointer that dangles or names
/// another tenant. Enforced in the FUNCTION, not just at today's call site.
#[tokio::test]
async fn an_unverifiable_session_forks_with_no_lineage() {
    for code in [403u16, 404, 500] {
        let mock = MockCloud::start_session_get_failing(code).await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();

        let (id, forked) = resolve_cloud_session(&client, &snapshot("claude"), "t", Some("sess_other_org"))
            .await
            .unwrap();

        assert_eq!(id, "sess_mock", "a fresh session is registered ({code})");
        assert_eq!(
            forked, None,
            "must NOT record resumedFrom for an unverified session ({code})"
        );
        // And the id we could not verify never reached cloud as lineage.
        let posted = mock
            .requests()
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/v1/agents/sessions")
            .map(|r| r.json().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !posted.contains("sess_other_org"),
            "leaked an unverified id into the register body ({code}): {posted}"
        );
    }
}

/// A cloud id minted by one control plane means nothing to another, so
/// `hanzo network use local` + resume of a prod session must not re-attach.
#[test]
fn resume_on_a_different_network_does_not_reattach() {
    let note = cloud_resume_block(
        "hanzo/z",
        Some(&id("hanzo/z")),
        "https://api.hanzo.ai",
        "http://localhost:3690",
    )
    .expect("must warn even though the identity matches");
    assert!(note.contains("api.hanzo.ai") && note.contains("localhost:3690"), "{note}");
    assert!(note.contains("NEW cloud session"), "{note}");

    // A trailing slash is not a different network.
    assert!(cloud_resume_block(
        "hanzo/z",
        Some(&id("hanzo/z")),
        "https://api.hanzo.ai/",
        "https://api.hanzo.ai"
    )
    .is_none());

    // A record predating the api field cannot contradict the active network.
    assert!(cloud_resume_block("hanzo/z", Some(&id("hanzo/z")), "", "https://api.hanzo.ai").is_none());
}

#[tokio::test]
async fn finalize_reports_usage_resume_handle_and_terminal_status() {
    let mock = MockCloud::start().await;
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();
    let snapshot = snapshot("claude");
    let outcome = Outcome {
        backend_session: Some("sid-x".into()),
        usage: Usage { input_tokens: Some(9), ..Default::default() },
        ..Default::default()
    };
    let root = crate::root::scratch();
    finalize(&client, "sess_9", &outcome, Status::Done, &snapshot, "https://api.hanzo.ai", "hanzo/z", None, &root, None).await;

    let reqs = mock.requests();
    // usage log event
    assert!(reqs.iter().any(|r| r.path.ends_with("/events") && r.json()["kind"] == "log" && r.json()["payload"]["type"] == "usage"));
    // resume-handle status event
    assert!(reqs.iter().any(|r| r.path.ends_with("/events") && r.json()["kind"] == "status" && r.json()["payload"]["type"] == "resume" && r.json()["payload"]["backendSessionId"] == "sid-x"));
    // terminal status
    assert!(reqs.iter().any(|r| r.method == "PATCH" && r.json()["status"] == "done"));
    // The resume record lands in the session's own root, and nowhere else.
    let record = root.data().join("code/sessions/sess_9.json");
    assert!(record.is_file(), "{}", record.display());
    let _ = std::fs::remove_file(record);
}

#[tokio::test]
async fn finalize_interactive_suspends_to_paused_and_failure_is_error() {
    let mock = MockCloud::start().await;
    let client = SessionClient::new(&mock.base_url(), "T").unwrap();
    let snapshot = snapshot("dev");
    let outcome = Outcome::default();
    finalize(&client, "sess_i", &outcome, Status::Paused, &snapshot, "https://api.hanzo.ai", "hanzo/z", None, &crate::root::scratch(), None).await;
    assert!(mock.requests().iter().any(|r| r.method == "PATCH" && r.json()["status"] == "paused"));

    let mock2 = MockCloud::start().await;
    let client2 = SessionClient::new(&mock2.base_url(), "T").unwrap();
    finalize(&client2, "sess_e", &outcome, Status::Error, &snapshot, "https://api.hanzo.ai", "hanzo/z", None, &crate::root::scratch(), Some("dev exited with status 101")).await;
    assert!(mock2.requests().iter().any(|r| r.method == "PATCH" && r.json()["status"] == "error"));
    // The reason rides BOTH a closing status event and the PATCH; a status that is not an error takes none.
    assert!(
        mock2.requests().iter().any(|r| {
            r.method == "POST"
                && r.path.ends_with("/events")
                && r.json()["kind"] == "status"
                && r.json()["payload"]["status"] == "error"
                && r.json()["payload"]["error"] == "dev exited with status 101"
        }),
        "the reason did not reach a closing status event: {:?}",
        mock2.requests().iter().map(|r| r.json().to_string()).collect::<Vec<_>>()
    );
    assert!(
        mock2.requests().iter().any(|r| r.method == "PATCH" && r.json()["error"] == "dev exited with status 101"),
        "the reason did not reach the PATCH: {:?}",
        mock2.requests().iter().map(|r| r.json().to_string()).collect::<Vec<_>>()
    );
    // Even if a caller passes no reason on Status::Error, finalize synthesizes a non-empty reason on event + PATCH.
    let mock_no_reason = MockCloud::start().await;
    let client_no_reason = SessionClient::new(&mock_no_reason.base_url(), "T").unwrap();
    finalize(&client_no_reason, "sess_nr", &outcome, Status::Error, &snapshot, "https://api.hanzo.ai", "hanzo/z", None, &crate::root::scratch(), None).await;
    assert!(mock_no_reason.requests().iter().any(|r| {
        r.method == "POST"
            && r.path.ends_with("/events")
            && r.json()["kind"] == "status"
            && r.json()["payload"]["error"] == "dev ended with an error"
    }));
    assert!(mock_no_reason.requests().iter().any(|r| {
        r.method == "PATCH"
            && r.json()["status"] == "error"
            && r.json()["error"] == "dev ended with an error"
    }));
    let mock3 = MockCloud::start().await;
    let client3 = SessionClient::new(&mock3.base_url(), "T").unwrap();
    finalize(&client3, "sess_d", &outcome, Status::Done, &snapshot, "https://api.hanzo.ai", "hanzo/z", None, &crate::root::scratch(), Some("not an error")).await;
    assert!(mock3.requests().iter().all(|r| r.method != "PATCH" || r.json().get("error").is_none()));
    assert!(mock3.requests().iter().all(|r| !r.path.ends_with("/events")));
}


/// hanzo-inc/cloud#768: a clean exit suspends a session only when there is a
/// handle to resume it by. `--resume` needs the local record, and `finalize`
/// writes one only for a backend session, so a fresh `dev` TUI that quits
/// (or exits to run its own update) has nothing anyone can reopen: it is done,
/// not paused — a paused row says something is coming back, and the cloud kept
/// billing it as alive.
#[test]
fn an_interactive_exit_pauses_only_what_it_can_resume() {
    let quit: Result<bool> = Ok(true);
    assert_eq!(interactive_end(&quit, true), Status::Paused, "a resumable exit suspends");
    assert_eq!(interactive_end(&quit, false), Status::Done, "nothing to resume: it is over");
    let never_started: Result<bool> = Ok(false);
    assert_eq!(interactive_end(&never_started, true), Status::Error);
    let crashed: Result<bool> = Err(anyhow!("boom"));
    assert_eq!(interactive_end(&crashed, false), Status::Error);
}

#[test]
fn uuid_is_v4_shaped() {
    let u = uuid_v4();
    assert_eq!(u.len(), 36);
    assert_eq!(u.as_bytes()[14], b'4'); // version nibble
    assert!(matches!(u.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
}


/// A missing / deleted cwd yields a CLEAR message, not the cryptic
/// `resolving current dir` chain — a fresh or odd environment never dies
/// mysteriously.
#[test]
fn cwd_or_friendly_explains_a_missing_directory() {
    let ok = cwd_or_friendly(Ok(PathBuf::from("/some/dir"))).unwrap();
    assert_eq!(ok, PathBuf::from("/some/dir"));

    let err = cwd_or_friendly(Err(std::io::Error::from(std::io::ErrorKind::NotFound)))
        .unwrap_err()
        .to_string();
    assert!(err.contains("current directory is unavailable"), "got: {err}");
    assert!(err.to_lowercase().contains("cd into a directory") || err.contains("`cd`"), "got: {err}");
    // The old cryptic phrasing must be gone.
    assert!(!err.contains("resolving current dir"), "got: {err}");
}

#[test]
fn warn_does_not_panic_on_a_closed_stream() {
    struct ClosedPipe;
    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
    }
    warn_to(&mut ClosedPipe, "no folder granted");
}

