//! The Claude Code backend.
//!
//! Headless runs stream JSONL via `-p … --output-format stream-json --verbose`;
//! MCP is layered with `--mcp-config` (Hanzo's server and the caller's cloud tools
//! added on top, the repo's
//! own `.mcp.json` only under `--trust-project`); settings come from the USER
//! scope only (`--setting-sources user`) unless the repo is trusted, so a
//! hostile repo's `.claude/settings*.json` hooks / statusLine / plugins never
//! auto-run against our env; model calls route through the Hanzo gateway via
//! `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` (Bearer).
//!
//! Auto-approve is ON by default (`--dangerously-skip-permissions`), the confirmed
//! default; `--ask`/`--safe` (or `autoApprove: false`) opt out and hand back the
//! user's own permission mode. The trust gate is INDEPENDENT of and unweakened by
//! this: `--strict-mcp-config` + `--setting-sources user` still keep the repo's own
//! `.mcp.json` and settings/hooks out of the process env, so auto-approve never
//! reopens the routing-bearer exfil vector.
//!
//! On the gateway route the model reaches Claude Code as a CARRIER — a `claude-*`
//! id it recognizes — so the real (1M) window is granted instead of clamped, and
//! a per-run `modelOverrides` overlay rewrites the carrier back to the zen id before
//! the request leaves the process. The whole mapping lives in [`super::tier`]; a
//! model outside it passes through with the bare-id + `[1m]` behavior. A ROUTED
//! session runs in Hanzo's OWN config home (`~/.hanzo/claude`); `--no-route` keeps
//! the user's `~/.claude`, because that is where the account it inherits lives (see
//! [`super::home`]). Extra flags arrive only through passthrough.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::backend::{Approval, Backend, Launch, McpAttach, Mode, Remote, Route, Routing, Spec};
use crate::root::Root;
use super::event::{Mapped, Usage};
use super::{home, tier};

pub struct Claude;

