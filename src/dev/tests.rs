use super::*;
use crate::backend::McpAttach;
use crate::event::Kind;

fn spec(mode: Mode, api: &str) -> Spec {
    Spec {
        mode,
        task: Some("do it".into()),
        cwd: PathBuf::from("/tmp/proj"),
        routing: Route::Via(Routing::Gateway { api: api.into(), token: "JWT".into(), org: Some("hanzo".into()), model: "enso".into(), small_fast_model: "enso-flash".into(), context_window: 1_000_000 }),
        // The default is auto-approve ON (the confirmed default).
        approval: Approval::Auto,
        mcp: Some(McpAttach { program: "hanzo-mcp".into(), args: vec!["--project-dir".into(), "/tmp/proj".into()] }),
        remote: None,
        structured: true,
        preset_session: None,
        trust_project: false,
        resume: None,
        passthrough: vec![],
        refresh: vec!["hanzo".into(), "auth".into(), "token".into()],
        root: crate::root::scratch(),
    }
}

fn argv(l: &Launch) -> Vec<String> {
    l.command.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect()
}

fn envmap(l: &Launch) -> std::collections::HashMap<String, String> {
    l.command
        .as_std()
        .get_envs()
        .filter_map(|(k, v)| Some((k.to_string_lossy().to_string(), v?.to_string_lossy().to_string())))
        .collect()
}

#[test]
fn codex_and_open_interpreter_are_given_the_gateway_provider_even_at_the_native_api() {
    for agent in [Agent::CODEX, Agent::INTERPRETER] {
        let l = agent.build(&spec(Mode::Headless, "https://api.hanzo.ai")).unwrap();
        let args: Vec<String> = l.command.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.iter().any(|a| a == "model_provider=\"hanzocode\""), "{}: {args:?}", agent.program);
        assert!(args.iter().any(|a| a.starts_with("model_providers.hanzocode.base_url=") && a.contains("https://api.hanzo.ai/v1")), "{args:?}");
    }
    // dev carries the provider natively.
    let l = Agent::DEV.build(&spec(Mode::Headless, "https://api.hanzo.ai")).unwrap();
    assert!(!l.command.as_std().get_args().any(|a| a.to_string_lossy().contains("hanzocode")));
}

/// A task run is told nobody can answer it, once, before the prompt; a caller's own
/// developer instructions stand instead, an interactive session is left to the person
/// in it, and a resumed turn carries it too.
///
/// MUTATION: drop the `headless && task && !own` condition and the interactive and
/// caller-instruction cases carry it.
#[test]
fn a_task_run_is_told_nobody_is_there_to_answer() {
    let told = |l: &Launch| argv(l).iter().filter(|a| a.starts_with("developer_instructions=")).count();
    let l = Agent::DEV.build(&spec(Mode::Headless, "https://api.hanzo.ai")).unwrap();
    assert_eq!(told(&l), 1, "{:?}", argv(&l));
    assert!(argv(&l).iter().any(|a| a.starts_with("developer_instructions=") && a.contains("nobody is present")));
    let at = argv(&l).iter().position(|a| a.starts_with("developer_instructions=")).unwrap();
    assert!(at < argv(&l).iter().position(|a| a == "do it").unwrap(), "before the prompt");

    let mut own = spec(Mode::Headless, "https://api.hanzo.ai");
    own.passthrough = vec!["-c".into(), "developer_instructions=\"mine\"".into()];
    assert_eq!(told(&Agent::DEV.build(&own).unwrap()), 1, "the caller's, and only theirs");
    assert!(argv(&Agent::DEV.build(&own).unwrap()).iter().all(|a| !a.contains("nobody is present")));

    assert_eq!(told(&Agent::DEV.build(&spec(Mode::Interactive, "https://api.hanzo.ai")).unwrap()), 0, "a person is there");

    let mut resumed = spec(Mode::Headless, "https://api.hanzo.ai");
    resumed.resume = Some("th-1".into());
    assert_eq!(told(&Agent::DEV.build(&resumed).unwrap()), 1, "a resumed turn is just as unattended");
}

