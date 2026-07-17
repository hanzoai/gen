//! The normalized session-event vocabulary — the ONE model both backends map
//! into and the ONE mapping into cloud's `/v1/agents/sessions/:id/events` kinds.
//!
//! A backend parser turns each line of its native stream into zero or more
//! [`Mapped`] items; the orchestrator forwards them uniformly. Neither the
//! backends nor the orchestrator know the wire strings — they live here, once.

use serde::Serialize;
use serde_json::{json, Value};

/// Cloud's closed event-kind vocabulary (`cloud/clients/agents/sessions.go`).
/// A session's ordered log is exactly these kinds — nothing else is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Message,
    ToolCall,
    Spawn,
    Log,
    Status,
    /// Server-originated remote steering (pause/resume/stop/message). The CLI
    /// never emits it, but the vocabulary mirrors cloud's closed set exactly.
    #[allow(dead_code)]
    Control,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Message => "message",
            Kind::ToolCall => "tool-call",
            Kind::Spawn => "spawn",
            Kind::Log => "log",
            Kind::Status => "status",
            Kind::Control => "control",
        }
    }
}

/// Cloud's session status vocabulary. `running`/`paused` are live; `done`/`error`
/// are terminal (cloud refuses to reopen a terminal session — see `patchSession`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Paused,
    Done,
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Paused => "paused",
            Status::Done => "done",
            Status::Error => "error",
        }
    }
}

/// Per-session token/cost usage, as reported by a backend's terminal summary.
/// This is the session-log record of usage; the AUTHORITATIVE universal metering
/// is the model traffic itself, routed through the Hanzo gateway (see `route`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Usage {
    #[serde(rename = "inputTokens", skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(rename = "outputTokens", skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(rename = "cacheReadTokens", skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(rename = "cacheWriteTokens", skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(rename = "totalCostUsd", skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(rename = "numTurns", skip_serializing_if = "Option::is_none")]
    pub num_turns: Option<u64>,
    #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl Usage {
    /// True when nothing was reported — a caller can skip emitting an empty record.
    pub fn is_empty(&self) -> bool {
        *self == Usage::default()
    }

    /// Fold in a newer report. Backends report CUMULATIVE totals (Claude one
    /// final `result`; `dev` a running total per `turn.completed`), so the last
    /// non-empty value wins per field — never summed (which would double-count).
    pub fn merge(&mut self, other: Usage) {
        if other.input_tokens.is_some() { self.input_tokens = other.input_tokens; }
        if other.output_tokens.is_some() { self.output_tokens = other.output_tokens; }
        if other.cache_read_tokens.is_some() { self.cache_read_tokens = other.cache_read_tokens; }
        if other.cache_write_tokens.is_some() { self.cache_write_tokens = other.cache_write_tokens; }
        if other.total_cost_usd.is_some() { self.total_cost_usd = other.total_cost_usd; }
        if other.num_turns.is_some() { self.num_turns = other.num_turns; }
        if other.duration_ms.is_some() { self.duration_ms = other.duration_ms; }
    }
}

/// What a backend parser yields per input line. The orchestrator handles each
/// variant uniformly: forward Events, remember the resume handle, accumulate
/// Usage, and act on the Terminal signal.
#[derive(Debug, Clone, PartialEq)]
pub enum Mapped {
    /// A log-plane event to append to the session (message/tool-call/spawn/log).
    Event { kind: Kind, payload: Value },
    /// The backend disclosed its OWN session id — the resume handle.
    BackendSession(String),
    /// Token/cost usage from the backend's terminal summary.
    Usage(Usage),
    /// The run finished; `ok=false` means it errored.
    Terminal { ok: bool, summary: Option<String> },
}

impl Mapped {
    pub fn message(role: &str, text: impl Into<String>) -> Mapped {
        Mapped::Event {
            kind: Kind::Message,
            payload: json!({ "role": role, "text": clamp(text.into()) }),
        }
    }

