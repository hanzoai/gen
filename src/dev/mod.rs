//! Our own coding agent, `dev` (hanzoai/dev) — the DEFAULT backend — and
//! `codex`, which `dev` began as a fork of. Both take the same `-c` config
//! overrides and emit the same JSONL event stream, so one [`Agent`] driver runs
//! either; the program name is the only difference, and each reads its own
//! config home.
//!
//! Headless runs stream the v2 `ThreadEvent` JSONL via `<agent> exec --json`; MCP is
//! attached with `-c mcp_servers.hanzo.*` overrides, and the caller's cloud tools
//! with `-c mcp_servers.cloud.*` (ADDITIVE — the user's own
//! servers, config and `dev login` are untouched, since we never repoint
//! `CODEX_HOME`); model calls route through the native `hanzo` provider
//! (`api.hanzo.ai/v1`) with the bearer supplied as `HANZO_USER_KEY`.
//!
//! Auto-approve is ON by default (the confirmed default): `-c approval_policy="never"
//! -c sandbox_mode="workspace-write"` — it stops asking but KEEPS the workspace
//! sandbox. Each of the two is added only when the user's own `config.toml` does
//! not set it: their config wins unless they pass a flag. `--ask`/`--safe` (or `autoApprove: false`) hand back the user's own
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

use super::backend::{Approval, Backend, Launch, Mode, Remote, Route, Routing, Spec};
use crate::root::Root;
use super::event::{cap, clamp, Mapped, Usage};

/// The driver for the two agents that speak this protocol: our own `dev`
/// (hanzoai/dev) and `codex`, which ours began as a fork of. They take the same
/// `-c` config overrides and emit the same JSONL event stream, so ONE driver
/// serves both — they differ only in the program they exec and therefore in the
/// config home each reads. Adding a third program here is a const, not a file.
pub struct Agent {
    program: &'static str,
}

impl Agent {
    /// Our own coding agent — the default backend.
    pub const DEV: Agent = Agent { program: "dev" };
    /// `codex`, driven as its own backend. NOT an alias of `dev`: a user who
    /// names it gets that program, or a clear failure if it is not installed.
    pub const CODEX: Agent = Agent { program: "codex" };
    /// Open Interpreter, a Codex fork that kept its `exec`, `-c` and JSONL grammar.
    pub const INTERPRETER: Agent = Agent { program: "interpreter" };
}

/// The gateway origin the native `hanzo` provider already targets; when the
/// active network matches it we need no provider override, only the token env.
const NATIVE_API: &str = "https://api.hanzo.ai";

/// Codex's context-window fallback for an unknown model. A gateway model's window
/// is NAMED via a `model_catalog_json` only when it EXCEEDS this, so a run never
/// shrinks a model below codex's own default.
const CODEX_FALLBACK_WINDOW: u64 = 272_000;

