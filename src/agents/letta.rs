//! Letta Code. It keeps its own Letta login and takes no MCP server for one run.
//! `--output-format stream-json` prints Letta's messages (`message_type`), then
//! one `result`.

use anyhow::Result;
use serde_json::Value;

use super::{Agent, Reader, Run, Toolset, Upgrade};
use crate::commands::code::event::{Mapped, Usage};

pub const AGENT: Agent = Agent {
    name: "letta",
    aliases: &["letta-code"],
    title: "Letta Code",
    programs: &["letta"],
    upgrade: Upgrade::Npm("@letta-ai/letta-code"),
    routable: false,
    toolset: Toolset::Never,
    build,
    read,
};

fn build(r: &mut Run) -> Result<()> {
    if r.headless() {
        if r.structured() {
            r.args(["--output-format", "stream-json"]);
        }
        let task = r.task();
        r.arg("-p").arg(task);
    }
    Ok(())
}

/// A message's text: a string, or `{type: "text", text}` parts.
fn text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

fn read(_: &mut Reader, line: &str) -> Vec<Mapped> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    if v["type"] == "result" {
        let u = &v["usage"];
        return vec![
            Mapped::Usage(Usage {
                input_tokens: u["prompt_tokens"].as_u64(),
                output_tokens: u["completion_tokens"].as_u64(),
                cache_read_tokens: u["cached_input_tokens"].as_u64(),
                cache_write_tokens: u["cache_write_tokens"].as_u64(),
                num_turns: v["num_turns"].as_u64(),
                duration_ms: v["duration_ms"].as_u64(),
                ..Default::default()
            }),
            Mapped::Terminal { ok: v["subtype"] == "success", summary: v["result"].as_str().map(String::from) },
        ];
    }
    if v["type"] == "error" {
        return vec![Mapped::Terminal { ok: false, summary: v["message"].as_str().map(String::from) }];
    }
    match v["message_type"].as_str() {
        Some("assistant_message") => {
            let t = text(&v["content"]);
            if t.trim().is_empty() { Vec::new() } else { vec![Mapped::message("assistant", t.trim())] }
        }
        Some("tool_call_message") => {
            let c = &v["tool_call"];
            let args = c["arguments"].as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_else(|| c["arguments"].clone());
            vec![Mapped::tool_call(c["name"].as_str().unwrap_or("tool"), args, c["tool_call_id"].as_str())]
        }
        Some("tool_return_message") => {
            let out = v["tool_return"].as_str().map(String::from).unwrap_or_else(|| v["tool_return"].to_string());
            vec![Mapped::tool_result(v["tool_call_id"].as_str(), out, v["status"] == "error")]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};

    #[test]
    fn letta_takes_its_task_by_p() {
        assert_eq!(argv(&build("letta", &spec(Mode::Headless, Route::Inherit))), ["-p", "fix the test"]);
    }

    /// The shapes letta-code 0.34.14 writes (`letta.js`: `tool_call_message`,
    /// `projectToolResultMessage`, the stream-json `resultEvent`); a Letta login is
    /// needed to capture a live run.
    #[test]
    fn its_stream_reads_as_the_reply_the_tool_and_the_usage() {
        let out = read("letta", r#"{"type":"message","message_type":"tool_call_message","tool_call":{"name":"Bash","tool_call_id":"c1","arguments":"{\"command\":\"ls\"}"},"session_id":"s"}
{"message_type":"tool_return_message","tool_call_id":"c1","status":"success","tool_return":"calc.py","session_id":"s"}
{"message_type":"assistant_message","role":"assistant","content":[{"type":"text","text":"mango"}],"session_id":"s"}
{"type":"result","subtype":"success","session_id":"s","duration_ms":900,"num_turns":2,"result":"mango","usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,"step_count":2,"cached_input_tokens":4,"cache_write_tokens":0,"reasoning_tokens":0}}"#);
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(called(&out), ["Bash"]);
        assert_eq!(usage(&out).unwrap().input_tokens, Some(10));
        assert_eq!(terminal(&out), Some(true));
    }
}
