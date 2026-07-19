//! The `dev` (Codex fork) backend.
//!
//! Headless runs stream the v2 `ThreadEvent` JSONL via `dev exec --json`; MCP is
//! attached with `-c mcp_servers.hanzo.*` overrides (ADDITIVE — the user's own
//! servers, config and `dev login` are untouched, since we never repoint
//! `CODEX_HOME`); model calls route through the native `hanzo` provider
//! (`api.hanzo.ai/v1`) with the bearer supplied as `HANZO_USER_KEY`.
//!
//! Auto-approve is ON by default (the confirmed default): `-c approval_policy="never"
//! -c sandbox_mode="workspace-write"` — it stops asking but KEEPS the workspace
//! sandbox. `--ask`/`--safe` (or `autoApprove: false`) hand back the user's own
//! mode; `--no-sandbox` escalates to the full `--dangerously-bypass-approvals-and-sandbox`
//! (a deliberate per-invocation act, never a persisted default). `dev` reads no
//! repo-local MCP config, so it has no trust-gate vector.
//!
//! On the gateway route we give `dev` the model's REAL context window via a
//! `model_catalog_json` we control — a custom/unknown model would otherwise clamp
//! to codex's 272K fallback. This is the true-1M backend (open, we set the window
//! directly), unlike the Claude backend which a closed client caps at 200K.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::backend::{Approval, Backend, Launch, Mode, Route, Routing, Spec};
use super::event::{cap, clamp, Mapped, Usage};

pub struct Dev;

/// The gateway origin the native `hanzo` provider already targets; when the
/// active network matches it we need no provider override, only the token env.
const NATIVE_API: &str = "https://api.hanzo.ai";

/// Codex's context-window fallback for an unknown model. A gateway model's window
/// is NAMED via a `model_catalog_json` only when it EXCEEDS this, so a run never
/// shrinks a model below codex's own default.
const CODEX_FALLBACK_WINDOW: u64 = 272_000;

