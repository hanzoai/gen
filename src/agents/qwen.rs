//! Qwen Code. Under `--cloud` its model calls go to api.hanzo.ai through its
//! OpenAI auth (`--auth-type openai`, the key in `OPENAI_API_KEY`); the toolset
//! arrives as a file named by `--mcp-config`.

use anyhow::Result;
use serde_json::json;

use super::{wire, Agent, Run, Toolset};

pub const AGENT: Agent = Agent {
    name: "qwen",
    aliases: &["qwen-code"],
    title: "Qwen Code",
    programs: &["qwen"],
    package: Some("@qwen-code/qwen-code"),
    routable: true,
    toolset: Toolset::Always,
    build,
    read: wire::claude,
};

fn build(r: &mut Run) -> Result<()> {
    if let Some(m) = r.mcp() {
        let p = r.file(&json!({ "mcpServers": { "hanzo": { "type": "stdio", "command": m.program, "args": m.args, "tools": ["*"] } } }))?;
        r.arg("--mcp-config").arg(p);
    }
    if let Some(g) = r.gateway() {
        r.args(["--auth-type", "openai", "--openai-base-url"]).arg(g.v1()).arg("-m").arg(g.model);
        r.env("OPENAI_API_KEY", g.token);
    }
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

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::Mode;
    use serde_json::Value;

    #[test]
    fn qwen_takes_the_toolset_by_flag_and_the_gateway_by_openai_auth() {
        let l = build("qwen", &spec(Mode::Headless, gateway()));
        let a = argv(&l);
        assert_eq!(a[0], "--mcp-config");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&a[1]).unwrap()).unwrap();
        assert_eq!(doc["mcpServers"]["hanzo"]["command"], "/opt/hanzo");
        assert_eq!(a[2..], ["--auth-type", "openai", "--openai-base-url", "https://api.hanzo.ai/v1", "-m", "enso", "--yolo", "-p", "fix the test"]);
        assert_eq!(env(&l, "OPENAI_API_KEY").as_deref(), Some("tok"));
    }

    #[test]
    fn its_stream_is_claude_codes() {
        let out = read("qwen", include_str!("testdata/qwen.jsonl"));
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(called(&out), ["run_shell_command"]);
        assert_eq!(terminal(&out), Some(true));
        assert_eq!(usage(&out).unwrap().input_tokens, Some(37830));
    }
}
