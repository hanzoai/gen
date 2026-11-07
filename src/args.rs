//! The flags of a coding session: `gen`, `hanzo dev` and a bare `hanzo` all
//! flatten [`Args`] onto their own command, so every launcher takes exactly the
//! same flags and resolves them into one [`crate::Options`].

use anyhow::Result;

use crate::backend::{self, BackendKind};

/// The coding-session arguments — shared by `gen`, `hanzo dev` and a bare
/// `hanzo …` (each flattens them onto its own command), so all accept exactly
/// the same flags.
#[derive(clap::Args, Clone)]
pub struct Args {
    /// Coding backend: dev | claude | codex | cursor | agy | gemini | qwen | opencode | kilo | crush | pi | copilot | amp | grok (default: dev, our own agent). Each is also a flag: `hanzo --gemini`
    ///
    /// Equivalent to the tool's own flag (`hanzo dev --claude`). Every spelling
    /// resolves in ONE place — `commands::code::backend::select`.
    #[arg(long, value_name = "BACKEND", group = "backend_name")]
    pub backend: Option<String>,

    /// Use our own `dev` agent (same as `--backend dev`). This is the default.
    #[arg(long, group = "backend_name")]
    pub dev: bool,

    /// Use the `claude` backend (same as `--backend claude`).
    #[arg(long, group = "backend_name")]
    pub claude: bool,

    /// Use the `codex` backend (same as `--backend codex`).
    #[arg(long, group = "backend_name")]
    pub codex: bool,

    /// Use Antigravity (same as `--backend agy`).
    #[arg(long, group = "backend_name")]
    pub agy: bool,

    /// Use Cursor's agent (same as `--backend cursor`). `--agent` is the same flag.
    #[arg(long, visible_alias = "agent", group = "backend_name")]
    pub cursor: bool,

    /// Force streaming this session to Hanzo cloud (mission-control) on. Already
    /// the default for a signed-in run; `--link` only overrides a persisted
    /// `code.link = false`.
    #[arg(long)]
    pub link: bool,

    /// Never stream to cloud, even when signed in or `code.link = true`.
    #[arg(long)]
    pub no_link: bool,

    /// Run the session on Hanzo's metered cloud models (api.hanzo.ai) instead of
    /// the tool's own account. Without it `--claude`, `--codex`, `--agy` and
    /// `--cursor` talk straight to the account the tool is signed in to.
    #[arg(long)]
    pub cloud: bool,

    /// Do not attach the Hanzo MCP toolset.
    #[arg(long)]
    pub no_mcp: bool,

    /// Also load the repository's own `.mcp.json` MCP servers. Off by default: a
    /// repo is untrusted and any server it declares would run with your session's
    /// model key — only pass this for repos you trust.
    #[arg(long)]
    pub project_mcp: bool,

    /// Ask before each action instead of auto-approving it (`--safe` is an alias).
    /// Mutually exclusive with `--no-sandbox`.
    #[arg(long, visible_alias = "safe", conflicts_with = "no_sandbox")]
    pub ask: bool,

    /// Escalate PAST auto-approve to run unconfined on bare metal (runc / native host)
    /// with direct GPU pass-through, dropping the sandbox.
    #[arg(long, visible_alias = "runc", visible_alias = "bare")]
    pub no_sandbox: bool,

    /// Container / sandbox isolation runtime: `runc` (bare-metal native GPU), `microvm`, `container`, or `host`
    #[arg(long, value_name = "RUNTIME")]
    pub runtime: Option<String>,

    /// GPU allocation for high-performance agentic execution: `all` (default for runc), `cuda` (GB10), `rocm` (gfx1151), or `none`
    #[arg(long, value_name = "GPUS")]
    pub gpus: Option<String>,

    /// Resume a prior linked session by its cloud session id.
    #[arg(long, value_name = "SESSION_ID")]
    pub resume: Option<String>,

