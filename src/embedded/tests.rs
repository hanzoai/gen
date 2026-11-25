use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(unix)]
fn install_mock_tools(root: &Root, dev_body: &str) -> PathBuf {
    let bin = root.home().join(".local").join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    crate::exe::stub(&bin, "hanzo-mcp", "exit 0\n");
    crate::exe::stub(&bin, "dev", dev_body)
}

#[cfg(unix)]
#[tokio::test]
async fn multi_turn_embedded_dev_captures_actions_resumes_session_and_injects_mcp_without_home_writes() {
    let root_dir = tempfile::tempdir().unwrap();
    let ws_dir = tempfile::tempdir().unwrap();
    let root = Root::at(root_dir.path());
    let log_file = ws_dir.path().join("dev_invocations.log");

    // Mock `dev` script emitting codex v2 `ThreadEvent` JSONL:
    // - logs argv + HANZO_USER_KEY to `dev_invocations.log`
    // - turn 1 (`exec --json ...`): emits thread.started, mcp_tool_call, command_execution, agent_message, turn.completed
    // - turn 2 (`exec resume --json ... <sid> <prompt>`): verifies resume sid and emits follow-up reply + usage
    let script = format!(
        r#"printf 'ARGS:%s\n' "$*" >> "{log}"
printf 'KEY:%s\n' "${{HANZO_USER_KEY:-}}" >> "{log}"
case "$*" in
  *"exec resume --json"*"th_embed_42"*)
    echo '{{"type":"thread.started","thread_id":"th_embed_42"}}'
    echo '{{"type":"item.completed","item":{{"id":"m2","type":"agent_message","text":"Turn 2 resumed cleanly."}}}}'
    echo '{{"type":"turn.completed","usage":{{"input_tokens":80,"cached_input_tokens":40,"output_tokens":25}}}}'
    ;;
  *"exec --json"*)
    echo '{{"type":"thread.started","thread_id":"th_embed_42"}}'
    echo '{{"type":"item.completed","item":{{"id":"t1","type":"mcp_tool_call","server":"hanzo","tool":"fs_read","arguments":{{"path":"Cargo.toml"}},"status":"completed"}}}}'
    echo '{{"type":"item.completed","item":{{"id":"c1","type":"command_execution","command":"cargo check","aggregated_output":"ok","exit_code":0,"status":"completed"}}}}'
    echo '{{"type":"item.completed","item":{{"id":"m1","type":"agent_message","text":"Inspected Cargo.toml and verified build."}}}}'
    echo '{{"type":"turn.completed","usage":{{"input_tokens":120,"cached_input_tokens":30,"output_tokens":45}}}}'
    ;;
  *)
    echo '{{"type":"error","message":"unexpected invocation"}}'
    exit 1
    ;;