impl Backend for Claude {
    fn label(&self) -> &'static str {
        "claude"
    }

    fn version(&self, root: &Root) -> Option<String> {
        let prog = crate::exe::find_for_root(root, "claude")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "claude".to_string());
        crate::exe::version(&prog, super::backend::VERSION_WAIT)
    }

    fn build(&self, spec: &Spec) -> Result<Launch> {
        let mut cmd = tokio::process::Command::from(crate::exe::command(
            crate::exe::find_for_root(&spec.root, "claude").unwrap_or_else(|| "claude".into()),
        ));
        cmd.current_dir(&spec.cwd);
        let mut cleanup = super::backend::Scratch::default();

        // A ROUTED `hanzo dev --claude` is not the user's own Claude Code install: it
        // runs against its own config home, so their saved `/model` never becomes
        // this session's identity (it OUTRANKS `ANTHROPIC_MODEL`) and Hanzo's
        // injected tiers never leak back into their sessions. `--no-route` keeps
        // Claude's own home, because that is where the account it was promised
        // lives. Seeded by `run`; resolved here too because it is a pure function of
        // the route (see [`home::relocate`]).
        if let Some(dir) = home::relocate(&spec.root, &spec.routing) {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }

        // Task (headless). The structured stream is requested ONLY when we stream
        // to cloud; otherwise the run keeps Claude's native output untouched.
        if spec.mode == Mode::Headless {
            let task = spec.task.as_deref().unwrap_or_default();
            cmd.arg("-p").arg(task);
            if spec.structured {
                cmd.args(["--output-format", "stream-json", "--verbose"]);
            }
        }

        // Native resume against the backend's own session id, else optionally
        // pre-set the session id so a linked interactive run can tail its
        // transcript. `--resume` and `--session-id` are mutually exclusive.
        if let Some(sid) = &spec.resume {
            cmd.arg("--resume").arg(sid);
        } else if let Some(sid) = &spec.preset_session {
            cmd.arg("--session-id").arg(sid);
        }

        // Settings come from the USER scope only by default. Claude otherwise
        // auto-loads the repository's own `<cwd>/.claude/settings.json` and
        // `settings.local.json`, and in headless `-p` mode the workspace-trust
        // dialog is skipped — so a hostile repo's `SessionStart`/`UserPromptSubmit`
        // hook (or a `statusLine` command, or a project plugin) would auto-run a
        // shell command that inherits this process's env, where the model routing
        // bearer lives (`ANTHROPIC_AUTH_TOKEN` below). `--strict-mcp-config` scopes
        // only MCP, NOT settings, so `--setting-sources user` is the control that
        // stops repo settings/hooks/statusLine/plugins from loading. The repo's
        // project + local settings apply ONLY under the explicit `--trust-project`
        // opt-in — the SAME trust boundary that loads its `.mcp.json`.
        if spec.trust_project {
            cmd.args(["--setting-sources", "user,project,local"]);
        } else {
            cmd.args(["--setting-sources", "user"]);
        }

        // MCP is EXPLICIT. `--strict-mcp-config` makes Claude use ONLY the
        // servers we pass here and ignore every auto-discovered source — most
        // importantly the repository's own `<cwd>/.mcp.json`. Model calls route
        // with the session's key on this process's env, and any stdio MCP server
        // inherits that env, so an untrusted repo must never get to declare one.
        // The Hanzo toolset is layered by default; the repo's own `.mcp.json` is
        // loaded ONLY when the user explicitly trusts it with `--trust-project`.
        cmd.arg("--strict-mcp-config");
        if spec.trust_project {
            let project_cfg = spec.cwd.join(".mcp.json");
            if project_cfg.is_file() {
                cmd.arg("--mcp-config").arg(&project_cfg);
            }
        }
        if let Some(doc) = mcp_config(spec.mcp.as_ref(), spec.cloud()) {
            let mut file = tempfile::Builder::new()
                .prefix("hanzo-mcp-")
                .suffix(".json")
                .tempfile()
                .context("creating mcp-config temp file")?;
            file.write_all(doc.as_bytes())
                .context("writing mcp-config")?;
            let path = file.into_temp_path();
            cmd.arg("--mcp-config").arg(&*path);
            cleanup.files.push(path);
        }
        // The cloud server's bearer: in the env, which the document above NAMES.
        if let Some(remote) = spec.cloud() {
            cmd.env(Remote::TOKEN, &remote.token);
        }

        // Route model calls (credential via env, never argv). In every routed
        // branch we make OUR credential the SOLE one in the child: a stray
        // `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_BASE_URL` inherited
        // from the shell would otherwise win Claude's auth precedence — shadowing
        // the intended route, or worse redirecting prompts+code to an
        // attacker-set base URL and leaking the user's own key. So each branch
        // sets exactly what it needs and CLEARS the rest.
        match &spec.routing {
            // Gateway: Bearer + our base URL; clear the api-key so the Bearer is
            // unambiguous. Name the model too — Claude's built-in default
            // (`claude-fable-5`) is not in the gateway catalog and would 400, so
            // the routing decision already resolved a valid catalog id (`--model`
            // > exported `ANTHROPIC_MODEL` > `~/.hanzo/settings.json` > built-in
            // default `enso`). Setting it back to the user's own exported value is a
            // deliberate no-op; a `--model` overrides it, exactly the intended precedence.
            Route::Via(Routing::Gateway { api, token, org, model, small_fast_model, context_window }) => {
                cmd.env("ANTHROPIC_BASE_URL", api.trim_end_matches('/'));
                cmd.env("ANTHROPIC_AUTH_TOKEN", token);
                super::dev::apply_gateway_mcp_env(
                    &mut cmd,
                    api,
                    token,
                    org.as_deref(),
                    super::dev::inherited_hanzo_api_key().as_deref(),
                );
                // The model, every tiering slot Claude resolves subagents/`/compact`
                // through, and the picker's labels — one pure map from `tier`, so
                // carrier mapping and slot precedence have exactly one home.
                cmd.envs(tier::env(model, small_fast_model, *context_window));
                // `ANTHROPIC_SMALL_FAST_MODEL` is deprecated AND not rewritten by
                // `modelOverrides`, so it could only ever carry a stale shell value
                // past the mapping — clear it and let the HAIKU slot answer.
                cmd.env_remove("ANTHROPIC_SMALL_FAST_MODEL");
                // Surface the gateway's own catalog in `/model` (harmless when the
                // gateway serves no `claude-*` ids; future-proof when it does).
                cmd.env("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1");
                // MCP tool definitions are read when a tool is searched for, not in every
                // request: the toolset costs an agent that never calls it nothing, and the
                // prompt the provider caches stays the size of the agent's own. Claude's tool
                // search speaks the Anthropic wire, so it is asked for only on a model that does.
                if (spec.mcp.is_some() || spec.cloud().is_some()) && lazy_tools(model) {
                    cmd.env("ENABLE_TOOL_SEARCH", "true");
                }
                cmd.env_remove("ANTHROPIC_API_KEY");
                // The carrier⇒zen map, as a per-RUN settings overlay. Claude budgets
                // the context window from the carrier and sends the zen id, so the
                // gateway is served what it actually has. It rides the run and not
                // the config home because it is true only of THIS route: persisted,
                // it would rewrite a `--no-route` or direct-Anthropic session's model
                // to a zen id and 404 against api.anthropic.com. Model ids only — no
                // secret ever reaches argv.
                cmd.arg("--settings")
                    .arg(json!({ "modelOverrides": tier::overrides() }).to_string());
                // Correct the identity the CARRIER borrows. Claude Code is handed a
                // `claude-*` id to budget from, so its base prompt has the model
                // introduce itself as that model. An APPEND (never `--system-prompt`)
                // keeps the harness's own tool-use/safety/coding prompt intact and
                // rewrites only who it says it is. Applies in `--ask` too: identity is
                // not a permission bypass.
                if let Some(line) = tier::identity(model) {
                    cmd.arg("--append-system-prompt").arg(line);
                }
            }
            // Direct Anthropic: the user's own key on the default endpoint
            // (api.anthropic.com). Clear BASE_URL + AUTH_TOKEN so nothing redirects
            // the key or shadows it.
            Route::Via(Routing::Anthropic { key }) => {
                cmd.env("ANTHROPIC_API_KEY", key);
                cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
                cmd.env_remove("ANTHROPIC_BASE_URL");
            }
            // We hold NOTHING Claude can use — either an OpenAI key (the resolver
            // never pairs it with Claude) or `FailClosed` (a provider was SELECTED
            // but no usable credential resolved). FAIL CLOSED: clear all three so a
            // stray shell `ANTHROPIC_*` can't silently drive the child to an
            // inherited endpoint. The caller has already warned the user.
            Route::Via(Routing::OpenAI { .. }) | Route::FailClosed => {
                cmd.env_remove("ANTHROPIC_API_KEY");
                cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
                cmd.env_remove("ANTHROPIC_BASE_URL");
                cmd.env_remove("HANZO_USER_KEY");
                cmd.env_remove("HANZO_API_KEY");
                cmd.env_remove("HANZO_API_URL");
                cmd.env_remove("HANZO_API_BASE");
                cmd.env_remove("HANZO_ORG");
            }
            // `--no-route` (or an unconfigured, signed-out run): Claude uses its
            // OWN account. Leave the child's inherited model-auth exactly as the
            // shell has it — the deliberate pass-through `--no-route` promises.
            Route::Inherit => {}
        }

        // Auto-approve → skip the per-action permission prompt. Claude has no
        // separate sandbox layer, so `Auto` and `Bypass` are identical here;
        // `Ask` leaves the user's own permission mode untouched. Orthogonal to the
        // trust gate above — this widens PERMISSION, never which settings/MCP load.
        if matches!(spec.approval, Approval::Auto | Approval::Bypass) {
            cmd.arg("--dangerously-skip-permissions");
        }

        cmd.args(&spec.passthrough);
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
            Some("system") => system_event(&v),
            // Complete-message objects (stream-json) AND transcript entries share
            // this shape, so one branch serves headless stdout and interactive tail.
            Some("assistant") => role_message("assistant", &v),
            Some("user") => role_message("user", &v),
            Some("result") => result_event(&v),
            _ => Vec::new(),
        }
    }

    fn transcript_path(&self, root: &Root, route: &Route, cwd: &Path, backend_session_id: &str) -> Option<PathBuf> {
        // Claude writes transcripts under `$CLAUDE_CONFIG_DIR/projects/`, so this
        // must read the SAME home the launch sets — otherwise the cloud pointer and
        // the interactive tail both name a file that never exists.
        Some(home::transcript(root, route, cwd, backend_session_id))
    }
}