impl Backend for Agent {
    fn label(&self) -> &'static str {
        self.program
    }

    fn version(&self, root: &Root) -> Option<String> {
        let prog = crate::exe::find_for_root(root, self.program)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| self.program.to_string());
        crate::exe::version(&prog, super::backend::VERSION_WAIT)
    }

    fn build(&self, spec: &Spec) -> Result<Launch> {
        let mut cmd = tokio::process::Command::from(crate::exe::command(
            crate::exe::find_for_root(&spec.root, self.program).unwrap_or_else(|| self.program.into()),
        ));
        cmd.current_dir(&spec.cwd);

        let mut args: Vec<String> = Vec::new();
        let mut cleanup = super::backend::Scratch::default();
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

        let readonly_task = headless
            && spec
                .task
                .as_deref()
                .is_some_and(crate::readonly::requests_readonly_or_confirmation);

        // Auto-approve. `Auto` (the default) stops asking but KEEPS the workspace
        // sandbox (or clamps to `read-only` when the prompt explicitly requests a
        // read-only scan / confirmation before editing); `Bypass` (`--no-sandbox`)
        // drops it entirely; `Ask` leaves the user's own mode.
        match spec.approval {
            Approval::Auto => args.extend(auto_overrides_for(
                user_config(&spec.root, self.program).as_ref(),
                readonly_task,
            )),
            Approval::Bypass => args.push("--dangerously-bypass-approvals-and-sandbox".into()),
            Approval::Ask => {}
        }

        // MCP: additive `-c` overrides (never touches the user's config/login).
        // `codex-rs` (`rmcp-client`) strips MCP stdio child environments down to
        // `DEFAULT_ENV_VARS` unless variable names are explicitly listed in
        // `mcp_servers.hanzo.env_vars`, so list the Hanzo auth/endpoint vars here
        // (names only — no secret ever enters argv).
        if let Some(mcp) = &spec.mcp {
            args.push("-c".into());
            args.push(cfg_string("mcp_servers.hanzo.command", &mcp.program));
            args.push("-c".into());
            args.push(cfg_array("mcp_servers.hanzo.args", &mcp.args));
            args.push("-c".into());
            args.push(cfg_array("mcp_servers.hanzo.env_vars", &mcp_env_vars()));
            if self.program == Agent::DEV.program {
                args.push("-c".into());
                args.push("mcp_servers.hanzo.required=true".into());
                args.push("-c".into());
                args.push(cfg_string("mcp_servers.hanzo.startup_readiness", "connection"));
            }
        }
        // The caller's cloud tools: a streamable-HTTP server whose bearer the agent
        // reads from the variable it is NAMED — the value never reaches argv.
        if let Some(remote) = spec.cloud() {
            let key = |field: &str| format!("mcp_servers.{}.{field}", Remote::NAME);
            cmd.env(Remote::TOKEN, &remote.token);
            args.push("-c".into());
            args.push(cfg_string(&key("url"), &remote.url));
            args.push("-c".into());
            args.push(cfg_string(&key("bearer_token_env_var"), Remote::TOKEN));
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
            Route::Via(Routing::Gateway { api, token, org, model, context_window, .. }) => {
                apply_gateway_mcp_env(
                    &mut cmd,
                    api,
                    token,
                    org.as_deref(),
                    inherited_hanzo_api_key().as_deref(),
                );
                args.push("-c".into());
                args.push(cfg_string("model", model));
                // Name the model's REAL window so the agent doesn't clamp a custom
                // id to its 272K fallback — the true-1M path. `dev` takes it as
                // its `model_context_window` setting; codex reads a
                // `model_catalog_json` (which we control), applied at startup via `-c`.
                if *context_window > CODEX_FALLBACK_WINDOW && self.program == Agent::DEV.program {
                    args.push("-c".into());
                    args.push(format!("model_context_window={context_window}"));
                } else if *context_window > CODEX_FALLBACK_WINDOW {
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
                    cleanup.files.push(path);
                }
                // Only `dev` ships a native `hanzo` provider. codex and Open Interpreter
                // are told about the gateway on every run, or they call api.openai.com.
                let trimmed = api.trim_end_matches('/');
                if trimmed != NATIVE_API || self.program != Agent::DEV.program {
                    let base = if trimmed.ends_with("/v1") || trimmed.contains("/v1/") {
                        trimmed.to_string()
                    } else {
                        format!("{trimmed}/v1")
                    };
                    args.push("-c".into());
                    args.push(cfg_string("model_providers.hanzocode.name", "Hanzo Code"));
                    args.push("-c".into());
                    args.push(cfg_string("model_providers.hanzocode.base_url", &base));
                    args.push("-c".into());
                    args.push(cfg_string("model_providers.hanzocode.wire_api", "responses"));
                    if let (true, Some((program, rest))) = (is_session_token(token), spec.refresh.split_first()) {
                        // A sign-in token ages out within the hour, so the agent asks
                        // the host for it again rather than reading it once.
                        push_auth_refresh(&mut args, "hanzocode", program, rest);
                    } else {
                        args.push("-c".into());
                        args.push(cfg_string("model_providers.hanzocode.env_key", "HANZO_USER_KEY"));
                    }
                    args.push("-c".into());
                    args.push(cfg_string("model_provider", "hanzocode"));
                } else if let (true, Some((program, rest))) = (is_session_token(token), spec.refresh.split_first()) {
                    // `dev` on https://api.hanzo.ai already defaults to `model_providers.hanzo`,
                    // whose built-in `auth.command` runs `dev token`. Point `hanzo.auth` at the
                    // Host's refresh command with a 30s timeout so multi-hour runs auto-refresh
                    // expiring 1h IAM JWTs for the active brand instead of 401-ing at 60m.
                    push_auth_refresh(&mut args, "hanzo", program, rest);
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
                cmd.env_remove("HANZO_API_KEY");
                cmd.env_remove("HANZO_API_URL");
                cmd.env_remove("HANZO_API_BASE");
                cmd.env_remove("HANZO_ORG");
            }
            // `--no-route` (or an unconfigured, signed-out run): dev uses its OWN
            // account (`dev login` / native provider). Leave inherited model-auth
            // exactly as the shell has it — the pass-through `--no-route` promises.
            Route::Inherit => {}
        }

        // A task run has nobody to answer, so it is told so, unless the caller named
        // developer instructions of their own or explicitly requested a read-only scan /
        // confirmation before modifying files.
        let own = spec.passthrough.iter().any(|a| a.starts_with("developer_instructions="));
        if headless && spec.task.is_some() && !own {
            let instructions = if readonly_task {
                crate::readonly::READ_ONLY_INSTRUCTIONS
            } else {
                NO_ONE_TO_ASK
            };
            args.push("-c".into());
            args.push(cfg_string("developer_instructions", instructions));
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

    fn transcript_path(&self, _root: &Root, _route: &Route, _cwd: &Path, _backend_session_id: &str) -> Option<PathBuf> {
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
            // The rest of the entry is REQUIRED by the agent's catalog parser —
            // it rejects the whole file on the first missing field, and that
            // took the SESSION down with it rather than merely losing the
            // window. These are inert defaults; only the two windows above
            // carry meaning for us. Unknown keys are ignored, so one superset
            // satisfies both agents' slightly different schemas.
            "display_name": model,
            "base_instructions": "",
            "supported_reasoning_levels": [],
            "supports_reasoning_summaries": false,
            "supports_parallel_tool_calls": false,
            "experimental_supported_tools": [],
            "support_verbosity": false,
            "shell_type": "default",
            "truncation_policy": { "mode": "tokens", "limit": context_window },
            "visibility": "list",
            "supported_in_api": true,
            "priority": 0,
        }]
    })
    .to_string()
}

