//! The coding-backend seam: ONE trait both `claude` and `dev` satisfy, so the
//! orchestrator (register → spawn → stream → finalize) is identical for either.
//!
//! Each backend owns only what genuinely differs: how it is invoked (argv +
//! env), how its native MCP + model-routing are wired, and how one line of its
//! structured stream maps into the normalized [`Mapped`] vocabulary. Everything
//! else is shared here or in the orchestrator.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::claude::Claude;
use super::dev::Dev;
use super::event::Mapped;

/// Which coding agent to wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Claude,
    Dev,
}

impl BackendKind {
    /// Parse the `--backend` value. The default is Claude.
    pub fn parse(s: &str) -> Result<BackendKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Ok(BackendKind::Claude),
            "dev" | "codex" => Ok(BackendKind::Dev),
            other => anyhow::bail!("unknown backend '{other}' (expected: claude | dev)"),
        }
    }
}

/// Headless (structured stream on stdout) or interactive (TTY handed to the
/// backend). The rich per-event stream lives in headless mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Headless,
    Interactive,
}

/// Where a run's model calls go, and with what credential. The secret rides in
/// the child's ENV, never argv/logs, in every variant.
///
/// One value, three destinations — so "which provider am I logged in with?"
/// has exactly one place it is answered for routing:
/// - `Gateway` — the Hanzo gateway (api.hanzo.ai). Metered into cloud_usage/o11y
///   regardless of account/machine; the credential is the hanzo.id bearer (or a
///   stored `hk-` gateway key). The recommended path.
/// - `Anthropic` / `OpenAI` — the vendor's own API, reached directly with the
///   user's own key. Not metered by Hanzo; billed by the vendor.
#[derive(Debug, Clone)]
pub enum Routing {
    /// Route through the Hanzo gateway; `token` is the hanzo.id bearer / `hk-` key.
    Gateway {
        /// The active network's api origin (e.g. https://api.hanzo.ai).
        api: String,
        /// The bearer/key authenticating gateway model calls.
        token: String,
    },
    /// Talk to Anthropic directly; `key` is the user's `sk-ant-…` key.
    Anthropic { key: String },
    /// Talk to OpenAI directly; `key` is the user's `sk-…` key.
    OpenAI { key: String },
}

/// A resolved hanzo-mcp launch (command + base args incl. `--project-dir`).
#[derive(Debug, Clone)]
pub struct McpAttach {
    pub program: String,
    pub args: Vec<String>,
}

/// Everything a backend needs to construct its invocation.
pub struct Spec {
    pub mode: Mode,
    pub task: Option<String>,
    pub cwd: PathBuf,
    pub routing: Option<Routing>,
    pub mcp: Option<McpAttach>,
    /// Emit the machine-readable event stream (only when we actually stream to
    /// cloud). When false, a headless run keeps the backend's native output and
    /// the wrapper never parses it — the privacy gate is structural.
    pub structured: bool,
    /// Pre-set the backend's session id (Claude `--session-id`) so a linked
    /// interactive run can locate its transcript to tail. Ignored on resume.
    pub preset_session: Option<String>,
    /// Trust the repository's OWN project-local config. Off by default: a repo
    /// is untrusted, and anything it declares that auto-runs (an MCP server, a
    /// `.claude/settings*.json` hook / statusLine / project plugin) would run
    /// with this process's env — which carries the model routing bearer. When
    /// set (`--trust-project`), the backend both loads the repo's `.mcp.json`
    /// AND lets its project/local settings apply. Backends that never read
    /// repo-local config (e.g. `dev`, whose servers come from `CODEX_HOME`)
    /// ignore this.
    pub trust_project: bool,
    /// The backend's OWN session id to resume, if this is a `--resume` run.
    pub resume: Option<String>,
    /// Extra args forwarded verbatim to the backend (never widened by us).
    pub passthrough: Vec<String>,
}

/// A ready-to-spawn command plus temp files that must outlive the child (e.g. a
/// Claude `--mcp-config` file). Dropping `cleanup` deletes them.
pub struct Launch {
    pub command: tokio::process::Command,
    pub cleanup: Vec<tempfile::TempPath>,
}

pub trait Backend {
    /// The `agent` label recorded on the cloud session ("claude" | "dev").
    fn label(&self) -> &'static str;

    /// Best-effort backend version string for the context snapshot.
    fn version(&self) -> Option<String>;

    /// Build the invocation for `spec`. Sets program, args, env and cwd; the
    /// caller decides stdio (piped for headless, inherited for interactive).
    fn build(&self, spec: &Spec) -> Result<Launch>;

    /// Map one line of the backend's structured stream (stdout in headless mode,
    /// or a transcript line in interactive mode) into zero or more events.
    fn parse(&self, line: &str) -> Vec<Mapped>;

    /// Locate the backend's transcript file for a session id, so interactive
    /// linking can tail it. `None` when the backend has no tailable transcript.
    fn transcript_path(&self, cwd: &Path, backend_session_id: &str) -> Option<PathBuf>;
}

/// Resolve a backend kind to its implementation.
pub fn resolve(kind: BackendKind) -> Box<dyn Backend> {
    match kind {
        BackendKind::Claude => Box::new(Claude),
        BackendKind::Dev => Box::new(Dev),
    }
}

/// Resolve how to launch hanzo-mcp as a stdio server scoped to `cwd`. Prefer the
/// installed console script, else uv's ephemeral runner. Returns `None` when
/// neither is on PATH — MCP is an enhancement, so a missing server never blocks
/// the session (the caller warns and continues without it).
pub fn resolve_mcp(cwd: &Path) -> Option<McpAttach> {
    let project = cwd.display().to_string();
    if which::which("hanzo-mcp").is_ok() {
        return Some(McpAttach {
            program: "hanzo-mcp".into(),
            args: vec!["--project-dir".into(), project],
        });
    }
    if which::which("uvx").is_ok() {
        return Some(McpAttach {
            program: "uvx".into(),
            args: vec!["hanzo-mcp".into(), "--project-dir".into(), project],
        });
    }
    None
}

/// Best-effort `<bin> --version` (first line), for the context snapshot.
pub fn backend_version(bin: &str) -> Option<String> {
    let out = std::process::Command::new(bin).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_kind_parse() {
        assert_eq!(BackendKind::parse("claude").unwrap(), BackendKind::Claude);
        assert_eq!(BackendKind::parse("CC").unwrap(), BackendKind::Claude);
        assert_eq!(BackendKind::parse("dev").unwrap(), BackendKind::Dev);
        assert_eq!(BackendKind::parse("codex").unwrap(), BackendKind::Dev);
        assert!(BackendKind::parse("gpt").is_err());
    }
}
