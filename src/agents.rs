//! The coding agents `hanzo` launches beyond its own three (`dev`, `claude`,
//! `codex`): one row each, and adding an agent is adding a row.
//!
//! A row says what the agent is called, which binary runs it, where to install it
//! from, how it takes a task, how it is told to act without asking, how it
//! streams events, how the Hanzo toolset reaches it (`mcp`), and whether its
//! model calls can go through api.hanzo.ai (`gateway`). [`super::external`]
//! turns a row into a command line; nothing else reads the shapes.

/// How a task reaches the agent headless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    /// `<flag> TASK`, as in `-p TASK`.
    Flag(&'static str),
    /// `<sub> TASK`, as in `run TASK`.
    Sub(&'static str),
}

/// How the Hanzo MCP server is handed to the agent for one run, leaving the
/// person's own configuration as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mcp {
    /// The agent has no way to take a server for one run.
    None,
    /// `{"mcpServers": {...}}` in a file named after `flag`, `prefix` before the path.
    File { flag: &'static str, prefix: &'static str },
    /// `{"mcpServers": {...}}` as a settings file whose path is this variable.
    Settings(&'static str),
    /// `{"hanzo": {"command", "args"}}`, inline, after `flag`.
    Inline(&'static str),
    /// An opencode-shaped config document (`mcp`, and `provider` when routed) in this variable.
    Opencode(&'static str),
    /// crush's `crush.json`, in a data directory of Hanzo's own (`-D`).
    Crush,
}

/// How the agent's own model calls reach api.hanzo.ai under `--cloud`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gateway {
    /// Its model calls go to its own account only.
    None,
    /// `--auth-type openai --openai-base-url URL`, the key in `OPENAI_API_KEY`, `-m MODEL`.
    Qwen,
    /// A `hanzo` provider in the [`Mcp::Opencode`] / [`Mcp::Crush`] document.
    Config,
    /// `GROK_BASE_URL` and `GROK_API_KEY`, `-m MODEL`.
    Grok,
    /// A `hanzo` provider in `models.json` under Hanzo's own `PI_CODING_AGENT_DIR`.
    Pi,
}

/// One agent.
#[derive(Debug, PartialEq, Eq)]
pub struct Agent {
    /// The backend name: `hanzo --<name>`, `--backend <name>`.
    pub name: &'static str,
    /// Other spellings of the same agent.
    pub aliases: &'static [&'static str],
    pub title: &'static str,
    /// Binaries to try, in order.
    pub programs: &'static [&'static str],
    /// The npm package `hanzo upgrade <name>` installs, when it ships there.
    pub package: Option<&'static str>,
    pub prompt: Prompt,
    /// Act without asking (`--force`, `--yolo`, …).
    pub auto: &'static [&'static str],
    /// Structured events on stdout, for a run streamed to the cloud.
    pub stream: &'static [&'static str],
    pub mcp: Mcp,
    pub gateway: Gateway,
    /// Its own sign-in, when it has one `hanzo auth login` can drive.
    pub login: Option<&'static [&'static str]>,
}

