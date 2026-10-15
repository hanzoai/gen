//! Every agent in [`super::agents::AGENTS`]: one backend, driven by the row.
//!
//! A session launches the agent's own binary and leaves its login alone. The
//! Hanzo toolset reaches it the way the row says (a file, a settings path, a
//! config document), written for this run only; under `--cloud` the agent's own
//! model calls go through api.hanzo.ai where the row says it can. A headless run
//! asks for structured events only when the run is streamed to the cloud, so a
//! plain `hanzo --<agent> "task"` prints what the agent prints.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::agents::{self, Agent, Gateway, Mcp, Prompt};
use super::backend::{Approval, Backend, Launch, McpAttach, Mode, Route, Routing, Spec};
use super::event::Mapped;

pub struct External {
    pub agent: &'static Agent,
}

/// The gateway's OpenAI-compatible base: `{api}/v1`.
fn v1(api: &str) -> String {
    format!("{}/v1", api.trim_end_matches('/'))
}

/// The variable a routed agent's config names for the bearer, so the key is never written down.
const KEY: &str = "HANZO_API_KEY";

/// `{"mcpServers": {"hanzo": …}}` as qwen and copilot read it: copilot needs
/// `type` and the tool allowlist, and qwen takes them.
fn servers(m: &McpAttach) -> Value {
    json!({ "mcpServers": { "hanzo": { "type": "stdio", "command": m.program, "args": m.args, "tools": ["*"] } } })
}

/// The bare server entry, for agents that refuse keys they do not know.
fn entry(m: &McpAttach) -> Value {
    json!({ "command": m.program, "args": m.args })
}

/// A file of this run's, removed when the session ends.
fn scratch(doc: &Value, cleanup: &mut Vec<tempfile::TempPath>) -> Result<PathBuf> {
    let mut f = tempfile::Builder::new().prefix("hanzo-agent-").suffix(".json").tempfile().context("creating a config file")?;
    f.write_all(doc.to_string().as_bytes()).context("writing a config file")?;
    let path = f.into_temp_path();
    let p = path.to_path_buf();
    cleanup.push(path);
    Ok(p)
}

/// A directory of Hanzo's own for an agent's state, `~/.hanzo/<name>`.
fn home(name: &str) -> Result<PathBuf> {
    let d = agents::prefix().context("no home directory")?.join(name);
    super::kit::private_dir(&d)?;
    Ok(d)
}

impl External {
    fn gateway<'a>(&self, spec: &'a Spec) -> Option<(&'a str, &'a str, &'a str)> {
        match &spec.routing {
            Route::Via(Routing::Gateway { api, token, model, .. }) if self.agent.gateway != Gateway::None => {
                Some((api.as_str(), token.as_str(), model.as_str()))
            }
            _ => None,
        }
    }

    /// The opencode document: the toolset, and the `hanzo` provider on a routed run.
    fn opencode(&self, spec: &Spec) -> Value {
        let mut doc = json!({});
        if let Some(m) = &spec.mcp {
            let mut command = vec![m.program.clone()];
            command.extend(m.args.iter().cloned());
            doc["mcp"] = json!({ "hanzo": { "type": "local", "command": command, "enabled": true } });
        }
        if let Some((api, _, model)) = self.gateway(spec) {
            doc["provider"] = json!({ "hanzo": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Hanzo",
                "options": { "baseURL": v1(api), "apiKey": format!("{{env:{KEY}}}") },
                "models": { model: { "name": model } },
            } });
            doc["model"] = json!(format!("hanzo/{model}"));
        }
        doc
    }

    /// crush's `crush.json`: the toolset, and the `hanzo` provider on a routed run.
    fn crush(&self, spec: &Spec) -> Value {
        let mut doc = json!({});
        if let Some(m) = &spec.mcp {
            doc["mcp"] = json!({ "hanzo": { "type": "stdio", "command": m.program, "args": m.args } });
        }
        if let Some((api, _, model)) = self.gateway(spec) {
            doc["providers"] = json!({ "hanzo": {
                "name": "Hanzo",
                "type": "openai-compat",
                "base_url": v1(api),
                "api_key": format!("${KEY}"),
                "models": [{ "id": model, "name": model }],
            } });
        }
        doc
    }
}