/// The `--mcp-config` document: Hanzo's stdio server and the caller's cloud tools
/// over HTTP, whichever this run carries, in ONE file (Claude requires an explicit
/// `type`). The cloud server's header is `${HANZO_MCP_TOKEN}`, which Claude expands
/// from its environment, so the bearer is never written to disk. `None` when there
/// is neither.
fn mcp_config(mcp: Option<&McpAttach>, remote: Option<&Remote>) -> Option<String> {
    let mut servers = serde_json::Map::new();
    if let Some(mcp) = mcp {
        servers.insert(
            "hanzo".into(),
            json!({ "type": "stdio", "command": mcp.program, "args": mcp.args, "env": {} }),
        );
    }
    if let Some(remote) = remote {
        servers.insert(
            Remote::NAME.into(),
            json!({
                "type": "http",
                "url": remote.url,
                "headers": { "Authorization": format!("Bearer ${{{}}}", Remote::TOKEN) },
            }),
        );
    }
    (!servers.is_empty()).then(|| json!({ "mcpServers": servers }).to_string())
}

/// Whether `model` is served on the Anthropic wire, which Claude's tool search needs.
fn lazy_tools(model: &str) -> bool {
    model.starts_with("anthropic/") || model.starts_with("claude-")
}

fn system_event(v: &Value) -> Vec<Mapped> {
    if v.get("subtype").and_then(Value::as_str) != Some("init") {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
        out.push(Mapped::BackendSession(sid.to_string()));
    }
    if let Some(model) = v.get("model").and_then(Value::as_str) {
        out.push(Mapped::note("session-start", format!("model {model}")));
    }
    out
}

