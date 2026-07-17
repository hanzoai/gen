//! The `dev` (Codex fork) backend.
//!
//! Headless runs stream the v2 `ThreadEvent` JSONL via `dev exec --json`; MCP is
//! attached with `-c mcp_servers.hanzo.*` overrides (ADDITIVE — the user's own
//! servers, config and `dev login` are untouched, since we never repoint
//! `CODEX_HOME`); model calls route through the native `hanzo` provider
//! (`api.hanzo.ai/v1`) with the bearer supplied as `HANZO_USER_KEY`. We NEVER
//! pass `--yolo` / `--dangerously-bypass-approvals-and-sandbox`; the user's
//! sandbox governs and widening flags arrive only via explicit passthrough.

use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::backend::{Backend, Launch, Mode, Routing, Spec};
use super::event::{cap, clamp, Mapped, Usage};

pub struct Dev;

/// The gateway origin the native `hanzo` provider already targets; when the
/// active network matches it we need no provider override, only the token env.
const NATIVE_API: &str = "https://api.hanzo.ai";

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
            // native gateway.
            Some(Routing::Gateway { api, token }) => {
                cmd.env("HANZO_USER_KEY", token);
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
            Some(Routing::OpenAI { key }) => {
                cmd.env("OPENAI_API_KEY", key);
            }
            // An Anthropic key cannot drive codex (OpenAI wire protocol) — the
            // resolver never pairs them, so this arm is unreachable.
            Some(Routing::Anthropic { .. }) | None => {}
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
        Ok(Launch { command: cmd, cleanup: Vec::new() })
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
            routing: Some(Routing::Gateway { api: api.into(), token: "JWT".into() }),
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
        // Never widen the sandbox.
        assert!(!args.iter().any(|a| a.contains("yolo") || a.contains("dangerously-bypass")));
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
        s.routing = Some(Routing::OpenAI { key: "sk-openai-SECRET".into() });
        let l = Dev.build(&s).unwrap();
        let args = argv(&l);
        assert_eq!(envmap(&l).get("OPENAI_API_KEY").map(String::as_str), Some("sk-openai-SECRET"));
        // Direct: no Hanzo gateway provider override, no HANZO_USER_KEY.
        assert!(!args.iter().any(|a| a.contains("model_provider")));
        assert!(!envmap(&l).contains_key("HANZO_USER_KEY"));
        assert!(!args.iter().any(|a| a.contains("sk-openai-SECRET")), "key must not be in argv");
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
}