impl Backend for External {
    fn label(&self) -> &'static str {
        self.agent.name
    }

    fn version(&self) -> Option<String> {
        let p = agents::program(self.agent)?;
        super::backend::backend_version(&p.display().to_string())
    }

    fn build(&self, spec: &Spec) -> Result<Launch> {
        let a = self.agent;
        let program = agents::program(a).map(|p| p.display().to_string()).unwrap_or_else(|| a.programs[0].to_string());
        let mut cmd = tokio::process::Command::new(&program);
        cmd.current_dir(&spec.cwd);
        let mut cleanup = Vec::new();
        let auto = matches!(spec.approval, Approval::Auto | Approval::Bypass);
        let headless = spec.mode == Mode::Headless;

        // Flags that belong to the program rather than to its `run` subcommand.
        let mut root: Vec<String> = Vec::new();
        // Flags for the session itself, after the subcommand when there is one.
        let mut args: Vec<String> = Vec::new();

        match a.mcp {
            Mcp::None => {}
            Mcp::File { flag, prefix } => {
                if let Some(m) = &spec.mcp {
                    let p = scratch(&servers(m), &mut cleanup)?;
                    args.push(flag.to_string());
                    args.push(format!("{prefix}{}", p.display()));
                }
            }
            Mcp::Settings(var) => {
                if let Some(m) = &spec.mcp {
                    let p = scratch(&json!({ "mcpServers": { "hanzo": entry(m) } }), &mut cleanup)?;
                    cmd.env(var, p);
                }
            }
            Mcp::Extension(flag) => {
                if let Some(m) = &spec.mcp {
                    args.push(flag.to_string());
                    args.push(std::iter::once(&m.program).chain(&m.args).map(|w| shell_word(w)).collect::<Vec<_>>().join(" "));
                }
            }
            Mcp::Inline(flag) => {
                if let Some(m) = &spec.mcp {
                    args.push(flag.to_string());
                    args.push(json!({ "hanzo": entry(m) }).to_string());
                }
            }
            Mcp::Opencode(var) => {
                let doc = self.opencode(spec);
                if doc.as_object().is_some_and(|o| !o.is_empty()) {
                    cmd.env(var, doc.to_string());
                }
            }
            Mcp::Crush => {
                let dir = home("crush")?;
                crate::private::write(&dir.join("crush.json"), self.crush(spec).to_string().as_bytes())?;
                root.push("--data-dir".into());
                root.push(dir.display().to_string());
            }
        }

        if let Some((api, token, model)) = self.gateway(spec) {
            match a.gateway {
                Gateway::None => {}
                Gateway::Qwen => {
                    args.extend(["--auth-type".into(), "openai".into(), "--openai-base-url".into(), v1(api), "-m".into(), model.into()]);
                    cmd.env("OPENAI_API_KEY", token);
                }
                Gateway::Config => {
                    cmd.env(KEY, token);
                    if a.mcp == Mcp::Crush && headless {
                        args.extend(["-m".into(), format!("hanzo/{model}")]);
                    }
                }
                Gateway::Goose => {
                    cmd.env("OPENAI_HOST", api.trim_end_matches('/'))
                        .env("OPENAI_BASE_PATH", "v1/chat/completions")
                        .env("OPENAI_API_KEY", token);
                    args.extend(["--provider".into(), "openai".into(), "--model".into(), model.into()]);
                }
                Gateway::Hermes => {
                    let dir = home("hermes")?;
                    // JSON is YAML: the file Hermes reads, written without a YAML writer.
                    let mut doc = json!({
                        "model": { "provider": "hanzo", "default": model },
                        "providers": { "hanzo": { "base_url": v1(api), "key_env": KEY } },
                    });
                    if let Some(m) = &spec.mcp {
                        doc["mcp_servers"] = json!({ "hanzo": entry(m) });
                    }
                    crate::private::write(&dir.join("config.yaml"), doc.to_string().as_bytes())?;
                    cmd.env("HERMES_HOME", &dir).env(KEY, token);
                }
                Gateway::Aider => {
                    cmd.env("OPENAI_API_BASE", v1(api)).env("OPENAI_API_KEY", token);
                    // A gateway model is not in aider's own table; without this it warns, and
                    // `--yes-always` answers its offer to open the docs in a browser.
                    args.extend(["--model".into(), format!("openai/{model}"), "--no-show-model-warnings".into()]);
                }
                Gateway::Pi => {
                    let dir = home("pi")?;
                    let models = json!({ "providers": { "hanzo": {
                        "baseUrl": v1(api), "api": "openai-completions", "apiKey": KEY,
                        "models": [{ "id": model, "name": model }],
                    } } });
                    crate::private::write(&dir.join("models.json"), models.to_string().as_bytes())?;
                    cmd.env("PI_CODING_AGENT_DIR", &dir).env(KEY, token);
                    args.extend(["--provider".into(), "hanzo".into(), "--model".into(), model.into()]);
                }
            }
        }

        if auto && (a.auto_headless || !headless) {
            cmd.envs(a.auto_env.iter().copied());
            // crush's `--yolo` belongs to its screen, and `crush run` refuses it; every
            // other agent takes its own on the session.
            if a.mcp == Mcp::Crush {
                root.extend(a.auto.iter().map(|s| s.to_string()));
            } else if headless || matches!(a.prompt, Prompt::Flag(_) | Prompt::Positional) {
                args.extend(a.auto.iter().map(|s| s.to_string()));
            }
        }

        cmd.args(&root);
        if headless {
            let task = spec.task.as_deref().unwrap_or_default();
            if let Prompt::Sub { run, .. } = a.prompt {
                cmd.arg(run);
            }
            cmd.args(&args);
            if spec.structured {
                cmd.args(a.stream);
            }
            match a.prompt {
                Prompt::Flag(flag) | Prompt::Sub { flag: Some(flag), .. } => cmd.arg(flag).arg(task),
                Prompt::Sub { flag: None, .. } | Prompt::Positional => cmd.arg(task),
            };
        } else {
            if let Prompt::Sub { session: Some(session), .. } = a.prompt {
                cmd.arg(session);
            }
            cmd.args(&args);
        }
        cmd.args(&spec.passthrough);
        Ok(Launch { command: cmd, cleanup })
    }

    fn parse(&self, line: &str) -> Vec<Mapped> {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            return Vec::new();
        };
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => text_of(&v).map(|t| vec![Mapped::message("assistant", t)]).unwrap_or_default(),
            Some("result") => {
                let ok = !v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                vec![Mapped::Terminal { ok, summary: v.get("result").and_then(Value::as_str).map(String::from) }]
            }
            _ => Vec::new(),
        }
    }

    fn transcript_path(&self, _route: &Route, _cwd: &Path, _backend_session_id: &str) -> Option<PathBuf> {
        None
    }
}

