//! Hermes Agent. It reads its provider and its MCP servers only from its home,
//! so under `--cloud` it runs from a home of Hanzo's own
//! (`HERMES_HOME=~/.hanzo/hermes`) whose `config.yaml` names a `hanzo` provider
//! (the key in `HANZO_API_KEY`, by name) and the toolset. On its own account it
//! keeps its own home and takes no toolset. It prints plain text.

use anyhow::Result;
use serde_json::json;

use super::{wire, Agent, Run, Toolset, KEY};

pub const AGENT: Agent = Agent {
    name: "hermes",
    aliases: &["hermes-agent"],
    title: "Hermes Agent",
    programs: &["hermes"],
    package: None,
    routable: true,
    toolset: Toolset::Routed,
    build,
    read: wire::plain,
};

fn build(r: &mut Run) -> Result<()> {
    if let Some(g) = r.gateway() {
        let dir = r.home("hermes")?;
        // JSON is YAML: the file Hermes reads, written without a YAML writer.
        let mut doc = json!({
            "model": { "provider": "hanzo", "default": g.model },
            "providers": { "hanzo": { "base_url": g.v1(), "key_env": KEY } },
        });
        if let Some(m) = r.mcp() {
            doc["mcp_servers"] = json!({ "hanzo": { "command": m.program, "args": m.args } });
        }
        r.write(&dir.join("config.yaml"), &doc)?;
        r.env("HERMES_HOME", &dir).env(KEY, g.token);
    }
    if r.auto() {
        r.arg("--yolo");
    }
    if r.headless() {
        let task = r.task();
        r.arg("-z").arg(task);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};
    use serde_json::{json, Value};

    #[test]
    fn hermes_on_hanzo_reads_the_provider_and_the_toolset_from_a_home_of_hanzos_own() {
        let l = build("hermes", &spec(Mode::Headless, gateway()));
        assert_eq!(argv(&l), ["--yolo", "-z", "fix the test"]);
        let home = env(&l, "HERMES_HOME").unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(std::path::Path::new(&home).join("config.yaml")).unwrap()).unwrap();
        assert_eq!(doc["providers"]["hanzo"], json!({ "base_url": "https://api.hanzo.ai/v1", "key_env": "HANZO_API_KEY" }));
        assert_eq!(doc["model"]["default"], "enso");
        assert_eq!(doc["mcp_servers"]["hanzo"]["command"], "/opt/hanzo");
        assert_eq!(env(&l, "HANZO_API_KEY").as_deref(), Some("tok"));
        // On its own account Hermes keeps its own home.
        assert!(env(&build("hermes", &spec(Mode::Headless, Route::Inherit)), "HERMES_HOME").is_none());
    }
}