    /// Brand / tenant for auth: hanzo | lux | zoo | pars | bootnode
    #[arg(long, default_value_t = crate::host::BRAND.to_string())]
    pub brand: String,

    /// Claude theme to apply (Claude backend only), e.g. `dracula`. `--theme none`
    /// skips theming.
    #[arg(long)]
    pub theme: Option<String>,

    /// The gateway model to use, e.g. `enso-auto`, `enso-ultra`, `zen5-coder`. Applies
    /// on the metered Hanzo gateway route only; a direct provider key names its own
    /// model. No client-side allowlist — the gateway validates the id.
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// The task to run headless. Omit it for an interactive session.
    #[arg(value_name = "TASK")]
    pub positional: Option<String>,

    /// A second word; only a bare `hanzo <backend> "fix it"` binds it.
    #[arg(value_name = "TASK")]
    pub tail: Option<String>,

    /// Extra args passed verbatim to the backend (after `--`).
    #[arg(last = true, allow_hyphen_values = true)]
    pub passthrough: Vec<String>,
}

impl Args {
    /// Collapse the flag spellings and `--backend` into the one name the resolver
    /// reads. clap's `backend_name` group already guarantees at most one is set,
    /// so this chain never arbitrates — it only transcribes.
    pub fn named_backend(&self) -> Option<String> {
        if self.dev {
            Some("dev".into())
        } else if self.claude {
            Some("claude".into())
        } else if self.codex {
            Some("codex".into())
        } else if self.agy {
            Some("agy".into())
        } else if self.cursor {
            Some("cursor".into())
        } else {
            self.backend.clone()
        }
    }

    /// The args of a `dev` session (`gen`, `hanzo dev`): our own agent unless a
    /// flag names another tool, and the tool is a flag, never a positional, so one
    /// session has one spelling. `command` is how the caller is typed, for the
    /// message that says so.
    pub fn dev(mut self, command: &str) -> Result<Args> {
        if let Some(tool) = self.positional.as_deref().filter(|w| BackendKind::names(w)) {
            anyhow::bail!("name the tool with a flag: `{command} --{tool}`");
        }
        if self.named_backend().is_none() {
            self.dev = true;
        }
        Ok(self)
    }

    /// Map the parsed args to the code runner's [`Options`]. The `no_*` flags become
    /// their positive sense here, and the backend is resolved here — both in exactly
    /// ONE place, shared by `hanzo dev`, a bare `hanzo …` and `hanzo desktop`.
    pub fn into_options(self, root: crate::Root) -> Result<crate::Options> {
        let is_runc = self.no_sandbox
            || matches!(
                self.runtime.as_deref().map(str::to_lowercase).as_deref(),
                Some("runc" | "native" | "host" | "none" | "bare")
            );
        if let Some(gpu) = self.gpus.as_deref().map(str::to_lowercase) {
            match gpu.as_str() {
                "none" | "0" | "off" => {
                    std::env::set_var("CUDA_VISIBLE_DEVICES", "");
                    std::env::set_var("ROCR_VISIBLE_DEVICES", "");
                    std::env::set_var("HIP_VISIBLE_DEVICES", "");
                }
                "cuda" | "nvidia" => {
                    if std::env::var("CUDA_VISIBLE_DEVICES").unwrap_or_default().is_empty() {
                        std::env::set_var("CUDA_VISIBLE_DEVICES", "all");
                    }
                }
                "rocm" | "amd" if std::env::var("ROCR_VISIBLE_DEVICES").unwrap_or_default().is_empty() => {
                    std::env::set_var("ROCR_VISIBLE_DEVICES", "0");
                    std::env::set_var("HIP_VISIBLE_DEVICES", "0");
                }
                _ => {}
            }
        }
        let named = self.named_backend();
        // The reader's configured agent, if any. Loaded best-effort — the coding
        // agent must start even when `$HOME` is odd — so an unreadable settings
        // file leaves this None and the built-in default stands.
        let configured = crate::configured_agent(&root);
        let (backend, task) = backend::select(backend::Selection {
            positional: self.positional,
            tail: self.tail,
            named,
            configured,
        })?;
        Ok(crate::Options {
            backend,
            link: self.link,
            no_link: self.no_link,
            cloud: self.cloud,
            mcp: !self.no_mcp,
            project_mcp: self.project_mcp,
            ask: self.ask,
            no_sandbox: is_runc,
            resume: self.resume,
            brand: self.brand,
            theme: self.theme,
            model: self.model,
            task,
            passthrough: self.passthrough,
            root,
        })
    }
}