/// How often the agent asks for the sign-in token again: well inside the hour
/// an IAM token lives.
const TOKEN_REFRESH_MS: u64 = 600_000;

/// Timeout for the external auth-refresh command (`hanzo auth token`), generous
/// enough to ride out transient IAM/network latency during multi-hour sessions.
const AUTH_TIMEOUT_MS: u64 = 30_000;

fn push_auth_refresh(args: &mut Vec<String>, provider: &str, program: &str, rest: &[String]) {
    args.push("-c".into());
    args.push(cfg_string(&format!("model_providers.{provider}.auth.command"), program));
    args.push("-c".into());
    args.push(cfg_array(&format!("model_providers.{provider}.auth.args"), rest));
    args.push("-c".into());
    args.push(format!("model_providers.{provider}.auth.refresh_interval_ms={TOKEN_REFRESH_MS}"));
    args.push("-c".into());
    args.push(format!("model_providers.{provider}.auth.timeout_ms={AUTH_TIMEOUT_MS}"));
}

/// An IAM sign-in token (a JWT, which ages out) rather than an API key.
fn is_session_token(value: &str) -> bool {
    value.starts_with("eyJ") && value.split('.').count() == 3
}

/// The auto-approve overrides, less any the user's own config already sets: the
/// `approval_policy` and `sandbox_mode` they chose there stand. A key counts as
/// set at the top level or in the profile the config selects.
#[cfg(test)]
fn auto_overrides(user: Option<&toml::Table>) -> Vec<String> {
    auto_overrides_for(user, false)
}

