//! Meta Muse Code CLI (`muse`). Keeps its own Meta login (`muse login` or
//! `META_API_KEY`); `--yolo` enables tool execution and trusts the workspace
//! so project rules and `.mcp.json` servers load cleanly.

use anyhow::Result;
use serde_json::Value;

use super::{Agent, Reader, Run, Toolset, Upgrade};
use crate::event::{Mapped, Usage};

pub const AGENT: Agent = Agent {
    name: "muse",
    aliases: &["muse-code"],
    title: "Muse Code",
    programs: &["muse"],
    upgrade: Upgrade::Own(&["--version"]),
    routable: false,
    toolset: Toolset::Never,
    build,
    read,
};

fn build(r: &mut Run) -> Result<()> {
    if r.headless() {
        r.arg("exec");
        if r.auto() {
            r.arg("--yolo");
        }
        if r.structured() {
            r.arg("--json");
        }
        let task = r.task();
        r.arg(task);
    } else if r.auto() {
        r.arg("--yolo");
    }
    Ok(())
}

/// `muse exec --json` writes one JSON record per line with `payload_type` and
/// `payload`. Usage lands on `run.usage` and the final answer/outcome on
/// `run.terminal.*`.
fn read(r: &mut Reader, line: &str) -> Vec<Mapped> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    let ptype = v["payload_type"].as_str().unwrap_or_default();
    let payload = &v["payload"];
    match ptype {
        "run.lifecycle.started" => {
            vec![Mapped::note("session-start", "muse run started")]
        }
        "task.lifecycle.started" => {
            let kind = payload["event"]["task_kind"]
                .as_str()
                .or_else(|| payload["task_id"].as_str())
                .unwrap_or("task");
            vec![Mapped::note("task", kind)]
        }
        "run.usage" => {
            let mut input = 0u64;
            let mut output = 0u64;
            let mut cached = 0u64;
            if let Some(models) = payload["models"].as_array() {
                for m in models {
                    input += m["input_tokens"].as_u64().unwrap_or(0);
                    output += m["output_tokens"].as_u64().unwrap_or(0);
                    cached += m["cached_tokens"].as_u64().unwrap_or(0);
                }
            }
            vec![Mapped::Usage(Usage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                cache_read_tokens: Some(cached),
                ..Default::default()
            })]
        }
        t if t.starts_with("run.terminal.") => {
            let mut out: Vec<Mapped> = r.flush().into_iter().collect();
            if let Some(text) = payload["text"].as_str().filter(|s| !s.is_empty()) {
                r.delta(text);
                out.extend(r.flush());
            }
            let ok = payload["terminal"].as_str() == Some("completed") || t == "run.terminal.completed";
            let summary = payload["reason"].as_str().map(String::from);
            out.push(Mapped::Terminal { ok, summary });
            out
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::backend::{Mode, Route};

    #[test]
    fn muse_builds_headless_and_interactive_argvs() {
        let h = build("muse", &spec(Mode::Headless, gateway()));
        assert_eq!(argv(&h), ["exec", "--yolo", "fix the test"]);
        assert!(env(&h, "OPENAI_API_KEY").is_none());

        let i = build("muse", &spec(Mode::Interactive, Route::Inherit));
        assert_eq!(argv(&i), ["--yolo"]);
    }

    #[test]
    fn muse_reads_jsonl_usage_and_terminal_events() {
        let stream = r#"{"payload_type":"run.lifecycle.started","payload":{"kind":"run_started"}}
{"payload_type":"run.usage","payload":{"kind":"run_usage","models":[{"input_tokens":120,"cached_tokens":40,"output_tokens":35}]}}
{"payload_type":"run.terminal.completed","payload":{"kind":"run_terminal","terminal":"completed","text":"echo: ping","reason":null}}"#;
        let out = read("muse", stream);
        assert_eq!(said(&out), ["echo: ping"]);
        assert_eq!(terminal(&out), Some(true));
        let u = usage(&out).unwrap();
        assert_eq!(u.input_tokens, Some(120));
        assert_eq!(u.output_tokens, Some(35));
        assert_eq!(u.cache_read_tokens, Some(40));
    }
}