/// `w` as one word of a command line: quoted when it holds anything a shell would split on.
fn shell_word(w: &str) -> String {
    if !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@+%,".contains(c)) {
        w.to_string()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

fn text_of(v: &Value) -> Option<String> {
    if let Some(t) = v.get("text").and_then(Value::as_str) {
        return Some(t.to_string());
    }
    v.pointer("/message/content").and_then(Value::as_array).and_then(|parts| {
        parts.iter().find_map(|p| {
            (p.get("type").and_then(Value::as_str) == Some("text")).then(|| p.get("text").and_then(Value::as_str).map(String::from)).flatten()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(mode: Mode, routing: Route) -> Spec {
        Spec {
            mode,
            task: Some("fix the test".into()),
            cwd: PathBuf::from("/tmp"),
            routing,
            approval: Approval::Auto,
            mcp: Some(McpAttach { program: "/opt/hanzo".into(), args: vec!["__mcp".into(), "--".into(), "hanzo-mcp".into()] }),
            remote: None,
            structured: false,
            preset_session: None,
            trust_project: false,
            resume: None,
            passthrough: vec![],
        }
    }

    fn gateway() -> Route {
        Route::Via(Routing::Gateway {
            api: "https://api.hanzo.ai".into(),
            token: "tok".into(),
            model: "enso".into(),
            small_fast_model: "enso-flash".into(),
            context_window: 1_000_000,
        })
    }

    fn argv(launch: &Launch) -> Vec<String> {
        launch.command.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    fn env(launch: &Launch, key: &str) -> Option<String> {
        launch.command.as_std().get_envs().find(|(k, _)| *k == key).and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    fn of(name: &str) -> External {
        External { agent: agents::find(name).unwrap() }
    }

    #[test]
    fn a_plain_headless_run_prints_what_the_agent_prints() {
        let l = of("cursor").build(&spec(Mode::Headless, Route::Inherit)).unwrap();
        assert_eq!(argv(&l), ["--force", "--print", "fix the test"]);
        let mut s = spec(Mode::Headless, Route::Inherit);
        s.structured = true;
        let l = of("cursor").build(&s).unwrap();
        assert_eq!(argv(&l), ["--force", "--output-format", "stream-json", "--print", "fix the test"]);
    }

    #[test]
    fn qwen_takes_the_toolset_by_flag_and_the_gateway_by_openai_auth() {
        let l = of("qwen").build(&spec(Mode::Headless, gateway())).unwrap();
        let a = argv(&l);
        assert_eq!(a[0], "--mcp-config");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&a[1]).unwrap()).unwrap();
        assert_eq!(doc["mcpServers"]["hanzo"]["command"], "/opt/hanzo");
        assert_eq!(a[2..], ["--auth-type", "openai", "--openai-base-url", "https://api.hanzo.ai/v1", "-m", "enso", "--yolo", "-p", "fix the test"]);
        assert_eq!(env(&l, "OPENAI_API_KEY").as_deref(), Some("tok"));
        // The key never reaches argv.
        assert!(!a.iter().any(|x| x.contains("tok")));
    }

    #[test]
    fn opencode_and_kilo_read_one_document_from_their_own_variable() {
        for (name, var) in [("opencode", "OPENCODE_CONFIG_CONTENT"), ("kilo", "KILO_CONFIG_CONTENT")] {
            let l = of(name).build(&spec(Mode::Headless, gateway())).unwrap();
            assert_eq!(argv(&l), ["run", "--auto", "fix the test"], "{name}");
            let doc: Value = serde_json::from_str(&env(&l, var).unwrap()).unwrap();
            assert_eq!(doc["mcp"]["hanzo"]["command"][0], "/opt/hanzo", "{name}");
            assert_eq!(doc["provider"]["hanzo"]["options"]["baseURL"], "https://api.hanzo.ai/v1", "{name}");
            assert_eq!(doc["provider"]["hanzo"]["options"]["apiKey"], "{env:HANZO_API_KEY}", "{name}");
            assert_eq!(doc["model"], "hanzo/enso", "{name}");
            assert_eq!(env(&l, "HANZO_API_KEY").as_deref(), Some("tok"));
            // On its own account there is no provider, only the toolset.
            let own = of(name).build(&spec(Mode::Headless, Route::Inherit)).unwrap();
            let doc: Value = serde_json::from_str(&env(&own, var).unwrap()).unwrap();
            assert!(doc.get("provider").is_none() && doc.get("mcp").is_some(), "{name}");
        }
    }

    #[test]
    fn gemini_reads_the_toolset_as_system_settings() {
        let l = of("gemini").build(&spec(Mode::Headless, gateway())).unwrap();
        assert_eq!(argv(&l), ["--yolo", "-p", "fix the test"]);
        let p = env(&l, "GEMINI_CLI_SYSTEM_SETTINGS_PATH").unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert_eq!(doc["mcpServers"]["hanzo"]["args"][0], "__mcp");
        // Gemini speaks only its own wire: no gateway.
        assert!(env(&l, "OPENAI_API_KEY").is_none());
    }

    #[test]
    fn amp_takes_the_bare_entry_inline() {
        let l = of("amp").build(&spec(Mode::Headless, Route::Inherit)).unwrap();
        let a = argv(&l);
        assert_eq!(a[0], "--mcp-config");
        let doc: Value = serde_json::from_str(&a[1]).unwrap();
        assert_eq!(doc, json!({ "hanzo": { "command": "/opt/hanzo", "args": ["__mcp", "--", "hanzo-mcp"] } }));
        assert_eq!(a[2..], ["--dangerously-allow-all", "-x", "fix the test"]);
    }

    #[test]
    fn goose_takes_the_toolset_as_an_extension_and_the_gateway_as_its_openai_provider() {
        let l = of("goose").build(&spec(Mode::Headless, gateway())).unwrap();
        assert_eq!(argv(&l), ["run", "--with-extension", "/opt/hanzo __mcp -- hanzo-mcp", "--provider", "openai", "--model", "enso", "-t", "fix the test"]);
        assert_eq!(env(&l, "OPENAI_HOST").as_deref(), Some("https://api.hanzo.ai"));
        assert_eq!(env(&l, "OPENAI_BASE_PATH").as_deref(), Some("v1/chat/completions"));
        assert_eq!(env(&l, "GOOSE_MODE").as_deref(), Some("auto"));
        // Interactive goose is `goose session`, which takes the same extension.
        let l = of("goose").build(&spec(Mode::Interactive, Route::Inherit)).unwrap();
        assert_eq!(argv(&l)[..2], ["session", "--with-extension"]);
    }

    #[test]
    fn aider_takes_the_gateway_as_an_openai_base_and_an_openai_model() {
        let l = of("aider").build(&spec(Mode::Headless, gateway())).unwrap();
        assert_eq!(argv(&l), ["--model", "openai/enso", "--no-show-model-warnings", "--yes-always", "--message", "fix the test"]);
        assert_eq!(env(&l, "OPENAI_API_BASE").as_deref(), Some("https://api.hanzo.ai/v1"));
    }

    #[test]
    fn hermes_on_hanzo_reads_the_provider_and_the_toolset_from_a_home_of_hanzos_own() {
        let l = of("hermes").build(&spec(Mode::Headless, gateway())).unwrap();
        assert_eq!(argv(&l), ["--yolo", "-z", "fix the test"]);
        let home = env(&l, "HERMES_HOME").unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(std::path::Path::new(&home).join("config.yaml")).unwrap()).unwrap();
        assert_eq!(doc["providers"]["hanzo"], json!({ "base_url": "https://api.hanzo.ai/v1", "key_env": "HANZO_API_KEY" }));
        assert_eq!(doc["model"]["default"], "enso");
        assert_eq!(doc["mcp_servers"]["hanzo"]["command"], "/opt/hanzo");
        assert_eq!(env(&l, "HANZO_API_KEY").as_deref(), Some("tok"));
        assert!(agents::find("hermes").unwrap().takes_mcp(true) && !agents::find("hermes").unwrap().takes_mcp(false));
        // On its own account Hermes keeps its own home.
        let own = of("hermes").build(&spec(Mode::Headless, Route::Inherit)).unwrap();
        assert!(env(&own, "HERMES_HOME").is_none());
    }

    #[test]
    fn kimi_acts_without_asking_on_its_screen_only() {
        assert_eq!(argv(&of("kimi").build(&spec(Mode::Headless, Route::Inherit)).unwrap()), ["-p", "fix the test"]);
        assert_eq!(argv(&of("kimi").build(&spec(Mode::Interactive, Route::Inherit)).unwrap()), ["--yolo"]);
    }

    #[test]
    fn cline_takes_its_task_as_its_first_argument() {
        let l = of("cline").build(&spec(Mode::Headless, Route::Inherit)).unwrap();
        assert_eq!(argv(&l), ["--yolo", "fix the test"]);
    }

    #[test]
    fn a_command_line_word_is_quoted_only_when_a_shell_would_split_it() {
        assert_eq!(shell_word("/opt/hanzo"), "/opt/hanzo");
        assert_eq!(shell_word("/my dir/x"), "'/my dir/x'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
    }

    #[test]
    fn crush_keeps_its_own_flags_ahead_of_run_and_yolo_on_its_screen() {
        let l = of("crush").build(&spec(Mode::Headless, gateway())).unwrap();
        let a = argv(&l);
        assert_eq!(a[0], "--data-dir");
        assert_eq!(a[2..], ["run", "-m", "hanzo/enso", "fix the test"]);
        let l = of("crush").build(&spec(Mode::Interactive, Route::Inherit)).unwrap();
        assert_eq!(argv(&l)[2..], ["--yolo"]);
    }

    #[test]
    fn an_interactive_run_has_no_task_and_no_subcommand() {
        let l = of("opencode").build(&spec(Mode::Interactive, Route::Inherit)).unwrap();
        assert!(argv(&l).is_empty());
        let l = of("gemini").build(&spec(Mode::Interactive, Route::Inherit)).unwrap();
        assert_eq!(argv(&l), ["--yolo"]);
    }
}