/// Map an assistant/user message's content blocks. `Task` tool uses become
/// spawn events (subagent flow); everything else is a tool call or a message.
fn role_message(role: &str, v: &Value) -> Vec<Mapped> {
    let content = v.pointer("/message/content");
    let mut out = Vec::new();
    match content {
        Some(Value::Array(blocks)) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            if !t.trim().is_empty() {
                                out.push(Mapped::message(role, t));
                            }
                        }
                    }
                    Some("tool_use") => {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let input = b.get("input").cloned().unwrap_or(Value::Null);
                        let id = b.get("id").and_then(Value::as_str);
                        if name == "Task" {
                            out.push(Mapped::spawn(name, input));
                        } else {
                            out.push(Mapped::tool_call(name, input, id));
                        }
                    }
                    Some("tool_result") => {
                        let id = b.get("tool_use_id").and_then(Value::as_str);
                        let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        out.push(Mapped::tool_result(id, stringify_content(b.get("content")), is_error));
                    }
                    _ => {}
                }
            }
        }
        Some(Value::String(s)) if !s.trim().is_empty() => {
            out.push(Mapped::message(role, s.clone()));
        }
        _ => {}
    }
    out
}

fn result_event(v: &Value) -> Vec<Mapped> {
    let mut out = Vec::new();
    let u = v.get("usage");
    let usage = Usage {
        input_tokens: u.and_then(|u| u.get("input_tokens")).and_then(Value::as_u64),
        output_tokens: u.and_then(|u| u.get("output_tokens")).and_then(Value::as_u64),
        cache_read_tokens: u
            .and_then(|u| u.get("cache_read_input_tokens"))
            .and_then(Value::as_u64),
        cache_write_tokens: u
            .and_then(|u| u.get("cache_creation_input_tokens"))
            .and_then(Value::as_u64),
        total_cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
        num_turns: v.get("num_turns").and_then(Value::as_u64),
        duration_ms: v.get("duration_ms").and_then(Value::as_u64),
    };
    if !usage.is_empty() {
        out.push(Mapped::Usage(usage));
    }
    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let ok = v.get("subtype").and_then(Value::as_str) == Some("success") && !is_error;
    let summary = v.get("result").and_then(Value::as_str).map(|s| s.to_string());
    out.push(Mapped::Terminal { ok, summary });
    out
}

/// A tool_result's `content` may be a string or an array of blocks; render a
/// compact string either way.
fn stringify_content(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|it| it.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests;