/// `--<agent>` for every agent of [`crate::agents`] that has no flag of its own:
/// it is `--backend <agent>`. One table names the agents, so a new row is a new
/// flag. `cmd` is the command [`Args`] is flattened onto, which knows the flags
/// that ARE declared; arguments after `--` belong to the backend and are left alone.
pub fn expand(cmd: &clap::Command, mut argv: Vec<String>) -> Vec<String> {
    let known = |name: &str| {
        cmd.get_arguments()
            .any(|a| a.get_long() == Some(name) || a.get_all_aliases().is_some_and(|al| al.contains(&name)))
    };
    let mut i = 1;
    while i < argv.len() {
        if argv[i] == "--" {
            break;
        }
        if let Some(name) = argv[i].strip_prefix("--") {
            if !known(name) && BackendKind::parse(name).is_ok() {
                let name = name.to_string();
                argv.splice(i..=i, ["--backend".to_string(), name]);
                i += 1;
            }
        }
        i += 1;
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches, Parser};

    #[derive(Parser)]
    struct Gen {
        #[command(flatten)]
        args: Args,
    }

    fn parse(argv: &[&str]) -> Result<Args> {
        let argv = expand(&Gen::command(), argv.iter().map(|s| s.to_string()).collect());
        let m = Gen::command().try_get_matches_from(argv)?;
        Gen::from_arg_matches(&m)?.args.dev("gen")
    }

    fn backend(argv: &[&str]) -> BackendKind {
        parse(argv).unwrap().into_options(crate::root::scratch()).unwrap().backend
    }

    /// `gen` is `hanzo dev`: our own agent by default, the tool a flag.
    #[test]
    fn gen_is_dev_and_the_tool_is_a_flag() {
        assert_eq!(backend(&["gen"]), BackendKind::Dev);
        assert_eq!(backend(&["gen", "fix the bug"]), BackendKind::Dev);
        assert_eq!(backend(&["gen", "--claude"]), BackendKind::Claude);
        assert_eq!(backend(&["gen", "--codex", "fix it"]), BackendKind::Codex);
        assert_eq!(backend(&["gen", "--backend", "cursor"]).as_str(), "cursor");
        let o = parse(&["gen", "fix the bug"]).unwrap().into_options(crate::root::scratch()).unwrap();
        assert_eq!(o.task.as_deref(), Some("fix the bug"));
        let err = parse(&["gen", "claude"]).err().expect("a tool named as a word is refused").to_string();
        assert!(err.contains("`gen --claude`"), "{err}");
    }

    /// Every agent without a flag of its own is one anyway.
    #[test]
    fn an_agent_with_no_declared_flag_is_still_a_flag() {
        for name in ["gemini", "qwen", "opencode", "kilo", "crush", "pi", "copilot", "amp", "grok"] {
            assert_eq!(backend(&["gen", &format!("--{name}")]).as_str(), name, "--{name}");
        }
        // After `--` an argument is the backend's, whatever it spells.
        let o = parse(&["gen", "--claude", "--", "--gemini"]).unwrap().into_options(crate::root::scratch()).unwrap();
        assert_eq!(o.backend, BackendKind::Claude);
        assert_eq!(o.passthrough, ["--gemini"]);
    }

    #[test]
    fn two_tools_are_refused() {
        assert!(parse(&["gen", "--claude", "--codex"]).is_err());
    }
}
