//! GitHub Copilot CLI. It keeps its own GitHub login; the toolset arrives as a
//! file named by `--additional-mcp-config @FILE`, beside the person's own servers.

use anyhow::Result;
use serde_json::{json, Value};

use super::{Agent, Reader, Run, Toolset};
use crate::commands::code::event::{Mapped, Usage};

pub const AGENT: Agent = Agent {
    name: "copilot",
    aliases: &["github-copilot"],
    title: "GitHub Copilot CLI",
    programs: &["copilot"],
    package: Some("@github/copilot"),
    routable: false,
    toolset: Toolset::Always,
    build,
    read,
};

fn build(r: &mut Run) -> Result<()> {
    if let Some(m) = r.mcp() {
        // Copilot needs `type` and the tool allowlist.
        let p = r.file(&json!({ "mcpServers": { "hanzo": { "type": "stdio", "command": m.program, "args": m.args, "tools": ["*"] } } }))?;
        r.arg("--additional-mcp-config").arg(format!("@{}", p.display()));
    }
    if r.auto() {
        r.arg("--allow-all-tools");
    }
    if r.headless() {
        if r.structured() {
            r.args(["--output-format", "json"]);
        }
        let task = r.task();
        r.arg("-p").arg(task);
    }
    Ok(())
}

/// `--output-format json`: one event per line, each `{type, data}`; a reply is
/// `assistant.message` (its text and the tools it asks for), a tool's end is
/// `tool.execution_complete`, and the run ends with `result`.
fn read(_: &mut Reader, line: &str) -> Vec<Mapped> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    let d = &v["data"];
    match v["type"].as_str() {
        Some("assistant.message") => {
            let mut out = Vec::new();
            if let Some(t) = d["content"].as_str().filter(|t| !t.trim().is_empty()) {
                out.push(Mapped::message("assistant", t.trim()));
            }
            for t in d["toolRequests"].as_array().into_iter().flatten() {
                out.push(Mapped::tool_call(t["name"].as_str().unwrap_or("tool"), t["arguments"].clone(), t["toolCallId"].as_str()));
            }
            out
        }
        Some("tool.execution_complete") => {
            let output = d["result"]["content"].as_str().unwrap_or_default().to_string();
            vec![Mapped::tool_result(d["toolCallId"].as_str(), output, !d["success"].as_bool().unwrap_or(false))]
        }
        Some("result") => vec![
            Mapped::Usage(Usage { duration_ms: v["usage"]["sessionDurationMs"].as_u64(), ..Default::default() }),
            Mapped::Terminal { ok: v["exitCode"] == 0, summary: None },
        ],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};

    #[test]
    fn copilot_takes_the_toolset_as_an_additional_config_file() {
        let l = build("copilot", &spec(Mode::Headless, Route::Inherit));
        let a = argv(&l);
        assert_eq!(a[0], "--additional-mcp-config");
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(a[1].trim_start_matches('@')).unwrap()).unwrap();
        assert_eq!(doc["mcpServers"]["hanzo"]["tools"][0], "*");
        assert_eq!(a[2..], ["--allow-all-tools", "-p", "fix the test"]);
    }

    #[test]
    fn its_stream_reads_as_the_reply_and_the_shell_call() {
        let out = read("copilot", include_str!("testdata/copilot.jsonl"));
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(called(&out), ["bash"]);
        assert_eq!(terminal(&out), Some(true));
    }
}
