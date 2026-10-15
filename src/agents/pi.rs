//! Pi. Under `--cloud` it runs from an agent directory of Hanzo's own
//! (`PI_CODING_AGENT_DIR=~/.hanzo/pi`) whose `models.json` names a `hanzo`
//! provider; pi expands `${HANZO_API_KEY}` from its environment, so the key is
//! never written down. Pi takes no MCP server.

use anyhow::Result;
use serde_json::json;

use super::{wire, Agent, Run, Toolset, Upgrade, KEY};

pub const AGENT: Agent = Agent {
    name: "pi",
    aliases: &["pi-coding-agent"],
    title: "Pi",
    programs: &["pi"],
    upgrade: Upgrade::Npm("@earendil-works/pi-coding-agent"),
    routable: true,
    toolset: Toolset::Never,
    build,
    read: wire::pi,
};

fn build(r: &mut Run) -> Result<()> {
    if let Some(g) = r.gateway() {
        let dir = r.home("pi")?;
        let models = json!({ "providers": { "hanzo": {
            "baseUrl": g.v1(), "api": "openai-completions", "apiKey": format!("${{{KEY}}}"),
            "models": [{ "id": g.model, "name": g.model }],
        } } });
        r.write(&dir.join("models.json"), &models)?;
        r.env("PI_CODING_AGENT_DIR", &dir).env(KEY, g.token);
        r.args(["--provider", "hanzo", "--model"]).arg(g.model);
    }
    if r.headless() {
        if r.structured() {
            r.args(["--mode", "json"]);
        }
        let task = r.task();
        r.arg("-p").arg(task);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};
    use serde_json::Value;

    #[test]
    fn pi_on_hanzo_reads_a_provider_from_its_own_directory_and_the_key_from_its_environment() {
        let l = build("pi", &spec(Mode::Headless, gateway()));
        assert_eq!(argv(&l), ["--provider", "hanzo", "--model", "enso", "-p", "fix the test"]);
        let dir = env(&l, "PI_CODING_AGENT_DIR").unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(std::path::Path::new(&dir).join("models.json")).unwrap()).unwrap();
        assert_eq!(doc["providers"]["hanzo"]["apiKey"], "${HANZO_API_KEY}");
        assert!(env(&build("pi", &spec(Mode::Headless, Route::Inherit)), "PI_CODING_AGENT_DIR").is_none());
    }

    #[test]
    fn its_stream_reads_as_the_reply_the_shell_call_and_the_usage_of_both_messages() {
        let out = read("pi", include_str!("testdata/pi.jsonl"));
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(called(&out), ["bash"]);
        assert_eq!(usage(&out).unwrap().input_tokens, Some(4287 + 4364));
    }
}
