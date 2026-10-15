//! Amp. It keeps its own Amp login; the toolset arrives inline, as the JSON after
//! `--mcp-config`, and `--stream-json` prints Claude Code's stream-json.

use anyhow::Result;
use serde_json::json;

use super::{wire, Agent, Run, Toolset};

pub const AGENT: Agent = Agent {
    name: "amp",
    aliases: &[],
    title: "Amp",
    programs: &["amp"],
    package: Some("@sourcegraph/amp"),
    routable: false,
    toolset: Toolset::Always,
    build,
    read: wire::claude,
};

fn build(r: &mut Run) -> Result<()> {
    if let Some(m) = r.mcp() {
        r.arg("--mcp-config").arg(json!({ "hanzo": { "command": m.program, "args": m.args } }).to_string());
    }
    if r.auto() {
        r.arg("--dangerously-allow-all");
    }
    if r.headless() {
        if r.structured() {
            r.arg("--stream-json");
        }
        let task = r.task();
        r.arg("-x").arg(task);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};
    use serde_json::{json, Value};

    #[test]
    fn amp_takes_the_bare_entry_inline() {
        let a = argv(&build("amp", &spec(Mode::Headless, Route::Inherit)));
        assert_eq!(a[0], "--mcp-config");
        let doc: Value = serde_json::from_str(&a[1]).unwrap();
        assert_eq!(doc, json!({ "hanzo": { "command": "/opt/hanzo", "args": ["__mcp", "--", "hanzo-mcp"] } }));
        assert_eq!(a[2..], ["--dangerously-allow-all", "-x", "fix the test"]);
    }

    /// Amp's `--help` documents `--stream-json` as "Claude Code-compatible stream JSON",
    /// read with `select(.type=="assistant") | .message.content[] | select(.type=="text")`.
    #[test]
    fn its_stream_is_claude_codes() {
        let out = read("amp", r#"{"type":"assistant","message":{"content":[{"type":"text","text":"mango"}]}}
{"type":"result","subtype":"success","is_error":false,"result":"mango"}"#);
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(terminal(&out), Some(true));
    }
}