/// Every agent, in the order help lists them.
pub const AGENTS: &[Agent] = &[
    Agent {
        name: "cursor",
        aliases: &["agent", "cursor-agent"],
        title: "Cursor",
        programs: &["agent", "cursor-agent"],
        package: None,
        prompt: Prompt::Flag("--print"),
        auto: &["--force"],
        stream: &["--output-format", "stream-json"],
        mcp: Mcp::None,
        gateway: Gateway::None,
        login: Some(&["login"]),
    },
    Agent {
        name: "agy",
        aliases: &["antigravity"],
        title: "Antigravity",
        programs: &["agy"],
        package: None,
        prompt: Prompt::Flag("--print"),
        auto: &["--dangerously-skip-permissions"],
        stream: &["--output-format", "stream-json"],
        mcp: Mcp::None,
        gateway: Gateway::None,
        login: None,
    },
    Agent {
        name: "gemini",
        aliases: &["gemini-cli"],
        title: "Gemini CLI",
        programs: &["gemini"],
        package: Some("@google/gemini-cli"),
        prompt: Prompt::Flag("-p"),
        auto: &["--yolo"],
        stream: &["--output-format", "stream-json"],
        mcp: Mcp::Settings("GEMINI_CLI_SYSTEM_SETTINGS_PATH"),
        gateway: Gateway::None,
        login: None,
    },
    Agent {
        name: "qwen",
        aliases: &["qwen-code"],
        title: "Qwen Code",
        programs: &["qwen"],
        package: Some("@qwen-code/qwen-code"),
        prompt: Prompt::Flag("-p"),
        auto: &["--yolo"],
        stream: &["--output-format", "stream-json"],
        mcp: Mcp::File { flag: "--mcp-config", prefix: "" },
        gateway: Gateway::Qwen,
        login: None,
    },
    Agent {
        name: "opencode",
        aliases: &[],
        title: "OpenCode",
        programs: &["opencode"],
        package: Some("opencode-ai"),
        prompt: Prompt::Sub("run"),
        auto: &["--auto"],
        stream: &["--format", "json"],
        mcp: Mcp::Opencode("OPENCODE_CONFIG_CONTENT"),
        gateway: Gateway::Config,
        login: Some(&["auth", "login"]),
    },
    Agent {
        name: "kilo",
        aliases: &["kilocode", "kilo-code"],
        title: "Kilo Code",
        programs: &["kilo", "kilocode"],
        package: Some("@kilocode/cli"),
        prompt: Prompt::Sub("run"),
        auto: &["--auto"],
        stream: &["--format", "json"],
        mcp: Mcp::Opencode("KILO_CONFIG_CONTENT"),
        gateway: Gateway::Config,
        login: Some(&["auth", "login"]),
    },
    Agent {
        name: "crush",
        aliases: &[],
        title: "Crush",
        programs: &["crush"],
        package: Some("@charmland/crush"),
        prompt: Prompt::Sub("run"),
        auto: &["--yolo"],
        stream: &[],
        mcp: Mcp::Crush,
        gateway: Gateway::Config,
        login: None,
    },
    Agent {
        name: "pi",
        aliases: &["pi-coding-agent"],
        title: "Pi",
        programs: &["pi"],
        package: Some("@mariozechner/pi-coding-agent"),
        prompt: Prompt::Flag("-p"),
        auto: &[],
        stream: &["--mode", "json"],
        mcp: Mcp::None,
        gateway: Gateway::Pi,
        login: None,
    },
    Agent {
        name: "copilot",
        aliases: &["github-copilot"],
        title: "GitHub Copilot CLI",
        programs: &["copilot"],
        package: Some("@github/copilot"),
        prompt: Prompt::Flag("-p"),
        auto: &["--allow-all-tools"],
        stream: &["--output-format", "json"],
        mcp: Mcp::File { flag: "--additional-mcp-config", prefix: "@" },
        gateway: Gateway::None,
        login: None,
    },
    Agent {
        name: "amp",
        aliases: &[],
        title: "Amp",
        programs: &["amp"],
        package: Some("@sourcegraph/amp"),
        prompt: Prompt::Flag("-x"),
        auto: &["--dangerously-allow-all"],
        stream: &["--stream-json"],
        mcp: Mcp::Inline("--mcp-config"),
        gateway: Gateway::None,
        login: Some(&["login"]),
    },
    Agent {
        name: "grok",
        aliases: &["grok-cli"],
        title: "Grok CLI",
        programs: &["grok"],
        package: Some("@vibe-kit/grok-cli"),
        prompt: Prompt::Flag("-p"),
        auto: &[],
        stream: &[],
        mcp: Mcp::None,
        gateway: Gateway::Grok,
        login: None,
    },
];

/// The agent a spelling names: its name or one of its aliases, any case.
pub fn find(name: &str) -> Option<&'static Agent> {
    let name = name.trim().to_ascii_lowercase();
    AGENTS.iter().find(|a| a.name == name || a.aliases.contains(&name.as_str()))
}

/// Where `hanzo upgrade` installs agents that ship on npm: `~/.hanzo`, so their
/// binaries land in `~/.hanzo/bin` and nothing global is touched.
pub fn prefix() -> Option<std::path::PathBuf> {
    Some(dirs::home_dir()?.join(".hanzo"))
}

/// The binary that runs `a`: the first of its programs on PATH, else in `~/.hanzo/bin`.
pub fn program(a: &Agent) -> Option<std::path::PathBuf> {
    a.programs.iter().find_map(|p| which::which(p).ok()).or_else(|| {
        let bin = prefix()?.join("bin");
        a.programs.iter().map(|p| bin.join(p)).find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spelling_is_one_agent_and_no_two_rows_share_one() {
        let mut seen = std::collections::HashSet::new();
        for a in AGENTS {
            for s in std::iter::once(&a.name).chain(a.aliases) {
                assert!(seen.insert(*s), "{s} names two agents");
                assert_eq!(find(&s.to_ascii_uppercase()).unwrap().name, a.name);
            }
            assert!(!a.programs.is_empty(), "{} runs nothing", a.name);
        }
        // Our own three are not rows: they have backends of their own.
        for own in ["dev", "claude", "codex"] {
            assert!(find(own).is_none(), "{own}");
        }
    }

    #[test]
    fn a_gateway_in_the_config_has_a_config_to_ride_in() {
        for a in AGENTS.iter().filter(|a| a.gateway == Gateway::Config) {
            assert!(matches!(a.mcp, Mcp::Opencode(_) | Mcp::Crush), "{}", a.name);
        }
    }
}
