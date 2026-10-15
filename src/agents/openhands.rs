//! OpenHands. A task runs `--headless`; under `--cloud` its model calls go to
//! api.hanzo.ai through `LLM_BASE_URL`, `LLM_API_KEY` and `LLM_MODEL=openai/…`,
//! which it reads under `--override-with-envs`. It takes no MCP server for one run.

use anyhow::Result;
use serde_json::Value;

use super::{Agent, Reader, Run, Toolset, Upgrade};
use crate::commands::code::event::Mapped;

pub const AGENT: Agent = Agent {
    name: "openhands",
    aliases: &["openhands-cli"],
    title: "OpenHands",
    programs: &["openhands"],
    upgrade: Upgrade::Uv("openhands"),
    routable: true,
    toolset: Toolset::Never,
    build,
    read,
};

fn build(r: &mut Run) -> Result<()> {
    if let Some(g) = r.gateway() {
        r.env("LLM_BASE_URL", g.v1()).env("LLM_API_KEY", g.token).env("LLM_MODEL", format!("openai/{}", g.model));
        r.arg("--override-with-envs");
    }
    if r.auto() {
        r.arg("--always-approve");
    }
    if r.headless() {
        r.arg("--headless");
        if r.structured() {
            r.arg("--json");
        }
        let task = r.task();
        r.arg("-t").arg(task);
    }
    Ok(())
}

/// The text parts of `content`.
fn text(content: &Value) -> String {
    content.as_array().map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default()
}

/// `--json`: its status lines as printed, and one JSON event per line between
/// them: `MessageEvent`, `ActionEvent` (a tool call), `ObservationEvent` (its result).
fn read(_: &mut Reader, line: &str) -> Vec<Mapped> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    match v["kind"].as_str() {
        Some("MessageEvent") if v["source"] == "agent" => {
            let t = text(&v["llm_message"]["content"]);
            if t.trim().is_empty() { Vec::new() } else { vec![Mapped::message("assistant", t.trim())] }
        }
        Some("ActionEvent") => vec![Mapped::tool_call(v["tool_name"].as_str().unwrap_or("tool"), v["action"].clone(), v["tool_call_id"].as_str())],
        Some("ObservationEvent") => {
            let o = &v["observation"];
            vec![Mapped::tool_result(v["tool_call_id"].as_str(), text(&o["content"]), o["is_error"].as_bool().unwrap_or(false))]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};

    #[test]
    fn openhands_runs_headless_and_reads_the_gateway_from_its_llm_variables() {
        let l = build("openhands", &spec(Mode::Headless, gateway()));
        assert_eq!(argv(&l), ["--override-with-envs", "--always-approve", "--headless", "-t", "fix the test"]);
        assert_eq!(env(&l, "LLM_BASE_URL").as_deref(), Some("https://api.hanzo.ai/v1"));
        assert_eq!(env(&l, "LLM_MODEL").as_deref(), Some("openai/enso"));
        assert!(!argv(&build("openhands", &spec(Mode::Interactive, Route::Inherit))).contains(&"--headless".to_string()));
    }

    #[test]
    fn its_stream_reads_past_the_status_lines_as_the_reply_and_the_terminal_call() {
        let out = read("openhands", include_str!("testdata/openhands.jsonl"));
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(called(&out), ["terminal"]);
    }
}
