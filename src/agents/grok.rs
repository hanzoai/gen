//! Grok Build, xAI's CLI. On its own account it keeps its xAI login and its own
//! home, takes no MCP server for one run and prints plain text. Under `--cloud`
//! it runs from a home of Hanzo's own (`GROK_HOME=~/.hanzo/grok`) whose
//! `config.toml` names the toolset, with its API-key sign-in pointed at
//! api.hanzo.ai: the key in `XAI_API_KEY`, the base in `GROK_XAI_API_BASE_URL`.

use anyhow::{Context, Result};
use serde_json::json;

use super::{wire, Agent, Run, Toolset, Upgrade};

pub const AGENT: Agent = Agent {
    name: "grok",
    aliases: &["grok-build", "xai"],
    title: "Grok Build",
    programs: &["grok"],
    upgrade: Upgrade::Own(&["update"]),
    routable: true,
    toolset: Toolset::Routed,
    build,
    read: wire::plain,
};

fn build(r: &mut Run) -> Result<()> {
    if let Some(g) = r.gateway() {
        let dir = r.home("grok")?;
        let mut doc = json!({});
        if let Some(m) = r.mcp() {
            doc["mcp_servers"] = json!({ "hanzo": { "command": m.program, "args": m.args } });
        }
        let toml = toml::to_string(&doc).context("writing Grok's config")?;
        crate::private::write(&dir.join("config.toml"), toml.as_bytes()).context("writing Grok's config")?;
        r.env("GROK_HOME", &dir).env("XAI_API_KEY", g.token).env("GROK_XAI_API_BASE_URL", g.v1());
        r.arg("-m").arg(g.model);
    }
    if r.auto() {
        r.arg("--always-approve");
    }
    if r.headless() {
        let task = r.task();
        r.arg("-p").arg(task);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};

    #[test]
    fn grok_takes_its_task_by_p() {
        assert_eq!(argv(&build("grok", &spec(Mode::Headless, Route::Inherit))), ["--always-approve", "-p", "fix the test"]);
    }

    #[test]
    fn grok_on_hanzo_reads_the_toolset_from_a_home_of_hanzos_own() {
        let l = build("grok", &spec(Mode::Headless, gateway()));
        assert_eq!(argv(&l), ["-m", "enso", "--always-approve", "-p", "fix the test"]);
        let home = env(&l, "GROK_HOME").unwrap();
        let doc: toml::Value = std::fs::read_to_string(std::path::Path::new(&home).join("config.toml")).unwrap().parse().unwrap();
        assert_eq!(doc["mcp_servers"]["hanzo"]["command"].as_str(), Some("/opt/hanzo"));
        assert_eq!(env(&l, "XAI_API_KEY").as_deref(), Some("tok"));
        assert_eq!(env(&l, "GROK_XAI_API_BASE_URL").as_deref(), Some("https://api.hanzo.ai/v1"));
        // On its own account Grok keeps its own home and login.
        let own = build("grok", &spec(Mode::Headless, Route::Inherit));
        assert!(env(&own, "GROK_HOME").is_none() && env(&own, "XAI_API_KEY").is_none());
    }
}
