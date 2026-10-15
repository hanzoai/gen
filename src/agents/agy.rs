//! Antigravity's CLI (`agy`). It keeps its own Google login, and takes no MCP
//! server for one run.

use anyhow::Result;
use serde_json::Value;

use super::{inside, Agent, Reader, Run, Toolset};
use crate::commands::code::event::{Mapped, Usage};

pub const AGENT: Agent = Agent {
    name: "agy",
    aliases: &["antigravity"],
    title: "Antigravity",
    programs: &["agy"],
    package: None,
    routable: false,
    toolset: Toolset::Never,
    build,
    read,
};

fn build(r: &mut Run) -> Result<()> {
    if r.auto() {
        r.arg("--dangerously-skip-permissions");
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

/// `--output-format stream-json`: one `step_update` per change of a step, the
/// reply as `text_delta` pieces until its step is `DONE`, then one `result`.
fn read(r: &mut Reader, line: &str) -> Vec<Mapped> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    match v["event"].as_str() {
        Some("step_update") => {
            let s = &v["step_update"];
            let done = s["state"] == "DONE";
            let id = s["step_index"].as_u64().map(|i| i.to_string());
            match s["step_type"].as_str() {
                Some("agent_response") => {
                    if let Some(t) = s["text_delta"].as_str() {
                        r.delta(t);
                    }
                    if done { r.flush().into_iter().collect() } else { Vec::new() }
                }
                Some("tool") => {
                    let name = s["tool_name"].as_str().unwrap_or("tool");
                    let info = &s["tool_info"];
                    if done {
                        vec![Mapped::tool_result(id.as_deref(), info["output"].as_str().unwrap_or_default().to_string(), false)]
                    } else {
                        vec![Mapped::tool_call(name, info["parameters"].clone(), id.as_deref())]
                    }
                }
                _ => Vec::new(),
            }
        }
        Some("result") => {
            let res = &v["result"];
            let u = &res["usage"];
            let mut out: Vec<Mapped> = r.flush().into_iter().collect();
            // Cache is counted apart from input.
            out.push(Mapped::Usage(Usage {
                input_tokens: inside(u["input_tokens"].as_u64(), u["cache_read_tokens"].as_u64(), None),
                output_tokens: u["output_tokens"].as_u64(),
                cache_read_tokens: u["cache_read_tokens"].as_u64(),
                num_turns: res["num_turns"].as_u64(),
                duration_ms: res["duration_seconds"].as_f64().map(|s| (s * 1000.0) as u64),
                ..Default::default()
            }));
            out.push(Mapped::Terminal { ok: res["status"] == "SUCCESS", summary: res["response"].as_str().map(|s| s.trim().to_string()) });
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
    fn agy_takes_its_task_by_print() {
        assert_eq!(argv(&build("agy", &spec(Mode::Headless, Route::Inherit))), ["--dangerously-skip-permissions", "--print", "fix the test"]);
    }

    #[test]
    fn its_stream_reads_as_one_reply_from_its_pieces() {
        let out = read("agy", include_str!("testdata/agy.jsonl"));
        assert_eq!(said(&out), ["mango"]);
        assert_eq!(called(&out), ["run_command"]);
        assert_eq!(terminal(&out), Some(true));
        assert_eq!(usage(&out).unwrap().input_tokens, Some(30615 + 25333));
    }
}
