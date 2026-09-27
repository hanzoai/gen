//! Antigravity (`agy`) and Cursor (`agent`).
//!
//! These two keep their own login. A session launches the binary and leaves its
//! credential store alone — there is no gateway rewrite, because neither CLI
//! speaks the Hanzo model route. Headless runs ask for the stream-json each
//! binary documents; an interactive run is the binary's own screen.

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::backend::{Approval, Backend, Launch, Mode, Route, Spec};
use super::event::Mapped;

/// Which of the two external agents.
#[derive(Clone, Copy)]
pub enum Which {
    /// Google Antigravity.
    Agy,
    /// Cursor's agent CLI (`agent`, or `cursor-agent` when that is the name on PATH).
    Cursor,
}

pub struct External {
    which: Which,
}

impl External {
    pub const AGY: External = External { which: Which::Agy };
    pub const CURSOR: External = External {
        which: Which::Cursor,
    };

    fn label_str(&self) -> &'static str {
        match self.which {
            Which::Agy => "agy",
            Which::Cursor => "cursor",
        }
    }

    /// The binary to exec. Cursor ships as `agent` and, on some installs, only
    /// as `cursor-agent`.
    fn program(&self) -> &'static str {
        match self.which {
            Which::Agy => "agy",
            Which::Cursor => {
                if which::which("agent").is_ok() {
                    "agent"
                } else {
                    "cursor-agent"
                }
            }
        }
    }
}

impl Backend for External {
    fn label(&self) -> &'static str {
        self.label_str()
    }

    fn version(&self) -> Option<String> {
        super::backend::backend_version(self.program())
    }

    fn build(&self, spec: &Spec) -> Result<Launch> {
        let mut cmd = tokio::process::Command::new(self.program());
        cmd.current_dir(&spec.cwd);

        if matches!(spec.approval, Approval::Auto | Approval::Bypass) {
            match self.which {
                Which::Agy => {
                    cmd.arg("--dangerously-skip-permissions");
                }
                Which::Cursor => {
                    cmd.arg("--force");
                }
            }
        }

        if spec.mode == Mode::Headless {
            let task = spec.task.as_deref().unwrap_or_default();
            cmd.arg("--print")
                .arg(task)
                .args(["--output-format", "stream-json"]);
        }

        cmd.args(&spec.passthrough);
        Ok(Launch {
            command: cmd,
            cleanup: Vec::new(),
        })
    }

    fn parse(&self, line: &str) -> Vec<Mapped> {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            return Vec::new();
        };
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => text_of(&v)
                .map(|t| vec![Mapped::message("assistant", t)])
                .unwrap_or_default(),
            Some("result") => {
                let ok = !v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                vec![Mapped::Terminal {
                    ok,
                    summary: v.get("result").and_then(Value::as_str).map(String::from),
                }]
            }
            _ => Vec::new(),
        }
    }

    fn transcript_path(
        &self,
        _route: &Route,
        _cwd: &Path,
        _backend_session_id: &str,
    ) -> Option<PathBuf> {
        None
    }
}

fn text_of(v: &Value) -> Option<String> {
    if let Some(t) = v.get("text").and_then(Value::as_str) {
        return Some(t.to_string());
    }
    v.pointer("/message/content")
        .and_then(Value::as_array)
        .and_then(|parts| {
            parts.iter().find_map(|p| {
                (p.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| p.get("text").and_then(Value::as_str).map(String::from))
                    .flatten()
            })
        })
}