fn auto_overrides_for(user: Option<&toml::Table>, readonly: bool) -> Vec<String> {
    let sets = |key: &str| {
        let Some(user) = user else { return false };
        let profile = user
            .get("profile")
            .and_then(toml::Value::as_str)
            .and_then(|name| user.get("profiles")?.get(name));
        user.contains_key(key) || profile.is_some_and(|profile| profile.get(key).is_some())
    };
    let sandbox_mode = if readonly { "read-only" } else { "workspace-write" };
    let mut args = Vec::new();
    for (key, value) in [("approval_policy", "never"), ("sandbox_mode", sandbox_mode)] {
        if !sets(key) {
            args.push("-c".into());
            args.push(cfg_string(key, value));
        }
    }
    args
}

/// The agent's own `config.toml`, read from the home it reads: `DEV_HOME` (else
/// `~/.hanzo/dev`) for `dev`, `CODEX_HOME` (else `~/.codex`) for `codex`, and
/// `CODEX_HOME` (else `~/.openinterpreter`) for Open Interpreter.
#[cfg(not(test))]
fn user_config(root: &Root, program: &str) -> Option<toml::Table> {
    let (var, default) = match program {
        "dev" => ("DEV_HOME", ".hanzo/dev"),
        "interpreter" => ("CODEX_HOME", ".openinterpreter"),
        _ => ("CODEX_HOME", ".codex"),
    };
    let home = std::env::var_os(var)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| root.home().join(default));
    std::fs::read_to_string(home.join("config.toml")).ok()?.parse().ok()
}

/// Tests describe the user's config through [`auto_overrides`] directly, never
/// through the machine running them.
#[cfg(test)]
fn user_config(_root: &Root, _program: &str) -> Option<toml::Table> {
    None
}

/// Parent environment variables forwarded past `codex-rs` `rmcp-client`'s
/// `DEFAULT_ENV_VARS` filter into the `hanzo-mcp` stdio server.
const MCP_ENV_VARS: [&str; 6] = [
    "HANZO_USER_KEY",
    "HANZO_API_KEY",
    "HANZO_API_URL",
    "HANZO_API_BASE",
    "HANZO_ORG",
    "LOCAL_KAI_URL",
];

fn mcp_env_vars() -> Vec<String> {
    MCP_ENV_VARS.into_iter().map(String::from).collect()
}

pub(crate) fn apply_gateway_mcp_env(
    cmd: &mut tokio::process::Command,
    api: &str,
    token: &str,
    org: Option<&str>,
    existing_api_key: Option<&str>,
) {
    let clean_api = api.trim_end_matches('/');
    cmd.env("HANZO_USER_KEY", token);
    if existing_api_key.map(str::trim).filter(|s| !s.is_empty()).is_none() {
        cmd.env("HANZO_API_KEY", token);
    }
    cmd.env("HANZO_API_URL", clean_api);
    cmd.env("HANZO_API_BASE", clean_api);
    if let Some(o) = org.map(str::trim).filter(|s| !s.is_empty()) {
        cmd.env("HANZO_ORG", o);
    }
}

#[cfg(not(test))]
pub(crate) fn inherited_hanzo_api_key() -> Option<String> {
    std::env::var("HANZO_API_KEY").ok().filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
pub(crate) fn inherited_hanzo_api_key() -> Option<String> {
    None
}

fn cfg_string(key: &str, val: &str) -> String {
    format!("{key}={}", toml_string(val))
}

/// What a task run's agent is told: nobody is present to answer it, plus
/// long-horizon modularization and atomic git hygiene invariants.
pub(crate) const NO_ONE_TO_ASK: &str = "This run is non-interactive: nobody is present to answer a question or approve a step. \
Inspect the codebase, decide, execute the whole task with your tools, and verify before you end your turn. \
Do not stop to ask whether to continue, do not offer follow-up work, and do not announce a next step without taking it; \
end your turn only when the task is complete, with the result. \
Keep modules clean and <= 1,000 lines; split files into focused submodules before they grow unwieldy. \
When working in a git repository on multi-step changes, shape work into small, atomic `<type>(<scope>): <summary>` commits \
when appropriate rather than giant monolithic dumps, without force-pushing or discarding user work.";

/// A `-c key=value` override where `value` is a TOML array of strings.
fn cfg_array(key: &str, items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| toml_string(s)).collect();
    format!("{key}=[{}]", parts.join(","))
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests;
