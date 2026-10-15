//! Cline. On its own account it keeps its own login and data. Under `--cloud` it
//! runs from a data directory of Hanzo's own (`--data-dir ~/.hanzo/cline`):
//! `settings/providers.json` selects its `openai-compatible` provider at
//! api.hanzo.ai with no key in it, so Cline reads the key from `OPENAI_API_KEY`,
//! and `settings/cline_mcp_settings.json` carries the toolset.

use anyhow::Result;
use serde_json::{json, Value};

use super::{Agent, Reader, Run, Toolset, Upgrade};
use crate::commands::code::event::{Mapped, Usage};

pub const AGENT: Agent = Agent {
    name: "cline",
    aliases: &[],
    title: "Cline",
    programs: &["cline"],
    upgrade: Upgrade::Npm("cline"),
    routable: true,
    toolset: Toolset::Routed,
    build,
    read,
};

fn build(r: &mut Run) -> Result<()> {
    if let Some(g) = r.gateway() {
        let dir = r.home("cline")?;
        let settings = dir.join("settings");
        super::super::kit::private_dir(&settings)?;
        // Cline replaces a file it cannot validate with its own default, so every
        // field it writes itself is here, `modes` and `updatedAt` included.
        let providers = json!({
            "version": 1,
            "lastUsedProvider": "openai-compatible",
            "modes": {},
            "providers": { "openai-compatible": {
                "settings": { "provider": "openai-compatible", "model": g.model, "baseUrl": g.v1() },
                "updatedAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "tokenSource": "manual",
            } },
        });
        r.write(&settings.join("providers.json"), &providers)?;
        let servers = r.mcp().map(|m| json!({ "hanzo": { "transport": { "type": "stdio", "command": m.program, "args": m.args } } })).unwrap_or_else(|| json!({}));
        r.write(&settings.join("cline_mcp_settings.json"), &json!({ "mcpServers": servers }))?;
        r.env("OPENAI_API_KEY", g.token);
        r.arg("--data-dir").arg(&dir);
    }
    if r.auto() {
        r.arg("--yolo");
    }
    if r.headless() {
        if r.structured() {
            r.arg("--json");
        }
        let task = r.task();
        r.arg(task);
    }
    Ok(())
}

/// `--json`: `agent_event`s (a `content_start` / `content_end` pair per piece of
/// text, reasoning or tool), then `run_result` with the run's usage.
fn read(_: &mut Reader, line: &str) -> Vec<Mapped> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    let e = &v["event"];
    match (v["type"].as_str(), e["type"].as_str(), e["contentType"].as_str()) {
        (Some("agent_event"), Some("content_end"), Some("text")) => {
            e["text"].as_str().filter(|t| !t.trim().is_empty()).map(|t| vec![Mapped::message("assistant", t.trim())]).unwrap_or_default()
        }
        (Some("agent_event"), Some("content_start"), Some("tool")) => {
            let mut out = vec![Mapped::tool_call(e["toolName"].as_str().unwrap_or("tool"), e["input"].clone(), e["toolCallId"].as_str())];
            // Under `--yolo` Cline ends by submitting: the summary it submits is its answer.
            if e["toolName"] == "submit_and_exit" {
                if let Some(t) = e["input"]["summary"].as_str().filter(|t| !t.trim().is_empty()) {
                    out.push(Mapped::message("assistant", t.trim()));
                }
            }
            out
        }
        (Some("agent_event"), Some("content_end"), Some("tool")) => {
            let output = match &e["output"] {
                Value::String(s) => s.clone(),
                Value::Array(items) => items.iter().filter_map(|i| i["result"].as_str()).collect::<Vec<_>>().join(""),
                other => other.to_string(),
            };
            let failed = e["output"].as_array().is_some_and(|items| items.iter().any(|i| i["success"] == false));
            vec![Mapped::tool_result(e["toolCallId"].as_str(), output, failed)]
        }
        (Some("run_result"), _, _) => {
            let u = &v["usage"];
            vec![
                Mapped::Usage(Usage {
                    input_tokens: u["inputTokens"].as_u64(),
                    output_tokens: u["outputTokens"].as_u64(),
                    cache_read_tokens: u["cacheReadTokens"].as_u64(),
                    cache_write_tokens: u["cacheWriteTokens"].as_u64(),
                    total_cost_usd: u["totalCost"].as_f64(),
                    num_turns: v["iterations"].as_u64(),
                    duration_ms: v["durationMs"].as_u64(),
                }),
                Mapped::Terminal { ok: v["finishReason"] == "completed", summary: v["text"].as_str().map(String::from) },
            ]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};
    use serde_json::Value;

    #[test]
    fn cline_takes_its_task_as_its_first_argument() {
        assert_eq!(argv(&build("cline", &spec(Mode::Headless, Route::Inherit))), ["--yolo", "fix the test"]);
    }

    #[test]
    fn cline_on_hanzo_runs_from_a_data_directory_of_hanzos_own_with_no_key_in_it() {
        let l = build("cline", &spec(Mode::Headless, gateway()));
        let a = argv(&l);
        assert_eq!(a[0], "--data-dir");
        assert_eq!(a[2..], ["--yolo", "fix the test"]);
        let settings = std::path::Path::new(&a[1]).join("settings");
        let p: Value = serde_json::from_str(&std::fs::read_to_string(settings.join("providers.json")).unwrap()).unwrap();
        assert_eq!(p["providers"]["openai-compatible"]["settings"]["baseUrl"], "https://api.hanzo.ai/v1");
        assert!(!p.to_string().contains("\"tok\""));
        let m: Value = serde_json::from_str(&std::fs::read_to_string(settings.join("cline_mcp_settings.json")).unwrap()).unwrap();
        assert_eq!(m["mcpServers"]["hanzo"]["transport"]["command"], "/opt/hanzo");
        assert_eq!(env(&l, "OPENAI_API_KEY").as_deref(), Some("tok"));
    }

    #[test]
    fn its_stream_reads_as_the_reply_the_tools_and_the_usage() {
        let out = read("cline", include_str!("testdata/cline-text.jsonl"));
        assert_eq!(said(&out), ["4"]);
        assert_eq!(terminal(&out), Some(true));
        let out = read("cline", include_str!("testdata/cline.jsonl"));
        assert_eq!(called(&out), ["run_commands", "submit_and_exit"]);
        assert_eq!(said(&out), ["Executed ls command and echoed the word mango as instructed."]);
        assert_eq!(usage(&out).unwrap().input_tokens, Some(10145));
    }
}
