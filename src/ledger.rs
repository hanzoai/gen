//! What the Hanzo toolset did in one session: one line per tool call.
//!
//! The lazy proxy ([`super::mcp`]) sees every `tools/call` and its answer, so it
//! writes the ledger and no server or tool has to know about it. A line names the
//! tool, how long the call took, whether it succeeded, and the model and tokens
//! the answer itself reports (`model`, `usage`), read from the answer's JSON
//! wherever it puts them. Nothing here names a tool: a call that reports a model
//! went to one, and one that does not ran on this machine.
//!
//! The session summary reads the file back ([`rows`]) to say what was handed to
//! Enso, Kai or any other model, beside the agent's own account.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// One tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub tool: String,
    pub ms: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
}

/// `~/.hanzo/sessions/<id>.jsonl`.
pub fn path(id: &str) -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".hanzo").join("sessions").join(format!("{id}.jsonl")))
}

/// The entry for a `tools/call` named `tool` that took `ms`, from the server's reply.
pub fn entry(tool: &str, ms: u64, reply: &Value) -> Entry {
    let result = reply.get("result");
    let ok = reply.get("error").is_none() && !result.and_then(|r| r.get("isError")).and_then(Value::as_bool).unwrap_or(false);
    // A tool answers in text; the text is JSON when the tool reports what served it.
    let docs: Vec<Value> = result
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .filter_map(|t| serde_json::from_str::<Value>(t).ok())
                .collect()
        })
        .unwrap_or_default();
    let structured = result.and_then(|r| r.get("structuredContent")).cloned();
    let mut model = None;
    let mut usage = None;
    for d in docs.iter().chain(structured.iter()) {
        model = model.or_else(|| find(d, &|k, v| (k == "model").then(|| v.as_str().map(String::from)).flatten(), 0));
        usage = usage.or_else(|| find(d, &|k, v| (k == "usage" && v.is_object()).then(|| v.clone()), 0));
    }
    let count = |keys: &[&str]| usage.as_ref().and_then(|u| keys.iter().find_map(|k| u.get(*k).and_then(Value::as_u64)));
    Entry {
        tool: tool.to_string(),
        ms,
        ok,
        model,
        input: count(&["input_tokens", "prompt_tokens"]),
        output: count(&["output_tokens", "completion_tokens"]),
    }
}

/// The first value `pick` returns, breadth first over objects and arrays, a few levels deep.
fn find<T>(v: &Value, pick: &dyn Fn(&str, &Value) -> Option<T>, depth: usize) -> Option<T> {
    if depth > 4 {
        return None;
    }
    match v {
        Value::Object(m) => m
            .iter()
            .find_map(|(k, x)| pick(k, x))
            .or_else(|| m.values().find_map(|x| find(x, pick, depth + 1))),
        Value::Array(a) => a.iter().find_map(|x| find(x, pick, depth + 1)),
        _ => None,
    }
}

/// Append one entry. A ledger that cannot be written never fails the call it records.
pub fn append(path: &Path, e: &Entry) {
    let Ok(line) = serde_json::to_string(e) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Every entry in the file; a missing file is an empty ledger.
pub fn read(path: &Path) -> Vec<Entry> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default()
}

