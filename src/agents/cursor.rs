//! Cursor's CLI (`cursor-agent`). It keeps its own login, and takes no MCP server
//! for one run: it reads only `~/.cursor/mcp.json`.

use anyhow::Result;
use serde_json::Value;

use super::{inside, Agent, Reader, Run, Toolset};
use crate::commands::code::event::{Mapped, Usage};

pub const AGENT: Agent = Agent {
    name: "cursor",
    aliases: &["agent", "cursor-agent"],
    title: "Cursor",
    // `cursor-agent` first: xAI's installer also puts an `agent` on PATH.
    programs: &["cursor-agent", "agent"],
    package: None,
    routable: false,
    toolset: Toolset::Never,
    build,
    read,
};

fn build(r: &mut Run) -> Result<()> {
    if r.auto() {
        r.arg("--force");
    }
    if r.headless() {
        if r.structured() {
            r.args(["--output-format", "stream-json"]);
        }
        let task = r.task();
        r.arg("--print").arg(task);
    }
    Ok(())
}

/// `--output-format stream-json`: Claude's message shapes, its own `tool_call`
/// events (one `<kind>ToolCall` object each), and usage in camelCase.
fn read(_: &mut Reader, line: &str) -> Vec<Mapped> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    match v["type"].as_str() {
        Some("assistant") => v["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["text"].as_str().filter(|t| !t.trim().is_empty()))
            .map(|t| Mapped::message("assistant", t.trim()))
            .collect(),
        Some("tool_call") => {
            let Some((kind, call)) = v["tool_call"].as_object().and_then(|o| o.iter().find(|(k, _)| k.ends_with("ToolCall"))) else {
                return Vec::new();
            };
            let name = kind.strip_suffix("ToolCall").unwrap_or(kind);
            let id = v["call_id"].as_str();
            match v["subtype"].as_str() {
                Some("started") => vec![Mapped::tool_call(name, call["args"].clone(), id)],
                Some("completed") => {
                    let result = &call["result"];
                    let ok = &result["success"];
                    let output = ok["interleavedOutput"].as_str().or(ok["stdout"].as_str()).map(String::from).unwrap_or_else(|| result.to_string());
                    vec![Mapped::tool_result(id, output, ok.is_null())]
                }
                _ => Vec::new(),
            }
        }
        Some("result") => {
            // Cache is counted apart from input.
            let u = &v["usage"];
            let mut out = vec![Mapped::Usage(Usage {
                input_tokens: inside(u["inputTokens"].as_u64(), u["cacheReadTokens"].as_u64(), u["cacheWriteTokens"].as_u64()),
                output_tokens: u["outputTokens"].as_u64(),
                cache_read_tokens: u["cacheReadTokens"].as_u64(),
                cache_write_tokens: u["cacheWriteTokens"].as_u64(),
                duration_ms: v["duration_ms"].as_u64(),
                ..Default::default()
            })];
            let ok = v["subtype"] == "success" && !v["is_error"].as_bool().unwrap_or(false);
            out.push(Mapped::Terminal { ok, summary: v["result"].as_str().map(String::from) });
            out
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};

    #[test]
    fn a_plain_headless_run_prints_what_cursor_prints() {
        let l = build("cursor", &spec(Mode::Headless, Route::Inherit));
        assert_eq!(argv(&l), ["--force", "--print", "fix the test"]);
        let mut s = spec(Mode::Headless, Route::Inherit);
        s.structured = true;
        assert_eq!(argv(&build("cursor", &s)), ["--force", "--output-format", "stream-json", "--print", "fix the test"]);
        assert_eq!(argv(&build("cursor", &spec(Mode::Interactive, Route::Inherit))), ["--force"]);
    }

    #[test]
    fn its_stream_reads_as_the_reply_the_shell_call_and_the_usage() {
        let out = read("cursor", include_str!("testdata/cursor.jsonl"));
        assert_eq!(said(&out), ["I'll run `ls` once, then reply with mango.", "mango"]);
        assert_eq!(called(&out), ["shell"]);
        assert_eq!(terminal(&out), Some(true));
        assert_eq!(usage(&out).unwrap().input_tokens, Some(12519 + 20096));
    }
}
