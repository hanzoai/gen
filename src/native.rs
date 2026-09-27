//! Native sign-in for the four coding agents a person already uses.
//!
//! Each one has its own browser login. We do not reimplement that OAuth, and we
//! do not ask for an API key in its place: the official CLI opens the browser,
//! stores the session in its own credential store, and a later `hanzo` session
//! launches THAT agent so it finds the login itself.
//!
//! Antigravity is the one exception in shape. Current `agy` builds sign in on
//! the CLI's own first screen (Google OAuth) and have no `auth` subcommand.
//! Older builds advertise `agy auth login`. [`agy_login_args`] picks from the
//! help text so both work.

/// One coding agent that can be signed in to from `hanzo auth login`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeAgent {
    /// Canonical backend name (`BackendKind::as_str`).
    pub backend: &'static str,
    /// Extra spellings accepted by `--provider` and the backend resolver.
    pub aliases: &'static [&'static str],
    /// Menu title.
    pub title: &'static str,
    /// The line after the title in the picker.
    pub detail: &'static str,
    /// Binaries to try, in order. The first one on `PATH` is used.
    pub programs: &'static [&'static str],
    /// How that binary signs a person in.
    pub login: Login,
}

/// How to invoke the official sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Login {
    /// Fixed argv, as in `claude auth login` or `codex login`.
    Args(&'static [&'static str]),
    /// `agy`: `auth login` when the binary advertises it, otherwise launch `agy`
    /// itself — its first screen is the Google sign-in.
    Antigravity,
}

/// Claude, ChatGPT (Codex), Antigravity (`agy`), Cursor (`agent`).
pub const AGENTS: &[NativeAgent] = &[
    NativeAgent {
        backend: "claude",
        aliases: &["claude-code", "cc"],
        title: "Claude",
        detail: "Login with Claude",
        programs: &["claude"],
        login: Login::Args(&["auth", "login"]),
    },
    NativeAgent {
        backend: "codex",
        aliases: &["chatgpt", "openai-login"],
        title: "ChatGPT",
        detail: "Login with ChatGPT",
        programs: &["codex"],
        login: Login::Args(&["login"]),
    },
    NativeAgent {
        backend: "agy",
        aliases: &["antigravity"],
        title: "Antigravity",
        detail: "Login with Google",
        programs: &["agy"],
        login: Login::Antigravity,
    },
    NativeAgent {
        backend: "cursor",
        aliases: &["agent", "cursor-agent"],
        title: "Cursor",
        detail: "Login with Cursor",
        programs: &["agent", "cursor-agent"],
        login: Login::Args(&["login"]),
    },
];

/// The picker, Hanzo first and the paste-a-key escape at the end.
pub fn menu_lines() -> Vec<String> {
    let mut lines = vec![
        "Hanzo         unified billing, every model through the gateway  (recommended)".to_string(),
    ];
    for agent in AGENTS {
        lines.push(format!("{:<13} {}", agent.title, agent.detail));
    }
    lines.push("Paste key     paste an API key (hk- / sk-ant- / sk-)".to_string());
    lines
}

/// Resolve a `--provider` spelling to a native agent. `anthropic` and `openai`
/// stay API-key providers and are NOT here — a person who wants the browser
/// says `claude` or `chatgpt`.
pub fn find(name: &str) -> Option<&'static NativeAgent> {
    let name = name.trim().to_ascii_lowercase();
    AGENTS
        .iter()
        .find(|agent| agent.backend == name || agent.aliases.contains(&name.as_str()))
}

/// Argv for Antigravity given the text of `agy help`. A line whose first word
/// is `auth` means this build still has `agy auth login`. Otherwise the sign-in
/// is the CLI itself.
pub fn agy_login_args(help: &str) -> &'static [&'static str] {
    let advertises = help.lines().any(|line| {
        let word = line.split_whitespace().next().unwrap_or("");
        word == "auth"
    });
    if advertises {
        &["auth", "login"]
    } else {
        &[]
    }
}

/// The sentence printed before Antigravity is launched with no login subcommand,
/// because that launch IS the sign-in screen.
pub const AGY_LAUNCH_NOTE: &str =
    "Antigravity signs you in on its first screen (Google OAuth). Finish that, then quit agy — hanzo will use it from here.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_offers_hanzo_then_the_four_agents_then_a_key() {
        let lines = menu_lines();
        assert_eq!(lines.len(), 6);
        assert!(lines[0].starts_with("Hanzo"));
        assert!(lines[1].contains("Login with Claude"));
        assert!(lines[2].contains("Login with ChatGPT"));
        assert!(lines[3].contains("Login with Google"));
        assert!(lines[4].contains("Login with Cursor"));
        assert!(lines[5].starts_with("Paste key"));
    }

    #[test]
    fn names_resolve_to_the_agent_and_api_key_providers_do_not() {
        assert_eq!(find("claude").unwrap().backend, "claude");
        assert_eq!(find("ChatGPT").unwrap().backend, "codex");
        assert_eq!(find("antigravity").unwrap().backend, "agy");
        assert_eq!(find("agent").unwrap().backend, "cursor");
        assert_eq!(find("cursor-agent").unwrap().backend, "cursor");
        assert!(find("openai").is_none());
        assert!(find("anthropic").is_none());
        assert!(find("hanzo").is_none());
    }

    #[test]
    fn agy_uses_auth_login_only_when_the_binary_advertises_it() {
        let old = "Available subcommands:\n  auth           Sign in\n  help           Show help\n";
        assert_eq!(agy_login_args(old), &["auth", "login"]);
        let current = "Available subcommands:\n  help            Show help for subcommands\n  models          List available models\n";
        assert!(agy_login_args(current).is_empty());
    }
}