#[test]
fn headless_exec_json_with_native_provider_needs_only_token_env() {
    let l = Agent::DEV.build(&spec(Mode::Headless, "https://api.hanzo.ai")).unwrap();
    let args = argv(&l);
    assert_eq!(&args[0..2], &["exec", "--json"]);
    assert_eq!(args.last().unwrap(), "do it"); // prompt is the trailing positional
    // MCP attached additively.
    assert!(args.iter().any(|a| a == r#"mcp_servers.hanzo.command="hanzo-mcp""#));
    assert!(args.iter().any(|a| a == r#"mcp_servers.hanzo.args=["--project-dir","/tmp/proj"]"#));
    // Native gateway -> no provider override, just the attributable token env.
    assert!(!args.iter().any(|a| a.contains("model_provider")));
    assert_eq!(envmap(&l).get("HANZO_USER_KEY").map(String::as_str), Some("JWT"));
    assert!(!args.iter().any(|a| a.contains("JWT")), "token must not be in argv");
    // Auto-approve is ON by default: stop asking but KEEP the workspace sandbox
    // (never the full bypass).
    assert!(args.iter().any(|a| a == r#"approval_policy="never""#));
    assert!(args.iter().any(|a| a == r#"sandbox_mode="workspace-write""#));
    assert!(!args.iter().any(|a| a.contains("yolo") || a.contains("dangerously-bypass")));
    // The gateway model's real (1M) window is named as dev's own setting
    // (the true-1M path), since the default window exceeds the 272K fallback.
    assert!(args.iter().any(|a| a.starts_with("model_context_window=")), "gateway route must name the model window: {args:?}");
    assert!(l.cleanup.files.is_empty(), "dev takes the window as a setting, not a file");
}

/// The caller's cloud tools ride as ONE streamable-HTTP server beside hanzo-mcp:
/// its url, and the NAME of the variable holding the bearer. The bearer itself is
/// in the env and nowhere in argv — and only on the Hanzo-routed path, where the
/// child already holds it.
#[test]
fn the_cloud_tools_are_one_http_server_whose_bearer_is_named_not_spelled() {
    let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
    s.routing = Route::Via(Routing::Gateway { api: "https://api.hanzo.ai".into(), token: "JWT".into(), org: Some("hanzo".into()), model: "enso".into(), small_fast_model: "enso-flash".into(), context_window: 1_000_000 });
    s.remote = Some(Remote { url: "https://api.hanzo.ai/v1/mcp".into(), token: "IAM-BEARER".into() });
    let l = Agent::DEV.build(&s).unwrap();
    let args = argv(&l);
    assert!(args.iter().any(|a| a == r#"mcp_servers.cloud.url="https://api.hanzo.ai/v1/mcp""#), "{args:?}");
    assert!(args.iter().any(|a| a == r#"mcp_servers.cloud.bearer_token_env_var="HANZO_MCP_TOKEN""#), "{args:?}");
    // hanzo-mcp is still attached: the cloud server is beside it, not instead.
    assert!(args.iter().any(|a| a == r#"mcp_servers.hanzo.command="hanzo-mcp""#));
    assert_eq!(envmap(&l).get("HANZO_MCP_TOKEN").map(String::as_str), Some("IAM-BEARER"));
    assert!(!args.iter().any(|a| a.contains("IAM-BEARER")), "the bearer must not be in argv");
    // The server's options precede the prompt, so none can be read as it.
    let last = args.iter().rposition(|a| a.starts_with("mcp_servers.cloud.")).unwrap();
    assert!(last < args.iter().position(|a| a == "do it").unwrap());
}

/// `--no-route`, a direct provider, or a route that failed closed: the agent
/// runs shell commands unasked, so the IAM bearer never enters its env and no
/// cloud server is configured, whatever the kit answered.
#[test]
fn the_iam_bearer_is_absent_off_the_hanzo_route() {
    let mut built = 0;
    for routing in [Route::Via(Routing::OpenAI { key: "sk-openai".into() }), Route::Inherit, Route::FailClosed] {
        let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
        s.routing = routing.clone();
        s.remote = Some(Remote { url: "https://api.hanzo.ai/v1/mcp".into(), token: "IAM-BEARER".into() });
        let Ok(l) = Agent::DEV.build(&s) else { continue };
        built += 1;
        assert!(!envmap(&l).contains_key("HANZO_MCP_TOKEN"), "{routing:?}");
        assert!(!envmap(&l).values().any(|v| v.contains("IAM-BEARER")), "{routing:?}");
        assert!(!argv(&l).iter().any(|a| a.contains("mcp_servers.cloud.") || a.contains("IAM-BEARER")), "{routing:?}");
    }
    assert!(built >= 2, "only {built} routes built");
}

#[test]
fn the_cloud_server_is_absent_without_a_kit() {
    let l = Agent::DEV.build(&spec(Mode::Headless, "https://api.hanzo.ai")).unwrap();
    assert!(!argv(&l).iter().any(|a| a.contains("mcp_servers.cloud.")));
    assert!(!envmap(&l).contains_key("HANZO_MCP_TOKEN"));
}

#[test]
fn custom_network_api_defines_a_full_provider() {
    let l = Agent::DEV.build(&spec(Mode::Headless, "http://localhost:3690")).unwrap();
    let args = argv(&l);
    assert!(args.iter().any(|a| a == r#"model_providers.hanzocode.base_url="http://localhost:3690/v1""#));
    assert!(args.iter().any(|a| a == r#"model_providers.hanzocode.wire_api="responses""#));
    assert!(args.iter().any(|a| a == r#"model_provider="hanzocode""#));
}

/// "Logged in with OpenAI": a stored OpenAI key drives codex directly on its
/// native `openai` provider — `OPENAI_API_KEY` in env, no Hanzo provider
/// override, and the key never in argv.
#[test]
fn openai_key_routes_dev_directly_via_env() {
    let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
    s.routing = Route::Via(Routing::OpenAI { key: "sk-openai-SECRET".into() });
    let l = Agent::DEV.build(&s).unwrap();
    let args = argv(&l);
    assert_eq!(envmap(&l).get("OPENAI_API_KEY").map(String::as_str), Some("sk-openai-SECRET"));
    // Direct: no Hanzo gateway provider override, no HANZO_USER_KEY.
    assert!(!args.iter().any(|a| a.contains("model_provider")));
    assert!(!envmap(&l).contains_key("HANZO_USER_KEY"));
    assert!(!args.iter().any(|a| a.contains("sk-openai-SECRET")), "key must not be in argv");
    // A direct route names NO gateway model — the top-level `model` override
    // rides only the gateway path; codex keeps its own default here.
    assert!(!args.iter().any(|a| a.starts_with("model=")), "direct route must not set a gateway model");
}

/// The dev gateway route names the model too (`-c model=<id>`): codex's built-in
/// default is not in the gateway catalog and would 400. Same gap as Claude,
/// fixed analogously — here the resolved built-in default (`enso`).
#[test]
fn gateway_route_names_the_resolved_model() {
    let l = Agent::DEV.build(&spec(Mode::Headless, "https://api.hanzo.ai")).unwrap();
    assert!(argv(&l).iter().any(|a| a == r#"model="enso""#), "gateway route must set -c model=<catalog id>");
}

/// An explicit model (from `--model`, resolved into the routing value) passes
/// straight through to codex's `model` config on the gateway route.
#[test]
fn gateway_route_honors_an_explicit_model() {
    let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
    s.routing = Route::Via(Routing::Gateway {
        api: "https://api.hanzo.ai".into(),
        token: "JWT".into(),
        org: Some("hanzo".into()),
        model: "enso".into(),
        small_fast_model: "enso-flash".into(),
        context_window: 1_000_000,
    });
    let l = Agent::DEV.build(&s).unwrap();
    assert!(argv(&l).iter().any(|a| a == r#"model="enso""#), "gateway route must honor an explicit model");
}

#[test]
fn headless_resume_uses_exec_resume_with_sid_before_prompt() {
    let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
    s.resume = Some("thread-uuid".into());
    let l = Agent::DEV.build(&s).unwrap();
    let args = argv(&l);
    assert_eq!(&args[0..3], &["exec", "resume", "--json"]);
    let sid = args.iter().position(|a| a == "thread-uuid").unwrap();
    let prompt = args.iter().position(|a| a == "do it").unwrap();
    assert!(sid < prompt, "session id must precede the prompt");
}

#[test]
fn interactive_resume_uses_top_level_resume() {
    let mut s = spec(Mode::Interactive, "https://api.hanzo.ai");
    s.task = None;
    s.resume = Some("tid".into());
    let l = Agent::DEV.build(&s).unwrap();
    let args = argv(&l);
    assert_eq!(args[0], "resume");
    assert!(args.iter().any(|a| a == "tid"));
    assert!(!args.iter().any(|a| a == "exec"));
}

#[test]
fn parse_thread_started_is_backend_session() {
    let out = Agent::DEV.parse(r#"{"type":"thread.started","thread_id":"th-1"}"#);
    assert!(matches!(&out[0], Mapped::BackendSession(s) if s == "th-1"));
}

#[test]
fn parse_agent_message_and_command_execution() {
    let msg = Agent::DEV.parse(r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"hi"}}"#);
    assert!(matches!(&msg[0], Mapped::Event{kind:Kind::Message, payload} if payload["text"]=="hi"));

    let cmd = Agent::DEV.parse(r#"{"type":"item.completed","item":{"id":"i2","type":"command_execution","command":"ls -la","aggregated_output":"a\nb","exit_code":0,"status":"completed"}}"#);
    let Mapped::Event{kind, payload} = &cmd[0] else { panic!() };
    assert_eq!(*kind, Kind::ToolCall);
    assert_eq!(payload["name"], "shell");
    assert_eq!(payload["input"]["command"], "ls -la");
    assert_eq!(payload["exitCode"], 0);
}

#[test]
fn parse_mcp_and_collab_and_turn_usage() {
    let mcp = Agent::DEV.parse(r#"{"type":"item.completed","item":{"id":"i","type":"mcp_tool_call","server":"hanzo","tool":"fs_read","arguments":{"path":"x"},"status":"completed"}}"#);
    assert!(matches!(&mcp[0], Mapped::Event{kind:Kind::ToolCall, payload} if payload["name"]=="hanzo/fs_read"));

    let collab = Agent::DEV.parse(r#"{"type":"item.completed","item":{"id":"i","type":"collab_tool_call","tool":"spawn","receiver_thread_ids":["t2"],"prompt":"go"}}"#);
    assert!(matches!(&collab[0], Mapped::Event{kind:Kind::Spawn, ..}));

    let turn = Agent::DEV.parse(r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":5,"reasoning_output_tokens":1}}"#);
    let Mapped::Usage(u) = &turn[0] else { panic!() };
    assert_eq!(u.input_tokens, Some(10));
    assert_eq!(u.output_tokens, Some(5));
    assert_eq!(u.cache_read_tokens, Some(2));
}

#[test]
fn parse_turn_failed_and_error_are_terminal_not_ok() {
    assert!(matches!(
        Agent::DEV.parse(r#"{"type":"turn.failed","error":{"message":"boom"}}"#).last().unwrap(),
        Mapped::Terminal{ok:false, ..}
    ));
    assert!(matches!(
        Agent::DEV.parse(r#"{"type":"error","message":"fatal"}"#).last().unwrap(),
        Mapped::Terminal{ok:false, ..}
    ));
}

/// Dev mirror: `FailClosed` (a provider is SELECTED but no usable key)
/// must clear dev's model-auth env so an inherited `OPENAI_API_KEY`/
/// `OPENAI_BASE_URL` can't silently drive the child. `--no-route` (`Inherit`)
/// leaves it untouched — the two are distinct.
#[test]
fn fail_closed_clears_dev_model_auth_inherit_does_not() {
    let cleared = |l: &Launch, var: &str| {
        l.command.as_std().get_envs().any(|(k, v)| k.to_string_lossy() == var && v.is_none())
    };

    let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
    s.routing = Route::FailClosed;
    let l = Agent::DEV.build(&s).unwrap();
    for var in ["OPENAI_API_KEY", "OPENAI_BASE_URL", "HANZO_USER_KEY"] {
        assert!(cleared(&l, var), "{var} must be cleared under FailClosed");
    }

    let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
    s.routing = Route::Inherit;
    let l = Agent::DEV.build(&s).unwrap();
    for var in ["OPENAI_API_KEY", "OPENAI_BASE_URL"] {
        assert!(
            !l.command.as_std().get_envs().any(|(k, _)| k.to_string_lossy() == var),
            "{var} must be untouched under --no-route (Inherit)"
        );
    }
}

/// Auto (the default) keeps the sandbox; Ask leaves the user's mode; Bypass
/// (`--no-sandbox`) drops the sandbox with the full bypass flag. codex 0.144.x
/// has no `--full-auto`/`--yolo`, so those never appear.
#[test]
fn approval_maps_to_codex_flags() {
    let build = |a: Approval| {
        let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
        s.approval = a;
        argv(&Agent::DEV.build(&s).unwrap())
    };

    let auto = build(Approval::Auto);
    assert!(auto.iter().any(|a| a == r#"approval_policy="never""#));
    assert!(auto.iter().any(|a| a == r#"sandbox_mode="workspace-write""#));
    assert!(!auto.iter().any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));

    let ask = build(Approval::Ask);
    assert!(!ask.iter().any(|a| a.contains("approval_policy")), "Ask leaves the user's own mode");
    assert!(!ask.iter().any(|a| a.contains("sandbox_mode")));
    assert!(!ask.iter().any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));

    let bypass = build(Approval::Bypass);
    assert!(bypass.iter().any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
    // Bypass replaces the sandboxed config, not adds to it.
    assert!(!bypass.iter().any(|a| a.contains("sandbox_mode")));

    for args in [auto, ask, bypass] {
        assert!(!args.iter().any(|a| a.contains("yolo") || a == "--full-auto"));
    }
}

/// The user's own `config.toml` wins: a key they set there is not overridden,
/// whether at the top level or in the profile their config selects.
#[test]
fn the_users_config_wins_over_auto_approve() {
    let none = auto_overrides(None);
    assert_eq!(none, vec!["-c", r#"approval_policy="never""#, "-c", r#"sandbox_mode="workspace-write""#]);

    let theirs: toml::Table = r#"sandbox_mode = "danger-full-access""#.parse().unwrap();
    assert_eq!(auto_overrides(Some(&theirs)), vec!["-c", r#"approval_policy="never""#]);

    let both: toml::Table = "approval_policy = \"on-request\"\nsandbox_mode = \"read-only\"".parse().unwrap();
    assert!(auto_overrides(Some(&both)).is_empty());

    let profile: toml::Table =
        "profile = \"work\"\n[profiles.work]\nsandbox_mode = \"danger-full-access\"".parse().unwrap();
    assert_eq!(auto_overrides(Some(&profile)), vec!["-c", r#"approval_policy="never""#]);

    // A profile the config does not select changes nothing.
    let unused: toml::Table = "[profiles.work]\nsandbox_mode = \"read-only\"".parse().unwrap();
    assert_eq!(auto_overrides(Some(&unused)), none);
}

/// On a custom gateway a sign-in token is asked of this CLI again before it
/// ages out; an API key, which never does, rides the environment as before.
#[test]
fn a_sign_in_token_is_refreshed_on_a_custom_gateway() {
    let mut s = spec(Mode::Headless, "https://gw.example.com");
    s.routing = Route::Via(Routing::Gateway {
        api: "https://gw.example.com".into(),
        token: "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ6In0.c2ln".into(),
        org: Some("acme".into()),
        model: "enso".into(),
        small_fast_model: "enso-flash".into(),
        context_window: 1_000_000,
    });
    let args = argv(&Agent::DEV.build(&s).unwrap());
    assert!(args.iter().any(|a| a == r#"model_providers.hanzocode.auth.command="hanzo""#));
    assert!(args.iter().any(|a| a == r#"model_providers.hanzocode.auth.args=["auth","token"]"#));
    assert!(args.iter().any(|a| a == "model_providers.hanzocode.auth.refresh_interval_ms=600000"));
    assert!(args.iter().any(|a| a == "model_providers.hanzocode.auth.timeout_ms=30000"));
    assert!(!args.iter().any(|a| a.contains("model_providers.hanzocode.env_key")), "auth and env_key cannot both be set");

    let key = argv(&Agent::DEV.build(&spec(Mode::Headless, "https://gw.example.com")).unwrap());
    assert!(key.iter().any(|a| a == r#"model_providers.hanzocode.env_key="HANZO_USER_KEY""#));
    assert!(!key.iter().any(|a| a.contains("hanzocode.auth")));
}

/// Multi-hour native `dev` runs on `https://api.hanzo.ai` also configure
/// `model_providers.hanzo.auth.*` with `timeout_ms=30000` when given a session JWT,
/// so 1h IAM tokens refresh through the Host's exact brand instead of expiring at 60m.
#[test]
fn native_api_session_token_configures_hanzo_provider_auth_refresh_with_timeout() {
    let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
    s.routing = Route::Via(Routing::Gateway {
        api: "https://api.hanzo.ai".into(),
        token: "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ6In0.c2ln".into(),
        org: Some("hanzo".into()),
        model: "enso-auto".into(),
        small_fast_model: "enso-flash".into(),
        context_window: 1_000_000,
    });
    s.refresh = vec!["/usr/local/bin/hanzo".into(), "auth".into(), "token".into(), "--brand".into(), "lux".into()];
    let args = argv(&Agent::DEV.build(&s).unwrap());
    assert!(args.iter().any(|a| a == r#"model_providers.hanzo.auth.command="/usr/local/bin/hanzo""#), "{args:?}");
    assert!(args.iter().any(|a| a == r#"model_providers.hanzo.auth.args=["auth","token","--brand","lux"]"#), "{args:?}");
    assert!(args.iter().any(|a| a == "model_providers.hanzo.auth.refresh_interval_ms=600000"), "{args:?}");
    assert!(args.iter().any(|a| a == "model_providers.hanzo.auth.timeout_ms=30000"), "{args:?}");
    assert!(!args.iter().any(|a| a.starts_with("model_provider=")), "native hanzo provider remains the default: {args:?}");

    // And headless instructions carry the <= 1,000-line + atomic commit guidance.
    let instr = args.iter().find(|a| a.starts_with("developer_instructions=")).unwrap();
    assert!(instr.contains("<= 1,000 lines") && instr.contains("<type>(<scope>): <summary>"), "{instr}");
}

/// The gateway route names the model's REAL window through a `model_catalog_json`
/// (the true-1M path) whenever it exceeds codex's 272K fallback, declaring both
/// `context_window` and `max_context_window` (codex clamps to the latter). A
/// window at or below the fallback writes NO catalog — codex's own resolution
/// stands and a temp file is never created.
#[test]
fn gateway_route_names_the_model_window_via_catalog() {
    // Default 1M window -> codex gets a catalog written + declared 1M.
    let l = Agent::CODEX.build(&spec(Mode::Headless, "https://api.hanzo.ai")).unwrap();
    let path = argv(&l)
        .into_iter()
        .find_map(|a| a.strip_prefix("model_catalog_json=").map(str::to_string))
        .expect("gateway route writes a model catalog");
    let body = std::fs::read_to_string(path.trim_matches('"')).expect("catalog file exists");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["models"][0]["slug"], "enso");
    assert_eq!(v["models"][0]["context_window"], 1_000_000);
    assert_eq!(v["models"][0]["max_context_window"], 1_000_000, "the ceiling must be raised or codex clamps");
    assert_eq!(l.cleanup.files.len(), 1);

    // EVERY field the agents' catalog parser requires must be present. It
    // refuses the whole document on the first missing one, and that refusal
    // killed the session outright — a lost context window would have been
    // survivable, a session that cannot start is not. Both agents share
    // this required set; extra keys are ignored by both.
    for field in [
        "slug",
        "display_name",
        "base_instructions",
        "context_window",
        "max_context_window",
        "supported_reasoning_levels",
        "supports_reasoning_summaries",
        "supports_parallel_tool_calls",
        "experimental_supported_tools",
        "support_verbosity",
        "shell_type",
        "truncation_policy",
        "visibility",
        "supported_in_api",
        "priority",
    ] {
        assert!(!v["models"][0][field].is_null(), "catalog entry must declare `{field}`");
    }
    assert_eq!(v["models"][0]["truncation_policy"]["mode"], "tokens");
    assert_eq!(v["models"][0]["truncation_policy"]["limit"], 1_000_000);

    // A standard window (<= codex's fallback) writes NO catalog.
    let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
    s.routing = Route::Via(Routing::Gateway { api: "https://api.hanzo.ai".into(), token: "JWT".into(), org: None, model: "enso".into(), small_fast_model: "enso-flash".into(), context_window: 200_000 });
    let l = Agent::CODEX.build(&s).unwrap();
    assert!(!argv(&l).iter().any(|a| a.starts_with("model_catalog_json=")), "no catalog below codex's fallback");
    assert!(l.cleanup.files.is_empty());

    // dev has the setting itself: the window rides in as `model_context_window`,
    // and no file is written for it.
    let l = Agent::DEV.build(&spec(Mode::Headless, "https://api.hanzo.ai")).unwrap();
    assert!(argv(&l).iter().any(|a| a == "model_context_window=1000000"), "{:?}", argv(&l));
    assert!(!argv(&l).iter().any(|a| a.starts_with("model_catalog_json=")));
    assert!(l.cleanup.files.is_empty());
}

#[test]
fn gateway_route_passes_unified_cloud_iam_auth_and_env_vars_to_hanzo_mcp() {
    let mut s = spec(Mode::Headless, "https://api.hanzo.ai/");
    s.routing = Route::Via(Routing::Gateway {
        api: "https://api.hanzo.ai/".into(),
        token: "IAM-JWT".into(),
        org: Some("acme".into()),
        model: "enso-auto".into(),
        small_fast_model: "enso-flash".into(),
        context_window: 1_000_000,
    });
    let l = Agent::DEV.build(&s).unwrap();
    let env = envmap(&l);
    assert_eq!(env.get("HANZO_USER_KEY").map(String::as_str), Some("IAM-JWT"));
    assert_eq!(env.get("HANZO_API_KEY").map(String::as_str), Some("IAM-JWT"));
    assert_eq!(env.get("HANZO_API_URL").map(String::as_str), Some("https://api.hanzo.ai"));
    assert_eq!(env.get("HANZO_API_BASE").map(String::as_str), Some("https://api.hanzo.ai"));
    assert_eq!(env.get("HANZO_ORG").map(String::as_str), Some("acme"));

    let args = argv(&l);
    assert!(
        args.iter().any(|a| a == r#"mcp_servers.hanzo.env_vars=["HANZO_USER_KEY","HANZO_API_KEY","HANZO_API_URL","HANZO_API_BASE","HANZO_ORG","LOCAL_KAI_URL"]"#),
        "dev must whitelist Hanzo auth env vars for hanzo-mcp stdio server: {args:?}"
    );
    assert!(!args.iter().any(|a| a.contains("IAM-JWT")), "token must never appear in argv");

    // An existing explicit HANZO_API_KEY in the parent shell is not overwritten.
    let mut cmd = tokio::process::Command::from(crate::exe::bare("/bin/true"));
    apply_gateway_mcp_env(&mut cmd, "https://api.hanzo.ai", "IAM-JWT", None, Some("sk-explicit"));
    let envs: std::collections::HashMap<String, String> = cmd
        .as_std()
        .get_envs()
        .filter_map(|(k, v)| Some((k.to_string_lossy().to_string(), v?.to_string_lossy().to_string())))
        .collect();
    assert_eq!(envs.get("HANZO_USER_KEY").map(String::as_str), Some("IAM-JWT"));
    assert!(!envs.contains_key("HANZO_API_KEY"), "existing HANZO_API_KEY must be preserved");
    assert!(!envs.contains_key("HANZO_ORG"));
}