impl Backend for Dev {
    fn label(&self) -> &'static str {
        "dev"
    }

    fn version(&self) -> Option<String> {
        super::backend::backend_version("dev")
    }

    fn build(&self, spec: &Spec) -> Result<Launch> {
        let mut cmd = tokio::process::Command::new("dev");
        cmd.current_dir(&spec.cwd);

        let mut args: Vec<String> = Vec::new();
        let mut cleanup: Vec<tempfile::TempPath> = Vec::new();
        let headless = spec.mode == Mode::Headless;

        // Subcommand + resume selection. The JSONL event stream (`--json`) is
        // requested ONLY when we stream to cloud.
        match (headless, &spec.resume) {
            (true, Some(_)) => args.extend(["exec".into(), "resume".into()]),
            (true, None) => args.push("exec".into()),
            (false, Some(_)) => args.push("resume".into()),
            (false, None) => {}
        }
        if headless && spec.structured {
            args.push("--json".into());
        }

        // Auto-approve. `Auto` (the default) stops asking but KEEPS the workspace
        // sandbox; `Bypass` (`--no-sandbox`) drops it entirely; `Ask` leaves the
        // user's own mode. codex 0.144.x has no `--full-auto`/`--yolo`.
        match spec.approval {
            Approval::Auto => {
                args.push("-c".into());
                args.push(cfg_string("approval_policy", "never"));
                args.push("-c".into());
                args.push(cfg_string("sandbox_mode", "workspace-write"));
            }
            Approval::Bypass => args.push("--dangerously-bypass-approvals-and-sandbox".into()),
            Approval::Ask => {}
        }

        // MCP: additive `-c` overrides (never touches the user's config/login).
        if let Some(mcp) = &spec.mcp {
            args.push("-c".into());
            args.push(cfg_string("mcp_servers.hanzo.command", &mcp.program));
            args.push("-c".into());
            args.push(cfg_array("mcp_servers.hanzo.args", &mcp.args));
        }

        // Routing (credential via env, never argv):
        match &spec.routing {
            // Gateway: the native `hanzo` provider + token env, or a full custom
            // provider when the active network points somewhere other than the
            // native gateway. Name the model too — codex's built-in default is not
            // in the gateway catalog and would 400. The routing decision already
            // resolved a valid catalog id (`--model` > `~/.hanzo/settings.json` >
            // built-in default `enso`; `dev` reads no `ANTHROPIC_*` env). `dev` has
            // no small/fast model.
            Route::Via(Routing::Gateway { api, token, model, context_window, .. }) => {
                cmd.env("HANZO_USER_KEY", token);
                args.push("-c".into());
                args.push(cfg_string("model", model));
                // Name the model's REAL window so codex doesn't clamp a custom id to
                // its 272K fallback — the true-1M path. A `model_catalog_json` (which
                // we control) declares it; applied at startup via `-c`.
                if *context_window > CODEX_FALLBACK_WINDOW {
                    let mut file = tempfile::Builder::new()
                        .prefix("hanzo-codex-catalog-")
                        .suffix(".json")
                        .tempfile()
                        .context("creating model-catalog temp file")?;
                    file.write_all(model_catalog(model, *context_window).as_bytes())
                        .context("writing model catalog")?;
                    let path = file.into_temp_path();
                    args.push("-c".into());
                    args.push(cfg_string("model_catalog_json", &path.to_string_lossy()));
                    cleanup.push(path);
                }
                if api.trim_end_matches('/') != NATIVE_API {
                    let base = format!("{}/v1", api.trim_end_matches('/'));
                    args.push("-c".into());
                    args.push(cfg_string("model_providers.hanzocode.name", "Hanzo Code"));
                    args.push("-c".into());
                    args.push(cfg_string("model_providers.hanzocode.base_url", &base));
                    args.push("-c".into());
                    args.push(cfg_string("model_providers.hanzocode.wire_api", "responses"));
                    args.push("-c".into());
                    args.push(cfg_string("model_providers.hanzocode.env_key", "HANZO_USER_KEY"));
                    args.push("-c".into());
                    args.push(cfg_string("model_provider", "hanzocode"));
                }
            }
            // Direct OpenAI: the user's own key on codex's native `openai`
            // provider (the default), reached at api.openai.com.
            Route::Via(Routing::OpenAI { key }) => {
                cmd.env("OPENAI_API_KEY", key);
            }
            // We hold NOTHING dev can use — either an Anthropic key (codex speaks
            // the OpenAI wire protocol, so the resolver never pairs it) or
            // `FailClosed` (a provider was SELECTED but no usable credential
            // resolved). FAIL CLOSED: clear dev's model-auth env so a stray shell
            // `OPENAI_API_KEY`/`OPENAI_BASE_URL` can't silently drive the child.
            Route::Via(Routing::Anthropic { .. }) | Route::FailClosed => {
                cmd.env_remove("OPENAI_API_KEY");
                cmd.env_remove("OPENAI_BASE_URL");
                cmd.env_remove("HANZO_USER_KEY");
            }
            // `--no-route` (or an unconfigured, signed-out run): dev uses its OWN
            // account (`dev login` / native provider). Leave inherited model-auth
            // exactly as the shell has it — the pass-through `--no-route` promises.
            Route::Inherit => {}
        }

        // Passthrough flags precede positionals so they can't swallow the prompt.
        args.extend(spec.passthrough.iter().cloned());

        // Positionals: [session-id] then [prompt], per the resume/exec grammar.
        if let Some(sid) = &spec.resume {
            args.push(sid.clone());
        }
        if headless {
            if let Some(task) = &spec.task {
                args.push(task.clone());
            }
        }

        cmd.args(&args);
        Ok(Launch { command: cmd, cleanup })
    }

    fn parse(&self, line: &str) -> Vec<Mapped> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        match v.get("type").and_then(Value::as_str) {
            Some("thread.started") => v
                .get("thread_id")
                .and_then(Value::as_str)
                .map(|id| vec![Mapped::BackendSession(id.to_string())])
                .unwrap_or_default(),
            Some("turn.completed") => v
                .get("usage")
                .map(|u| vec![Mapped::Usage(usage(u))])
                .unwrap_or_default(),
            Some("turn.failed") => vec![Mapped::Terminal {
                ok: false,
                summary: v.pointer("/error/message").and_then(Value::as_str).map(String::from),
            }],
            Some("error") => vec![Mapped::Terminal {
                ok: false,
                summary: v.get("message").and_then(Value::as_str).map(String::from),
            }],
            Some("item.completed") => v.get("item").map(item).unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn transcript_path(&self, _cwd: &Path, _backend_session_id: &str) -> Option<PathBuf> {
        // `dev`'s rollout files are date-bucketed under CODEX_HOME; native
        // `dev exec resume <id>` locates them itself, so we don't tail them.
        None
    }
}

