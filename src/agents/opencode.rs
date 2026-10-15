//! OpenCode. Its whole configuration for one run is one JSON document in
//! `OPENCODE_CONFIG_CONTENT`: the toolset, and under `--cloud` a `hanzo` provider whose key it
//! reads from `HANZO_API_KEY`, so the key is never written down.

use anyhow::Result;
use serde_json::json;

use super::{wire, Agent, Run, Toolset, KEY};

pub const AGENT: Agent = Agent {
    name: "opencode",
    aliases: &[],
    title: "OpenCode",
    programs: &["opencode"],
    package: Some("opencode-ai"),
    routable: true,
    toolset: Toolset::Always,
    build,
    read: wire::opencode,
};

fn build(r: &mut Run) -> Result<()> {
    let mut doc = json!({});
    if let Some(m) = r.mcp() {
        let command: Vec<&str> = std::iter::once(m.program.as_str()).chain(m.args.iter().map(String::as_str)).collect();
        doc["mcp"] = json!({ "hanzo": { "type": "local", "command": command, "enabled": true } });
    }
    if let Some(g) = r.gateway() {
        doc["provider"] = json!({ "hanzo": {
            "npm": "@ai-sdk/openai-compatible",
            "name": "Hanzo",
            "options": { "baseURL": g.v1(), "apiKey": format!("{{env:{KEY}}}") },
            "models": { (g.model): { "name": g.model } },
        } });
        doc["model"] = json!(format!("hanzo/{}", g.model));
        r.env(KEY, g.token);
    }
    if doc.as_object().is_some_and(|o| !o.is_empty()) {
        r.env("OPENCODE_CONFIG_CONTENT", doc.to_string());
    }
    if r.headless() {
        r.arg("run");
        if r.auto() {
            r.arg("--auto");
        }
        if r.structured() {
            r.args(["--format", "json"]);
        }
        let task = r.task();
        r.arg(task);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};
    use serde_json::Value;

    #[test]
    fn one_document_carries_the_toolset_and_on_hanzo_the_provider() {
        let l = build("opencode", &spec(Mode::Headless, gateway()));
        assert_eq!(argv(&l), ["run", "--auto", "fix the test"]);
        let doc: Value = serde_json::from_str(&env(&l, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(doc["mcp"]["hanzo"]["command"][0], "/opt/hanzo");
        assert_eq!(doc["provider"]["hanzo"]["options"]["baseURL"], "https://api.hanzo.ai/v1");
        assert_eq!(doc["provider"]["hanzo"]["options"]["apiKey"], "{env:HANZO_API_KEY}");
        assert_eq!(doc["model"], "hanzo/enso");
        assert_eq!(env(&l, "HANZO_API_KEY").as_deref(), Some("tok"));
        // On its own account there is no provider, only the toolset.
        let own = build("opencode", &spec(Mode::Headless, Route::Inherit));
        let doc: Value = serde_json::from_str(&env(&own, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert!(doc.get("provider").is_none() && doc.get("mcp").is_some());
        // Interactive is its own screen: no subcommand, no task.
        assert!(argv(&build("opencode", &spec(Mode::Interactive, Route::Inherit))).is_empty());
    }

    #[test]
    fn its_stream_reads_as_the_reply_the_shell_call_and_the_usage_of_both_steps() {
        let out = read("opencode", include_str!("testdata/opencode.jsonl"));
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(called(&out), ["bash"]);
        let u = usage(&out).unwrap();
        assert_eq!(u.num_turns, Some(2));
        assert!(u.input_tokens.unwrap() > 0);
    }
}