    /// A log event tagged with a `type` (e.g. "reasoning", "todo", "error").
    pub fn note(type_tag: &str, text: impl Into<String>) -> Mapped {
        Mapped::Event {
            kind: Kind::Log,
            payload: json!({ "type": type_tag, "text": clamp(text.into()) }),
        }
    }

    pub fn tool_call(name: &str, input: Value, id: Option<&str>) -> Mapped {
        Mapped::Event {
            kind: Kind::ToolCall,
            payload: cap(json!({ "name": name, "input": input, "id": id })),
        }
    }

    pub fn tool_result(tool_use_id: Option<&str>, output: String, is_error: bool) -> Mapped {
        Mapped::Event {
            kind: Kind::Log,
            payload: json!({
                "type": "tool-result",
                "toolUseId": tool_use_id,
                "isError": is_error,
                "output": clamp(output),
            }),
        }
    }

    pub fn spawn(agent: &str, input: Value) -> Mapped {
        Mapped::Event {
            kind: Kind::Spawn,
            payload: cap(json!({ "agent": agent, "input": input })),
        }
    }
}

/// Cloud rejects an event payload over 64 KiB (`maxEventPayload`). Keep a safe
/// margin so a payload's JSON envelope never trips the boundary.
pub const PAYLOAD_BUDGET: usize = 48 * 1024;

/// Truncate one long text field so a single tool output can't blow the budget.
pub(crate) fn clamp(mut s: String) -> String {
    if s.len() > PAYLOAD_BUDGET {
        s.truncate(PAYLOAD_BUDGET);
        s.push_str("…[truncated]");
    }
    s
}

/// Final guard: if a whole payload still exceeds the budget (deeply nested tool
/// input), replace it with a truncation marker rather than let cloud 400 it.
pub fn cap(payload: Value) -> Value {
    let len = serde_json::to_string(&payload).map(|s| s.len()).unwrap_or(0);
    if len <= PAYLOAD_BUDGET {
        return payload;
    }
    json!({ "truncated": true, "bytes": len })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_status_wire_strings_match_cloud() {
        assert_eq!(Kind::Message.as_str(), "message");
        assert_eq!(Kind::ToolCall.as_str(), "tool-call");
        assert_eq!(Kind::Spawn.as_str(), "spawn");
        assert_eq!(Kind::Log.as_str(), "log");
        assert_eq!(Kind::Status.as_str(), "status");
        assert_eq!(Kind::Control.as_str(), "control");
        assert_eq!(Status::Running.as_str(), "running");
        assert_eq!(Status::Paused.as_str(), "paused");
        assert_eq!(Status::Done.as_str(), "done");
        assert_eq!(Status::Error.as_str(), "error");
    }

    #[test]
    fn oversize_text_field_is_clamped_under_budget() {
        let big = "x".repeat(PAYLOAD_BUDGET * 2);
        let Mapped::Event { payload, .. } = Mapped::message("assistant", big) else {
            panic!("expected event");
        };
        let text = payload["text"].as_str().unwrap();
        assert!(text.len() < PAYLOAD_BUDGET + 64);
        assert!(text.ends_with("…[truncated]"));
    }

    #[test]
    fn oversize_payload_is_replaced_with_marker() {
        let huge = json!({ "input": { "blob": "y".repeat(PAYLOAD_BUDGET * 2) } });
        let capped = cap(huge);
        assert_eq!(capped["truncated"], json!(true));
        assert!(serde_json::to_string(&capped).unwrap().len() < 128);
    }

    #[test]
    fn tool_call_shapes_name_input_id() {
        let Mapped::Event { kind, payload } =
            Mapped::tool_call("Bash", json!({"command": "ls"}), Some("tu_1"))
        else {
            panic!("expected event");
        };
        assert_eq!(kind, Kind::ToolCall);
        assert_eq!(payload["name"], "Bash");
        assert_eq!(payload["input"]["command"], "ls");
        assert_eq!(payload["id"], "tu_1");
    }
}
