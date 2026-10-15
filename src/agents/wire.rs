//! Output formats more than one agent prints. An agent's file names the one it
//! speaks; the format is read here once.

use serde_json::Value;

use super::super::backend::Backend;
use super::super::claude::Claude;
use super::super::event::{Mapped, Usage};
use super::{inside, Reader};

/// Claude Code's stream-json, which Qwen Code, Amp and Kode print as well.
///
/// The session id it discloses is the agent's own, and `hanzo --resume` does not
/// hand it back to the agent, so it is not kept as a resume handle.
pub fn claude(_: &mut Reader, line: &str) -> Vec<Mapped> {
    Claude.parse(line).into_iter().filter(|m| !matches!(m, Mapped::BackendSession(_))).collect()
}

/// Plain text, for an agent with no structured output: each line is the agent speaking.
pub fn plain(_: &mut Reader, line: &str) -> Vec<Mapped> {
    let t = strip(line);
    if t.trim().is_empty() {
        Vec::new()
    } else {
        vec![Mapped::message("assistant", t.trim_end().to_string())]
    }
}

/// `line` without terminal escape sequences (colour, cursor movement).
pub fn strip(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // Parameters and intermediates, then one final byte in `@`..=`~`.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

/// OpenCode's `--format json`, which Kilo Code prints as well: one part per line,
/// a step's usage on its `step_finish`.
pub fn opencode(r: &mut Reader, line: &str) -> Vec<Mapped> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    let part = &v["part"];
    match v["type"].as_str() {
        Some("text") => part["text"].as_str().filter(|t| !t.trim().is_empty()).map(|t| vec![Mapped::message("assistant", t.trim())]).unwrap_or_default(),
        Some("tool_use") => {
            let name = part["tool"].as_str().unwrap_or("tool");
            let id = part["callID"].as_str();
            let state = &part["state"];
            let mut out = vec![Mapped::tool_call(name, state["input"].clone(), id)];
            if let Some(output) = state["output"].as_str() {
                out.push(Mapped::tool_result(id, output.to_string(), state["status"] == "error"));
            }
            out
        }
        Some("step_finish") => {
            // Cache is counted apart from input.
            let t = &part["tokens"];
            vec![r.add(Usage {
                input_tokens: inside(t["input"].as_u64(), t["cache"]["read"].as_u64(), t["cache"]["write"].as_u64()),
                output_tokens: t["output"].as_u64(),
                cache_read_tokens: t["cache"]["read"].as_u64(),
                cache_write_tokens: t["cache"]["write"].as_u64(),
                num_turns: Some(1),
                ..Default::default()
            })]
        }
        _ => Vec::new(),
    }
}

/// Pi's `--mode json`, which Oh My Pi prints as well: whole messages on
/// `message_end`, tool runs on `tool_execution_*`, usage per assistant message.
pub fn pi(r: &mut Reader, line: &str) -> Vec<Mapped> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    match v["type"].as_str() {
        Some("message_end") if v["message"]["role"] == "assistant" => {
            let m = &v["message"];
            let mut out = Vec::new();
            for part in m["content"].as_array().into_iter().flatten() {
                match part["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = part["text"].as_str().filter(|t| !t.trim().is_empty()) {
                            out.push(Mapped::message("assistant", t.trim()));
                        }
                    }
                    Some("toolCall") => out.push(Mapped::tool_call(part["name"].as_str().unwrap_or("tool"), part["arguments"].clone(), part["id"].as_str())),
                    _ => {}
                }
            }
            let u = &m["usage"];
            if u.is_object() {
                // Cache is counted apart from input.
                out.push(r.add(Usage {
                    input_tokens: inside(u["input"].as_u64(), u["cacheRead"].as_u64(), u["cacheWrite"].as_u64()),
                    output_tokens: u["output"].as_u64(),
                    cache_read_tokens: u["cacheRead"].as_u64(),
                    cache_write_tokens: u["cacheWrite"].as_u64(),
                    num_turns: Some(1),
                    ..Default::default()
                }));
            }
            if m["stopReason"] == "error" {
                out.push(Mapped::Terminal { ok: false, summary: m["errorMessage"].as_str().map(String::from) });
            }
            out
        }
        Some("tool_execution_end") => {
            let text = v["result"]["content"]
                .as_array()
                .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            vec![Mapped::tool_result(v["toolCallId"].as_str(), text, v["isError"].as_bool().unwrap_or(false))]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_the_agent_speaking_without_its_colours() {
        let mut r = Reader::default();
        assert_eq!(strip("\u{1b}[0m\u{1b}[1;36mmango\u{1b}[0m"), "mango");
        assert_eq!(super::super::test::said(&plain(&mut r, "\u{1b}[32mmango\u{1b}[0m  ")), ["mango"]);
        assert!(plain(&mut r, "\u{1b}[0m").is_empty());
    }
}