/// Map one completed `ThreadItem` into the normalized vocabulary. Emitting only
/// on `item.completed` keeps exactly one event per item (no started/updated dup).
fn item(it: &Value) -> Vec<Mapped> {
    let id = it.get("id").and_then(Value::as_str);
    match it.get("type").and_then(Value::as_str) {
        Some("agent_message") => text_of(it).map(|t| vec![Mapped::message("assistant", t)]).unwrap_or_default(),
        Some("reasoning") => text_of(it).map(|t| vec![Mapped::note("reasoning", t)]).unwrap_or_default(),
        Some("command_execution") => vec![Mapped::Event {
            kind: super::event::Kind::ToolCall,
            payload: cap(json!({
                "name": "shell",
                "id": id,
                "input": { "command": it.get("command").and_then(Value::as_str).unwrap_or_default() },
                "output": clamp(it.get("aggregated_output").and_then(Value::as_str).unwrap_or_default().to_string()),
                "exitCode": it.get("exit_code"),
                "status": it.get("status"),
            })),
        }],
        Some("file_change") => vec![Mapped::tool_call(
            "apply_patch",
            json!({ "changes": it.get("changes"), "status": it.get("status") }),
            id,
        )],
        Some("mcp_tool_call") => {
            let server = it.get("server").and_then(Value::as_str).unwrap_or("mcp");
            let tool = it.get("tool").and_then(Value::as_str).unwrap_or("tool");
            vec![Mapped::Event {
                kind: super::event::Kind::ToolCall,
                payload: cap(json!({
                    "name": format!("{server}/{tool}"),
                    "id": id,
                    "input": it.get("arguments").cloned().unwrap_or(Value::Null),
                    "status": it.get("status"),
                    "error": it.pointer("/error/message"),
                })),
            }]
        }
        Some("collab_tool_call") => vec![Mapped::spawn(
            it.get("tool").and_then(Value::as_str).unwrap_or("collab"),
            json!({
                "receivers": it.get("receiver_thread_ids"),
                "prompt": it.get("prompt"),
            }),
        )],
        Some("web_search") => vec![Mapped::tool_call(
            "web_search",
            json!({ "query": it.get("query") }),
            id,
        )],
        Some("todo_list") => vec![Mapped::note(
            "todo",
            it.get("items").map(|i| i.to_string()).unwrap_or_default(),
        )],
        Some("error") => text_or_message(it).map(|t| vec![Mapped::note("error", t)]).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn usage(u: &Value) -> Usage {
    Usage {
        input_tokens: u.get("input_tokens").and_then(Value::as_u64),
        output_tokens: u.get("output_tokens").and_then(Value::as_u64),
        cache_read_tokens: u.get("cached_input_tokens").and_then(Value::as_u64),
        cache_write_tokens: None,
        total_cost_usd: None,
        num_turns: None,
        duration_ms: None,
    }
}

fn text_of(it: &Value) -> Option<String> {
    it.get("text").and_then(Value::as_str).map(String::from).filter(|s| !s.trim().is_empty())
}

fn text_or_message(it: &Value) -> Option<String> {
    it.get("message")
        .or_else(|| it.get("text"))
        .and_then(Value::as_str)
        .map(String::from)
}

/// A codex `model_catalog_json` document declaring the gateway model's real
/// context window, so codex uses the true window instead of clamping a
/// custom/unknown model to its 272K fallback. `context_window` doubles as
/// `max_context_window` (codex clamps `model_context_window` to the latter, so the
/// ceiling must be raised here). Passed via `-c model_catalog_json=<file>`.
fn model_catalog(model: &str, context_window: u64) -> String {
    json!({
        "models": [{
            "slug": model,
            "context_window": context_window,
            "max_context_window": context_window,
        }]
    })
    .to_string()
}

/// A `-c key=value` override where `value` is a TOML string literal.
fn cfg_string(key: &str, val: &str) -> String {
    format!("{key}={}", toml_string(val))
}

/// A `-c key=value` override where `value` is a TOML array of strings.
fn cfg_array(key: &str, items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| toml_string(s)).collect();
    format!("{key}=[{}]", parts.join(","))
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code::backend::McpAttach;
    use crate::commands::code::event::Kind;

    fn spec(mode: Mode, api: &str) -> Spec {
        Spec {
            mode,
            task: Some("do it".into()),
            cwd: PathBuf::from("/tmp/proj"),
            routing: Route::Via(Routing::Gateway { api: api.into(), token: "JWT".into(), model: "enso".into(), small_fast_model: "enso-flash".into(), context_window: 1_000_000 }),
            // The default is auto-approve ON (the confirmed default).
            approval: Approval::Auto,
            mcp: Some(McpAttach { program: "hanzo-mcp".into(), args: vec!["--project-dir".into(), "/tmp/proj".into()] }),
            structured: true,
            preset_session: None,
            trust_project: false,
            resume: None,
            passthrough: vec![],
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
    fn headless_exec_json_with_native_provider_needs_only_token_env() {
        let l = Dev.build(&spec(Mode::Headless, "https://api.hanzo.ai")).unwrap();
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
        // The gateway model's real (1M) window is named via a model catalog (the
        // true-1M path), since the default window exceeds codex's 272K fallback.
        assert!(args.iter().any(|a| a.starts_with("model_catalog_json=")), "gateway route must name the model window: {args:?}");
        assert_eq!(l.cleanup.len(), 1, "the catalog temp file must outlive the child");
    }

    #[test]
    fn custom_network_api_defines_a_full_provider() {
        let l = Dev.build(&spec(Mode::Headless, "http://localhost:3690")).unwrap();
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
        let l = Dev.build(&s).unwrap();
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
        let l = Dev.build(&spec(Mode::Headless, "https://api.hanzo.ai")).unwrap();
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
            model: "enso".into(),
            small_fast_model: "enso-flash".into(),
            context_window: 1_000_000,
        });
        let l = Dev.build(&s).unwrap();
        assert!(argv(&l).iter().any(|a| a == r#"model="enso""#), "gateway route must honor an explicit model");
    }

    #[test]
    fn headless_resume_uses_exec_resume_with_sid_before_prompt() {
        let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
        s.resume = Some("thread-uuid".into());
        let l = Dev.build(&s).unwrap();
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
        let l = Dev.build(&s).unwrap();
        let args = argv(&l);
        assert_eq!(args[0], "resume");
        assert!(args.iter().any(|a| a == "tid"));
        assert!(!args.iter().any(|a| a == "exec"));
    }

    #[test]
    fn parse_thread_started_is_backend_session() {
        let out = Dev.parse(r#"{"type":"thread.started","thread_id":"th-1"}"#);
        assert!(matches!(&out[0], Mapped::BackendSession(s) if s == "th-1"));
    }

    #[test]
    fn parse_agent_message_and_command_execution() {
        let msg = Dev.parse(r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"hi"}}"#);
        assert!(matches!(&msg[0], Mapped::Event{kind:Kind::Message, payload} if payload["text"]=="hi"));

        let cmd = Dev.parse(r#"{"type":"item.completed","item":{"id":"i2","type":"command_execution","command":"ls -la","aggregated_output":"a\nb","exit_code":0,"status":"completed"}}"#);
        let Mapped::Event{kind, payload} = &cmd[0] else { panic!() };
        assert_eq!(*kind, Kind::ToolCall);
        assert_eq!(payload["name"], "shell");
        assert_eq!(payload["input"]["command"], "ls -la");
        assert_eq!(payload["exitCode"], 0);
    }

    #[test]
    fn parse_mcp_and_collab_and_turn_usage() {
        let mcp = Dev.parse(r#"{"type":"item.completed","item":{"id":"i","type":"mcp_tool_call","server":"hanzo","tool":"fs_read","arguments":{"path":"x"},"status":"completed"}}"#);
        assert!(matches!(&mcp[0], Mapped::Event{kind:Kind::ToolCall, payload} if payload["name"]=="hanzo/fs_read"));

        let collab = Dev.parse(r#"{"type":"item.completed","item":{"id":"i","type":"collab_tool_call","tool":"spawn","receiver_thread_ids":["t2"],"prompt":"go"}}"#);
        assert!(matches!(&collab[0], Mapped::Event{kind:Kind::Spawn, ..}));

        let turn = Dev.parse(r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":5,"reasoning_output_tokens":1}}"#);
        let Mapped::Usage(u) = &turn[0] else { panic!() };
        assert_eq!(u.input_tokens, Some(10));
        assert_eq!(u.output_tokens, Some(5));
        assert_eq!(u.cache_read_tokens, Some(2));
    }

    #[test]
    fn parse_turn_failed_and_error_are_terminal_not_ok() {
        assert!(matches!(
            Dev.parse(r#"{"type":"turn.failed","error":{"message":"boom"}}"#).last().unwrap(),
            Mapped::Terminal{ok:false, ..}
        ));
        assert!(matches!(
            Dev.parse(r#"{"type":"error","message":"fatal"}"#).last().unwrap(),
            Mapped::Terminal{ok:false, ..}
        ));
    }

    /// LOW-1 (dev mirror): `FailClosed` (a provider is SELECTED but no usable key)
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
        let l = Dev.build(&s).unwrap();
        for var in ["OPENAI_API_KEY", "OPENAI_BASE_URL", "HANZO_USER_KEY"] {
            assert!(cleared(&l, var), "{var} must be cleared under FailClosed");
        }

        let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
        s.routing = Route::Inherit;
        let l = Dev.build(&s).unwrap();
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
            argv(&Dev.build(&s).unwrap())
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

    /// The gateway route names the model's REAL window through a `model_catalog_json`
    /// (the true-1M path) whenever it exceeds codex's 272K fallback, declaring both
    /// `context_window` and `max_context_window` (codex clamps to the latter). A
    /// window at or below the fallback writes NO catalog — codex's own resolution
    /// stands and a temp file is never created.
    #[test]
    fn gateway_route_names_the_model_window_via_catalog() {
        // Default 1M window -> catalog written + declared 1M.
        let l = Dev.build(&spec(Mode::Headless, "https://api.hanzo.ai")).unwrap();
        let path = argv(&l)
            .into_iter()
            .find_map(|a| a.strip_prefix("model_catalog_json=").map(str::to_string))
            .expect("gateway route writes a model catalog");
        let body = std::fs::read_to_string(path.trim_matches('"')).expect("catalog file exists");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["models"][0]["slug"], "enso");
        assert_eq!(v["models"][0]["context_window"], 1_000_000);
        assert_eq!(v["models"][0]["max_context_window"], 1_000_000, "the ceiling must be raised or codex clamps");
        assert_eq!(l.cleanup.len(), 1);

        // A standard window (<= codex's fallback) writes NO catalog.
        let mut s = spec(Mode::Headless, "https://api.hanzo.ai");
        s.routing = Route::Via(Routing::Gateway { api: "https://api.hanzo.ai".into(), token: "JWT".into(), model: "enso".into(), small_fast_model: "enso-flash".into(), context_window: 200_000 });
        let l = Dev.build(&s).unwrap();
        assert!(!argv(&l).iter().any(|a| a.starts_with("model_catalog_json=")), "no catalog below codex's fallback");
        assert!(l.cleanup.is_empty());
    }
}
