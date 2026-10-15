//! Oh My Pi. Under `--cloud` it runs from an agent directory of Hanzo's own
//! (`PI_CODING_AGENT_DIR=~/.hanzo/omp`): `models.yml` names a `hanzo` provider
//! whose `apiKey` is the NAME of the variable holding the key (omp does not
//! expand `${…}`), and `mcp.json` carries the toolset. On its own account it
//! keeps its own directory, so it takes the toolset only when routed.

use anyhow::Result;
use serde_json::json;

use super::{wire, Agent, Run, Toolset, KEY};

pub const AGENT: Agent = Agent {
    name: "omp",
    aliases: &["oh-my-pi"],
    title: "Oh My Pi",
    programs: &["omp"],
    package: Some("@oh-my-pi/pi-coding-agent"),
    routable: true,
    toolset: Toolset::Routed,
    build,
    read: wire::pi,
};

fn build(r: &mut Run) -> Result<()> {
    if let Some(g) = r.gateway() {
        let dir = r.home("omp")?;
        // JSON is YAML: the file omp reads, written without a YAML writer.
        let models = json!({ "providers": { "hanzo": {
            "baseUrl": g.v1(), "api": "openai-completions", "apiKey": KEY,
            "models": [{ "id": g.model, "name": g.model }],
        } } });
        r.write(&dir.join("models.yml"), &models)?;
        let mcp = r.mcp().map(|m| json!({ "mcpServers": { "hanzo": { "command": m.program, "args": m.args } } })).unwrap_or_else(|| json!({ "mcpServers": {} }));
        r.write(&dir.join("mcp.json"), &mcp)?;
        r.env("PI_CODING_AGENT_DIR", &dir).env(KEY, g.token);
        r.arg("--model").arg(format!("hanzo/{}", g.model));
    }
    if r.auto() {
        r.arg("--auto-approve");
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
    fn omp_on_hanzo_reads_its_provider_and_the_toolset_from_a_directory_of_hanzos_own() {
        let l = build("omp", &spec(Mode::Headless, gateway()));
        assert_eq!(argv(&l), ["--model", "hanzo/enso", "--auto-approve", "-p", "fix the test"]);
        let dir = std::path::PathBuf::from(env(&l, "PI_CODING_AGENT_DIR").unwrap());
        let models: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("models.yml")).unwrap()).unwrap();
        assert_eq!(models["providers"]["hanzo"]["apiKey"], "HANZO_API_KEY");
        let mcp: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"]["hanzo"]["command"], "/opt/hanzo");
        // On its own account omp keeps its own directory.
        let own = build("omp", &spec(Mode::Headless, Route::Inherit));
        assert!(env(&own, "PI_CODING_AGENT_DIR").is_none());
        assert_eq!(argv(&own), ["--auto-approve", "-p", "fix the test"]);
    }

    #[test]
    fn its_stream_is_pis() {
        let out = read("omp", include_str!("testdata/omp.jsonl"));
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(called(&out), ["bash"]);
        assert!(usage(&out).unwrap().input_tokens.unwrap() > 0);
    }
}