esac
"#,
        log = log_file.display()
    );
    install_mock_tools(&root, &script);

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_ref = calls.clone();
    let token_fn: TokenFn = Arc::new(move || {
        let n = calls_ref.fetch_add(1, Ordering::SeqCst) + 1;
        Some(format!("eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ0dXJuX3tufSJ9.sig_{n}"))
    });

    let host = Arc::new(
        StaticHost::cloud("https://api.hanzo.ai", "hanzo", "z", token_fn)
            .with_org(Some("hanzo".into()))
            .with_refresh_cmd(vec![
                "/usr/local/bin/hanzo".into(),
                "auth".into(),
                "token".into(),
            ]),
    );

    // Turn 1: fresh session with MCP enabled + effort + custom instructions
    let mut req1 = EmbeddedTurnRequest::new(root.clone(), ws_dir.path(), "Check the workspace");
    req1.model = Some("enso-pro".into());
    req1.effort = Some("high".into());
    req1.instructions = Some("Be concise.".into());

    let res1 = run_embedded_turn(host.clone(), req1).await.unwrap();
    assert!(res1.ok, "{res1:?}");
    assert_eq!(res1.status, "done");
    assert_eq!(res1.backend, "dev");
    assert_eq!(res1.backend_session_id.as_deref(), Some("th_embed_42"));
    assert_eq!(res1.model, "enso-pro");
    assert_eq!(
        res1.text.as_deref(),
        Some("Inspected Cargo.toml and verified build.")
    );
    assert_eq!(res1.messages, vec!["Inspected Cargo.toml and verified build."]);
    assert_eq!(res1.tool_calls, 2);
    assert_eq!(res1.actions.len(), 2);
    assert_eq!(res1.actions[0].kind, "tool_call");
    assert_eq!(res1.actions[0].name, "hanzo/fs_read");
    assert!(!res1.actions[0].failed);
    assert_eq!(res1.actions[1].kind, "tool_call");
    assert_eq!(res1.actions[1].name, "shell");
    assert_eq!(res1.actions[1].summary, "cargo check");
    assert!(!res1.actions[1].failed);
    assert_eq!(res1.usage.input_tokens, Some(120));
    assert_eq!(res1.usage.cache_read_tokens, Some(30));
    assert_eq!(res1.usage.output_tokens, Some(45));
    assert!(res1.error.is_none());

    // Turn 2: resume exact `backend_session_id` from Turn 1; `token_fn` rotates dynamically!
    let mut req2 = EmbeddedTurnRequest::new(root.clone(), ws_dir.path(), "Now ship phase 2");
    req2.resume_backend_session = res1.backend_session_id.clone();
    req2.model = Some("enso-pro".into());

    let res2 = run_embedded_turn(host, req2).await.unwrap();
    assert!(res2.ok, "{res2:?}");
    assert_eq!(res2.status, "done");
    assert_eq!(res2.backend_session_id.as_deref(), Some("th_embed_42"));
    assert_eq!(res2.text.as_deref(), Some("Turn 2 resumed cleanly."));
    assert_eq!(res2.usage.input_tokens, Some(80));
    assert_eq!(calls.load(Ordering::SeqCst), 2, "token_fn must be read fresh on each turn");

    // Verify exact `-c` MCP injection, native hanzo auth refresh, and rotated JWTs.
    let logged = std::fs::read_to_string(&log_file).unwrap();
    assert!(logged.contains("mcp_servers.hanzo.command="), "{logged}");
    assert!(logged.contains("hanzo-mcp"), "{logged}");
    assert!(
        !logged.contains("__mcp"),
        "embedded hosts attach hanzo-mcp directly without __mcp wrapper: {logged}"
    );
    assert!(logged.contains("mcp_servers.hanzo.env_vars="), "{logged}");
    assert!(
        logged.contains("model_providers.hanzo.auth.timeout_ms=30000"),
        "{logged}"
    );
    assert!(logged.contains("model_reasoning_effort=\"high\""), "{logged}");
    assert!(logged.contains("sig_1") && logged.contains("sig_2"), "{logged}");
}

#[cfg(unix)]
#[tokio::test]
async fn embedded_local_engine_routes_without_cloud_token_and_cancel_interrupts_cleanly() {
    let root_dir = tempfile::tempdir().unwrap();
    let ws_dir = tempfile::tempdir().unwrap();
    let root = Root::at(root_dir.path());
    let marker = ws_dir.path().join("started.flag");
    let args_log = ws_dir.path().join("local_args.txt");

    // Mock `dev` that discloses `thread.started`, writes a marker file, and blocks on `sleep`.
    let script = format!(
        r#"printf '%s\n' "$*" >> "{args_log}"
echo '{{"type":"thread.started","thread_id":"th_local_99"}}'
echo '{{"type":"item.completed","item":{{"id":"m1","type":"agent_message","text":"Working locally..."}}}}'
touch "{marker}"
exec sleep 30
"#,
        args_log = args_log.display(),
        marker = marker.display()
    );
    install_mock_tools(&root, &script);

    let host = Arc::new(StaticHost::local("http://127.0.0.1:36900"));
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    let mut req = EmbeddedTurnRequest::new(root, ws_dir.path(), "Refactor engine");
    req.model = Some("qwen3-coder".into());
    req.cancel = Some(cancel_rx);

    let waiter_marker = marker.clone();
    tokio::spawn(async move {
        for _ in 0..200 {
            if waiter_marker.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
        let _ = cancel_tx.send(true);
    });

    let started = std::time::Instant::now();
    let res = run_embedded_turn(host, req).await.unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "cancel must interrupt promptly"
    );
    assert!(!res.ok);
    assert_eq!(res.status, "interrupted");
    assert_eq!(
        res.backend_session_id.as_deref(),
        Some("th_local_99"),
        "backend_session_id must survive cancellation so the caller can steer/resume"
    );
    assert_eq!(res.text.as_deref(), Some("Working locally..."));
    assert!(res.error.is_none());

    let argv_dump = std::fs::read_to_string(&args_log).unwrap();
    assert!(
        argv_dump.contains("model_providers.hanzocode.base_url=\"http://127.0.0.1:36900/v1\""),
        "{argv_dump}"
    );
    assert!(argv_dump.contains("model=\"qwen3-coder\""), "{argv_dump}");
}