/// The summary's rows: one for the calls in all, one per model the calls reached,
/// and one for the calls that ran on this machine.
pub fn rows(entries: &[Entry]) -> Vec<(String, String)> {
    if entries.is_empty() {
        return vec![("hanzo tools".into(), "attached, none called this session".into())];
    }
    let mut by_model: BTreeMap<&str, (u64, u64, u64, u64)> = BTreeMap::new();
    let mut local: BTreeMap<&str, u64> = BTreeMap::new();
    for e in entries {
        match e.model.as_deref() {
            Some(m) => {
                let s = by_model.entry(m).or_default();
                s.0 += 1;
                s.1 += e.input.unwrap_or(0);
                s.2 += e.output.unwrap_or(0);
                s.3 += e.ms;
            }
            None => *local.entry(e.tool.as_str()).or_default() += 1,
        }
    }
    let failed = entries.iter().filter(|e| !e.ok).count();
    let total: u64 = entries.iter().map(|e| e.ms).sum();
    let mut head = format!("{} {} · {}", entries.len(), plural(entries.len() as u64, "call"), seconds(total));
    if failed > 0 {
        head.push_str(&format!(" · {failed} failed"));
    }
    let mut out = vec![("hanzo tools".to_string(), head)];
    for (m, (n, i, o, ms)) in by_model {
        let mut line = format!("{n} {} handed to {m}", plural(n, "call"));
        if i + o > 0 {
            line.push_str(&format!(" · in {} · out {}", crate::commands::savings::int(i), crate::commands::savings::int(o)));
        }
        line.push_str(&format!(" · {} each", seconds(ms / n.max(1))));
        out.push((String::new(), line));
    }
    if !local.is_empty() {
        let n: u64 = local.values().sum();
        let names: Vec<&str> = local.keys().copied().collect();
        out.push((String::new(), format!("{n} {} on this machine ({})", plural(n, "call"), names.join(", "))));
    }
    out
}

fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

fn seconds(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply(doc: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 3, "result": { "content": [{ "type": "text", "text": doc.to_string() }], "isError": false } })
    }

    #[test]
    fn the_model_and_tokens_are_read_from_wherever_the_answer_puts_them() {
        let kai = entry("kai_decide", 53, &reply(json!({ "data": { "model": "kai", "usage": { "input_tokens": 11, "output_tokens": 0 } }, "ok": true })));
        assert_eq!((kai.model.as_deref(), kai.input, kai.output, kai.ok), (Some("kai"), Some(11), Some(0), true));
        let enso = entry("llm", 900, &reply(json!({ "data": { "content": "kiwi", "model": "enso-auto",
            "response": { "model": "enso-auto", "usage": { "prompt_tokens": 110, "completion_tokens": 29 } } } })));
        assert_eq!((enso.model.as_deref(), enso.input, enso.output), (Some("enso-auto"), Some(110), Some(29)));
    }

    #[test]
    fn a_call_that_reports_no_model_ran_here_and_an_error_is_not_ok() {
        let fs = entry("fs", 2, &json!({ "jsonrpc": "2.0", "id": 4, "result": { "content": [{ "type": "text", "text": "fn main() {}" }] } }));
        assert_eq!((fs.model, fs.ok), (None, true));
        let bad = entry("llm", 5, &json!({ "jsonrpc": "2.0", "id": 5, "error": { "code": -32000, "message": "no" } }));
        assert!(!bad.ok);
        let flagged = entry("llm", 5, &json!({ "jsonrpc": "2.0", "id": 6, "result": { "content": [], "isError": true } }));
        assert!(!flagged.ok);
    }

    #[test]
    fn the_ledger_round_trips_and_the_rows_name_each_model() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        for e in [
            Entry { tool: "llm".into(), ms: 800, ok: true, model: Some("enso-auto".into()), input: Some(100), output: Some(20) },
            Entry { tool: "llm".into(), ms: 1200, ok: true, model: Some("enso-auto".into()), input: Some(50), output: Some(10) },
            Entry { tool: "kai_decide".into(), ms: 50, ok: true, model: Some("kai".into()), input: Some(11), output: Some(0) },
            Entry { tool: "fs".into(), ms: 3, ok: true, model: None, input: None, output: None },
        ] {
            append(&p, &e);
        }
        let got = read(&p);
        assert_eq!(got.len(), 4);
        let rows = rows(&got);
        assert_eq!(rows[0], ("hanzo tools".into(), "4 calls · 2.1 s".into()));
        assert_eq!(rows[1].1, "2 calls handed to enso-auto · in 150 · out 30 · 1.0 s each");
        assert_eq!(rows[2].1, "1 call handed to kai · in 11 · out 0 · 50 ms each");
        assert_eq!(rows[3].1, "1 call on this machine (fs)");
        assert_eq!(super::rows(&[])[0].1, "attached, none called this session");
    }
}
