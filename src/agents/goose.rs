//! Goose. The toolset arrives as an extension, its command line after
//! `--with-extension`; under `--cloud` its model calls go to api.hanzo.ai through
//! its `openai` provider (`OPENAI_HOST`, `OPENAI_BASE_PATH`, `OPENAI_API_KEY`).
//! It acts without asking when `GOOSE_MODE=auto`.

use anyhow::Result;
use serde_json::Value;

use super::{shell_word, Agent, Reader, Run, Toolset, Upgrade};
use crate::commands::code::event::{Mapped, Usage};

pub const AGENT: Agent = Agent {
    name: "goose",
    aliases: &[],
    title: "Goose",
    programs: &["goose"],
    upgrade: Upgrade::Own(&["update"]),
    routable: true,
    toolset: Toolset::Always,
    build,
    read,
};

fn build(r: &mut Run) -> Result<()> {
    // `goose run` for a task, `goose session` for the screen.
    r.arg(if r.headless() { "run" } else { "session" });
    if let Some(m) = r.mcp() {
        let line = std::iter::once(&m.program).chain(&m.args).map(|w| shell_word(w)).collect::<Vec<_>>().join(" ");
        r.arg("--with-extension").arg(line);
    }
    if let Some(g) = r.gateway() {
        r.env("OPENAI_HOST", g.api.trim_end_matches('/')).env("OPENAI_BASE_PATH", "v1/chat/completions").env("OPENAI_API_KEY", g.token);
        r.args(["--provider", "openai", "--model"]).arg(g.model);
    }
    if r.auto() {
        r.env("GOOSE_MODE", "auto");
    }
    if r.headless() {
        if r.structured() {
            r.args(["--output-format", "stream-json"]);
        }
        let task = r.task();
        r.arg("-t").arg(task);
    }
    Ok(())
}

/// `--output-format stream-json`: a banner, then one `message` per line (text,
/// `toolRequest` from the assistant, `toolResponse` back), then `complete` with
/// the run's tokens.
fn read(_: &mut Reader, line: &str) -> Vec<Mapped> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    match v["type"].as_str() {
        Some("message") => {
            let m = &v["message"];
            let mut out = Vec::new();
            for part in m["content"].as_array().into_iter().flatten() {
                match part["type"].as_str() {
                    Some("text") if m["role"] == "assistant" => {
                        if let Some(t) = part["text"].as_str().filter(|t| !t.trim().is_empty()) {
                            out.push(Mapped::message("assistant", t.trim()));
                        }
                    }
                    Some("toolRequest") => {
                        let call = &part["toolCall"]["value"];
                        out.push(Mapped::tool_call(call["name"].as_str().unwrap_or("tool"), call["arguments"].clone(), part["id"].as_str()));
                    }
                    Some("toolResponse") => {
                        let res = &part["toolResult"];
                        let text = res["value"]["content"]
                            .as_array()
                            .map(|c| c.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"))
                            .unwrap_or_default();
                        let failed = res["status"] != "success" || res["value"]["isError"].as_bool().unwrap_or(false);
                        out.push(Mapped::tool_result(part["id"].as_str(), text, failed));
                    }
                    _ => {}
                }
            }
            out
        }
        Some("complete") => vec![Mapped::Usage(Usage {
            input_tokens: v["input_tokens"].as_u64(),
            output_tokens: v["output_tokens"].as_u64(),
            cache_read_tokens: v["cache_read_input_tokens"].as_u64(),
            cache_write_tokens: v["cache_write_input_tokens"].as_u64(),
            ..Default::default()
        })],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};

    #[test]
    fn goose_takes_the_toolset_as_an_extension_and_the_gateway_as_its_openai_provider() {
        let l = build("goose", &spec(Mode::Headless, gateway()));
        assert_eq!(argv(&l), ["run", "--with-extension", "/opt/hanzo __mcp -- hanzo-mcp", "--provider", "openai", "--model", "enso", "-t", "fix the test"]);
        assert_eq!(env(&l, "OPENAI_HOST").as_deref(), Some("https://api.hanzo.ai"));
        assert_eq!(env(&l, "OPENAI_BASE_PATH").as_deref(), Some("v1/chat/completions"));
        assert_eq!(env(&l, "GOOSE_MODE").as_deref(), Some("auto"));
        // Interactive goose is `goose session`, which takes the same extension.
        assert_eq!(argv(&build("goose", &spec(Mode::Interactive, Route::Inherit)))[..2], ["session", "--with-extension"]);
    }

    #[test]
    fn its_stream_reads_past_the_banner_as_the_reply_the_shell_call_and_the_tokens() {
        let out = read("goose", include_str!("testdata/goose.jsonl"));
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(called(&out), ["shell"]);
        assert_eq!(usage(&out).unwrap().input_tokens, Some(14697));
    }
}
