//! Kode, a Claude Code workalike: the toolset arrives as Claude's `--mcp-config`
//! file, and `--output-format stream-json` prints Claude's SDK messages. It keeps
//! its own model configuration.

use anyhow::Result;
use serde_json::json;

use super::{wire, Agent, Run, Toolset};

pub const AGENT: Agent = Agent {
    name: "kode",
    aliases: &[],
    title: "Kode",
    programs: &["kode"],
    package: Some("@shareai-lab/kode"),
    routable: false,
    toolset: Toolset::Always,
    build,
    read: wire::claude,
};

fn build(r: &mut Run) -> Result<()> {
    if let Some(m) = r.mcp() {
        let p = r.file(&json!({ "mcpServers": { "hanzo": { "type": "stdio", "command": m.program, "args": m.args } } }))?;
        r.arg("--mcp-config").arg(p);
    }
    if r.auto() {
        r.arg("--dangerously-skip-permissions");
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

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};

    #[test]
    fn kode_takes_the_toolset_as_claudes_mcp_config() {
        let mut s = spec(Mode::Headless, Route::Inherit);
        s.structured = true;
        let a = argv(&build("kode", &s));
        assert_eq!(a[0], "--mcp-config");
        assert_eq!(a[2..], ["--dangerously-skip-permissions", "--output-format", "stream-json", "-p", "fix the test"]);
    }

    /// Kode 2.2.1 writes Claude's SDK messages (`makeSdkResultMessage`, `kodeMessageToSdkMessage`).
    #[test]
    fn its_stream_is_claude_codes() {
        let out = read("kode", r#"{"type":"assistant","message":{"content":[{"type":"text","text":"mango"}]}}
{"type":"result","subtype":"success","result":"mango","num_turns":1,"is_error":false,"session_id":"s"}"#);
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(terminal(&out), Some(true));
    }
}