#[cfg(unix)]
#[tokio::test]
async fn embedded_turn_mirrors_to_linked_cloud_session_and_redacts_host_secrets() {
    let mock = crate::testmock::MockCloud::start().await;
    let root_dir = tempfile::tempdir().unwrap();
    let ws_dir = tempfile::tempdir().unwrap();
    let root = Root::at(root_dir.path());

    let script = concat!(
        r#"echo '{"type":"thread.started","thread_id":"th_linked_1"}'"#, "\n",
        r#"echo '{"type":"item.completed","item":{"id":"s1","type":"collab_tool_call","tool":"spawn_worker","receiver_thread_ids":["th_2"],"prompt":"audit tests"}}'"#, "\n",
        r#"echo '{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"Mirrored complete."}}'"#, "\n",
        r#"echo '{"type":"turn.completed","usage":{"input_tokens":50,"cached_input_tokens":0,"output_tokens":20}}'"#, "\n",
    );
    install_mock_tools(&root, script);

    let host_val = StaticHost::cloud(
        mock.base_url(),
        "hanzo",
        "z",
        Arc::new(|| Some("TOK_LIVE".into())),
    )
    .with_hanzo_key(Some("hk-super-secret".into()));
    assert!(!format!("{host_val:?}").contains("hk-super-secret"));

    let mut req = EmbeddedTurnRequest::new(root, ws_dir.path(), "Audit and mirror");
    req.link_session_id = Some("sess_embed_cloud".into());

    let res = run_embedded_turn(Arc::new(host_val), req).await.unwrap();
    assert!(res.ok);
    assert_eq!(res.status, "done");
    assert_eq!(res.actions.len(), 1);
    assert_eq!(res.actions[0].kind, "spawn");
    assert_eq!(res.actions[0].name, "spawn_worker");
    assert_eq!(res.actions[0].summary, "audit tests");

    let posted: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/agents/sessions/sess_embed_cloud/events")
        .collect();
    assert!(posted.len() >= 3, "expected context + spawn + message + usage events: {posted:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn embedded_turn_stderr_crash_records_closing_error_status_event_and_reason() {
    let mock = crate::testmock::MockCloud::start().await;
    let root_dir = tempfile::tempdir().unwrap();
    let ws_dir = tempfile::tempdir().unwrap();
    let root = Root::at(root_dir.path());

    // Simulates `dev` exiting non-zero with only stderr output and no stdout JSONL.
    let script = concat!(
        r#"echo "error: MCP server 'hanzo' failed handshake" >&2"#, "\n",
        "exit 2\n",
    );
    install_mock_tools(&root, script);

    let host_val = StaticHost::cloud(
        mock.base_url(),
        "hanzo",
        "z",
        Arc::new(|| Some("TOK_LIVE".into())),
    );
    let mut req = EmbeddedTurnRequest::new(root, ws_dir.path(), "machines");
    req.link_session_id = Some("sess_embed_err".into());

    let res = run_embedded_turn(Arc::new(host_val), req).await.unwrap();
    assert!(!res.ok);
    assert_eq!(res.status, "error");
    let err = res.error.as_deref().unwrap_or("");
    assert!(err.contains("dev exited with status 2"), "got: {err}");
    assert!(err.contains("MCP server 'hanzo' failed handshake"), "got: {err}");

    let status_events: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| {
            r.path == "/v1/agents/sessions/sess_embed_err/events"
                && r.json()["kind"] == "status"
                && r.json()["payload"]["status"] == "error"
        })
        .collect();
    assert_eq!(status_events.len(), 1, "expected closing error status event");
    let ev_err = status_events[0].json()["payload"]["error"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(ev_err.contains("dev exited with status 2"), "got: {ev_err}");
    assert!(ev_err.contains("MCP server 'hanzo' failed handshake"), "got: {ev_err}");
}
