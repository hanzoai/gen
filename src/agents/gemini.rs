//! Gemini CLI. It keeps its own Google login: it speaks only Gemini's wire, which
//! api.hanzo.ai does not serve. It takes no MCP server for one run: a system
//! settings file (`GEMINI_CLI_SYSTEM_SETTINGS_PATH`) is read only when it and
//! every directory above it are root's and writable by nobody else (gemini-cli
//! 0.64 `isFileAndDirectorySecureSync`), which no file this process writes is.
//! The toolset is added once, `gemini mcp add -s user hanzo <hanzo-mcp>`.

use anyhow::Result;
use serde_json::Value;

use super::{Agent, Reader, Run, Toolset, Upgrade};
use crate::commands::code::event::{Mapped, Usage};

pub const AGENT: Agent = Agent {
    name: "gemini",
    aliases: &["gemini-cli"],
    title: "Gemini CLI",
    programs: &["gemini"],
    upgrade: Upgrade::Npm("@google/gemini-cli"),
    routable: false,
    toolset: Toolset::Never,
    build,
    read,
};

fn build(r: &mut Run) -> Result<()> {
    if r.auto() {
        r.arg("--yolo");
    }
    if r.headless() {
        if r.structured() {
            r.args(["--output-format", "stream-json"]);
        }
        let task = r.task();
        r.arg("-p").arg(task);
    }
    Ok(())
}

/// `--output-format stream-json` (gemini-cli 0.64): `init`, `message` (the
/// reply as `delta` pieces), `tool_use`, `tool_result`, then `result` with `stats`.
fn read(r: &mut Reader, line: &str) -> Vec<Mapped> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    match v["type"].as_str() {
        Some("init") => v["model"].as_str().map(|m| vec![Mapped::note("session-start", format!("model {m}"))]).unwrap_or_default(),
        Some("message") if v["role"] == "assistant" => {
            let text = v["content"].as_str().unwrap_or_default();
            if v["delta"].as_bool().unwrap_or(false) {
                r.delta(text);
                Vec::new()
            } else {
                r.delta(text);
                r.flush().into_iter().collect()
            }
        }
        Some("tool_use") => {
            let mut out: Vec<Mapped> = r.flush().into_iter().collect();
            out.push(Mapped::tool_call(v["tool_name"].as_str().unwrap_or("tool"), v["parameters"].clone(), v["tool_id"].as_str()));
            out
        }
        Some("tool_result") => {
            let output = v["output"].as_str().map(String::from).or_else(|| v["error"]["message"].as_str().map(String::from)).unwrap_or_default();
            vec![Mapped::tool_result(v["tool_id"].as_str(), output, v["status"] == "error")]
        }
        Some("result") => {
            let mut out: Vec<Mapped> = r.flush().into_iter().collect();
            let s = &v["stats"];
            out.push(Mapped::Usage(Usage {
                input_tokens: s["input_tokens"].as_u64(),
                output_tokens: s["output_tokens"].as_u64(),
                cache_read_tokens: s["cached"].as_u64(),
                duration_ms: s["duration_ms"].as_u64(),
                ..Default::default()
            }));
            out.push(Mapped::Terminal { ok: v["status"] == "success", summary: v["error"]["message"].as_str().map(String::from) });
            out
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};

    /// Gemini skips a system settings file a user owns, so a run writes none.
    #[test]
    fn gemini_keeps_its_own_account_and_is_handed_no_settings_file() {
        let l = build("gemini", &spec(Mode::Headless, gateway()));
        assert_eq!(argv(&l), ["--yolo", "-p", "fix the test"]);
        assert!(env(&l, "GEMINI_CLI_SYSTEM_SETTINGS_PATH").is_none());
        assert!(env(&l, "OPENAI_API_KEY").is_none());
        assert_eq!(argv(&build("gemini", &spec(Mode::Interactive, Route::Inherit))), ["--yolo"]);
    }

    /// The events as gemini-cli 0.64.0 writes them (`bundle/gemini-*.js`, the
    /// `JsonStreamEventType` emitters): a Gemini login is needed to capture a live run.
    #[test]
    fn its_stream_reads_as_one_reply_from_its_pieces() {
        let stream = r#"{"type":"init","timestamp":"t","session_id":"s","model":"gemini-3-pro"}
{"type":"message","timestamp":"t","role":"user","content":"say mango"}
{"type":"tool_use","timestamp":"t","tool_name":"run_shell_command","tool_id":"c1","parameters":{"command":"ls"}}
{"type":"tool_result","timestamp":"t","tool_id":"c1","status":"success","output":"calc.py"}
{"type":"message","timestamp":"t","role":"assistant","content":"man","delta":true}
{"type":"message","timestamp":"t","role":"assistant","content":"go","delta":true}
{"type":"result","timestamp":"t","status":"success","stats":{"total_tokens":12,"input_tokens":10,"output_tokens":2,"cached":4,"input":6,"duration_ms":900,"tool_calls":1}}"#;
        let out = read("gemini", stream);
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(called(&out), ["run_shell_command"]);
        assert_eq!(terminal(&out), Some(true));
        assert_eq!(usage(&out).unwrap().cache_read_tokens, Some(4));
    }
}
