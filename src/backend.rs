//! The coding-backend seam: ONE trait every backend satisfies, so the
//! orchestrator (register → spawn → stream → finalize) is identical for all of
//! them. Three backends today — our own `dev` (the default), `claude` and
//! `codex` — and they are distinct products, never aliases of one another.
//!
//! Each backend owns only what genuinely differs: how it is invoked (argv +
//! env), how its native MCP + model-routing are wired, and how one line of its
//! structured stream maps into the normalized [`Mapped`] vocabulary. Everything
//! else is shared here or in the orchestrator.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::claude::Claude;
use super::dev::Agent;
use super::event::Mapped;

/// Which coding agent to wrap. Three distinct products — never aliases of each
/// other: a user who names one gets THAT agent, or a clear failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Our own coding agent (hanzoai/dev). The default.
    Dev,
    Claude,
    Codex,
}

impl BackendKind {
    /// Parse a backend name. Every accepted spelling lives HERE and nowhere else.
    pub fn parse(s: &str) -> Result<BackendKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dev" => Ok(BackendKind::Dev),
            "claude" | "claude-code" | "cc" => Ok(BackendKind::Claude),
            "codex" => Ok(BackendKind::Codex),
            other => anyhow::bail!("unknown backend '{other}' (expected: {EXPECTED})"),
        }
    }

    /// Does this operand NAME a backend (as opposed to being a task)? Reads the
    /// same table [`BackendKind::parse`] does, so the two can never disagree.
    pub fn names(s: &str) -> bool {
        BackendKind::parse(s).is_ok()
    }
}

/// The backend names offered in help text and in the "unknown backend" error.
const EXPECTED: &str = "dev | claude | codex";

/// The default backend when no spelling names one: OUR agent.
const DEFAULT: BackendKind = BackendKind::Dev;

/// How a backend was named on the command line. The CLI offers several spellings
/// —
///
/// ```text
/// hanzo code dev         hanzo code --dev      hanzo code --backend dev
/// hanzo code claude      hanzo code --claude
/// hanzo code codex       hanzo code --codex
/// hanzo dev              (top-level shorthand for `hanzo code dev`)
/// hanzo code             (default backend: dev)
/// hanzo "fix the test"   (bare session, default backend)
/// ```
///
/// — and every one of them lands in [`select`]. THIS IS THE ONLY PLACE A BACKEND
/// IS RESOLVED. The spellings are pure surface: they differ in nothing but how
/// the name was written, and they all launch through the single
/// [`super::run`] path. If you are about to add a second launch path for a new
/// spelling, add a spelling here instead.
pub struct Selection {
    /// The first positional operand — a backend name, or the task.
    pub positional: Option<String>,
    /// The second positional — only reachable as `hanzo code <backend> <task>`.
    pub tail: Option<String>,
    /// `--backend X`, or whichever of `--claude` / `--codex` / `--dev` was set.
    /// clap's arg group guarantees at most one of those reaches us.
    pub named: Option<String>,
}

/// Resolve `(backend, task)` from every spelling at once.
///
/// The one rule for the positional operand: it is the BACKEND if it is exactly a
/// backend name, otherwise it is the TASK. So `hanzo code dev` picks a backend
/// and `hanzo code "fix the test"` states a task, with no flag needed for either.
///
/// Naming the backend twice is an error rather than a precedence rule, because a
/// precedence rule here is one nobody could remember: `hanzo code claude --codex`
/// has no defensible winner, so it gets a clean message instead of a silent pick.
pub fn select(sel: Selection) -> Result<(BackendKind, Option<String>)> {
    let positional_names_backend = sel.positional.as_deref().is_some_and(BackendKind::names);

    if positional_names_backend {
        if let Some(named) = &sel.named {
            let pos = sel.positional.as_deref().unwrap_or_default();
            anyhow::bail!(
                "backend named twice: `{pos}` and `--{named}` — name it once \
                 (`hanzo code {pos}` or `hanzo code --{named}`)"
            );
        }
        let kind = BackendKind::parse(sel.positional.as_deref().unwrap_or_default())?;
        return Ok((kind, sel.tail));
    }

    // The positional is a task (or absent), so a second one has nothing to bind to.
    if let Some(extra) = &sel.tail {
        anyhow::bail!(
            "unexpected argument '{extra}' — a task is ONE argument; quote it \
             (`hanzo code \"fix the failing test\"`)"
        );
    }

    let kind = match sel.named.as_deref() {
        Some(name) => BackendKind::parse(name)?,
        None => DEFAULT,
    };
    Ok((kind, sel.positional))
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
///
/// `Debug` is hand-written to REDACT the secret (below): a run at `-vvv` wires
/// `trace!`, so a stray `trace!(?routing)` must never print an `sk-ant-…`/`hk-…`/
/// bearer — only the non-secret `api` survives.
#[derive(Clone)]
pub enum Routing {
    /// Route through the Hanzo gateway; `token` is the hanzo.id bearer / `hk-` key.
    Gateway {
        /// The active network's api origin (e.g. https://api.hanzo.ai).
        api: String,
        /// The bearer/key authenticating gateway model calls.
        token: String,
        /// The gateway catalog id for the main model, already resolved by precedence
        /// (`--model`, then exported env, then `~/.hanzo/settings.json`, then the
        /// built-in default). The gateway is the authority on validity — a bad id
        /// 400s with the gateway's own message. Carried ONLY here so the model can
        /// never leak onto a direct-provider route.
        model: String,
        /// The gateway catalog id for the small/fast model (Claude's
        /// `ANTHROPIC_SMALL_FAST_MODEL`); `dev` has no small/fast model and ignores it.
        small_fast_model: String,
        /// The context window (tokens) to request for `model`. A backend pointed at
        /// a custom gateway can't verify the model's real window and self-clamps to
        /// 200K, so this NAMES it — Claude via the `[1m]` model suffix, `dev` via a
        /// `model_catalog_json`. Carried ONLY here: a direct-provider route uses that
        /// provider's own window, never this.
        context_window: u64,
    },
    /// Talk to Anthropic directly; `key` is the user's `sk-ant-…` key.
    Anthropic { key: String },
    /// Talk to OpenAI directly; `key` is the user's `sk-…` key.
    OpenAI { key: String },
}

impl std::fmt::Debug for Routing {
    /// Redacted: the `token`/`key` is a secret and must NEVER reach a log. Same
    /// reason `Spec` omits `Debug` entirely.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Routing::Gateway { api, .. } => {
                f.debug_struct("Gateway").field("api", api).field("token", &"***").finish()
            }
            Routing::Anthropic { .. } => f.debug_struct("Anthropic").field("key", &"***").finish(),
            Routing::OpenAI { .. } => f.debug_struct("OpenAI").field("key", &"***").finish(),
        }
    }
}

/// The model-routing DECISION for a run — three outcomes, because the child's
/// model-auth env must be handled DIFFERENTLY in each. Collapsing them (the old
/// `Option<Routing>`) is exactly what let a selected-but-unresolved provider
/// silently inherit a shell-set `ANTHROPIC_BASE_URL`:
///   - `Via` — a credential resolved: SET it, and CLEAR the vendor's other auth
///     vars so no stray inherited value shadows or redirects it.
///   - `Inherit` — `--no-route` (or an unconfigured, signed-out run): the backend
///     uses its OWN model account, so the child's inherited model-auth env is left
///     exactly as the user's shell has it — the pass-through `--no-route` promises.
///   - `FailClosed` — routing was INTENDED (a provider is selected) but NO
///     credential resolved. Set nothing AND clear the vendor's model-auth env, so
///     a run never ships prompts+code to an inherited/attacker-set endpoint.
#[derive(Debug, Clone)]
pub enum Route {
    Via(Routing),
    Inherit,
    FailClosed,
}

impl Route {
    /// The resolved credential+destination, if one was found. `Inherit` and
    /// `FailClosed` carry none, so both read as `None` — what the banner + status
    /// line want (they only distinguish "routing on → where" from "off").
    pub fn via(&self) -> Option<&Routing> {
        match self {
            Route::Via(r) => Some(r),
            Route::Inherit | Route::FailClosed => None,
        }
    }
}

/// How much of the coding agent's actions run without a per-action prompt — the
/// resolved `autoApprove` decision, mapped by each backend to its own flags.
///
/// Three states, so the user's confirmed always-on default and the two opt-outs
/// each have exactly one representation (resolved once in [`super::resolve_approval`]):
///   - `Ask` — the backend's own ask-for-permission mode (`--ask`/`--safe`, or
///     `autoApprove: false`). Nothing is added; the user's sandbox governs.
///   - `Auto` — auto-approve, sandbox KEPT (the default). Claude
///     `--dangerously-skip-permissions`; `dev` `approval_policy=never` +
///     `sandbox_mode=workspace-write`.
///   - `Bypass` — auto-approve AND drop the sandbox (`--no-sandbox`, a deliberate
///     per-invocation escalation, never a persisted default). `dev`
///     `--dangerously-bypass-approvals-and-sandbox`; Claude is already sandbox-free
///     under skip-permissions, so `Bypass` maps there identically to `Auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Ask,
    Auto,
    Bypass,
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
    /// The resolved model-routing decision: set a credential (`Via`), inherit the
    /// shell's (`Inherit`, `--no-route`), or fail closed and clear it
    /// (`FailClosed`, a selected provider with no usable key).
    pub routing: Route,
    /// How much of the agent's actions auto-approve (the resolved `autoApprove`
    /// decision). Orthogonal to routing — it governs the backend's permission mode,
    /// not where model calls go. Each backend maps it to its own flags.
    pub approval: Approval,
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
    /// The route is what decides which config home the run wrote it in, so it must
    /// be the SAME value [`Backend::build`] launched with — a path resolved against
    /// the other home names a file that never exists.
    fn transcript_path(&self, route: &Route, cwd: &Path, backend_session_id: &str) -> Option<PathBuf>;
}

/// Resolve a backend kind to its implementation.
pub fn resolve(kind: BackendKind) -> Box<dyn Backend> {
    match kind {
        BackendKind::Dev => Box::new(Agent::DEV),
        BackendKind::Claude => Box::new(Claude),
        BackendKind::Codex => Box::new(Agent::CODEX),
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

    fn sel(positional: Option<&str>, tail: Option<&str>, named: Option<&str>) -> Selection {
        Selection {
            positional: positional.map(String::from),
            tail: tail.map(String::from),
            named: named.map(String::from),
        }
    }

    /// The point of [`select`]: every spelling is the SAME resolution. If these
    /// ever disagree, someone has forked the command.
    #[test]
    fn every_spelling_resolves_to_the_same_backend() {
        for (positional, named) in [(Some("dev"), None), (None, Some("dev"))] {
            let (kind, task) = select(sel(positional, None, named)).unwrap();
            assert_eq!(kind, BackendKind::Dev);
            assert_eq!(task, None);
        }
        for (positional, named) in [(Some("claude"), None), (None, Some("claude"))] {
            assert_eq!(select(sel(positional, None, named)).unwrap().0, BackendKind::Claude);
        }
        for (positional, named) in [(Some("codex"), None), (None, Some("codex"))] {
            assert_eq!(select(sel(positional, None, named)).unwrap().0, BackendKind::Codex);
        }
    }

    /// Naming nothing runs OUR agent — `hanzo code` and a bare `hanzo` alike.
    #[test]
    fn the_default_backend_is_ours() {
        assert_eq!(select(sel(None, None, None)).unwrap().0, BackendKind::Dev);
    }

    /// A lone operand is the TASK unless it is exactly a backend name — the one
    /// rule that lets `hanzo code dev` and `hanzo code "fix it"` both work.
    #[test]
    fn a_lone_operand_is_a_task_unless_it_names_a_backend() {
        let (kind, task) = select(sel(Some("fix the failing test"), None, None)).unwrap();
        assert_eq!(kind, BackendKind::Dev, "an unrecognised operand must not change the backend");
        assert_eq!(task.as_deref(), Some("fix the failing test"));

        // `hanzo code claude "fix it"` — backend positionally, task after it.
        let (kind, task) = select(sel(Some("claude"), Some("fix it"), None)).unwrap();
        assert_eq!(kind, BackendKind::Claude);
        assert_eq!(task.as_deref(), Some("fix it"));
    }

    /// Naming the backend twice is refused outright. A precedence rule here is
    /// one nobody could remember, and silently running an agent the user did not
    /// name is worse than refusing.
    #[test]
    fn naming_the_backend_twice_is_an_error() {
        let err = select(sel(Some("claude"), None, Some("codex"))).unwrap_err().to_string();
        assert!(err.contains("named twice"), "got: {err}");
        assert!(err.contains("claude") && err.contains("codex"), "both spellings named: {err}");

        // An unquoted multi-word task is a mistake worth catching, not a silent join.
        let err = select(sel(Some("fix"), Some("it"), None)).unwrap_err().to_string();
        assert!(err.contains("unexpected argument"), "got: {err}");
    }

    #[test]
    fn backend_kind_parse() {
        assert_eq!(BackendKind::parse("dev").unwrap(), BackendKind::Dev);
        assert_eq!(BackendKind::parse("claude").unwrap(), BackendKind::Claude);
        assert_eq!(BackendKind::parse("CC").unwrap(), BackendKind::Claude);
        // `codex` is its OWN backend. Aliasing it to `dev` would silently run a
        // different agent than the one the user named.
        assert_eq!(BackendKind::parse("codex").unwrap(), BackendKind::Codex);
        assert!(BackendKind::parse("gpt").is_err());
    }

    /// LOW-2: `Routing`'s `Debug` must REDACT the secret. No path prints it today,
    /// but `-vvv` wires `trace!`, so a future `trace!(?routing)` would otherwise
    /// leak an `sk-ant-…`/`hk-…`/bearer. The non-secret `api` stays for debugging.
    #[test]
    fn routing_debug_redacts_the_secret() {
        let g = Routing::Gateway { api: "https://api.hanzo.ai".into(), token: "hk-SECRET-TOKEN".into(), model: "enso".into(), small_fast_model: "enso-flash".into(), context_window: 1_000_000 };
        let s = format!("{g:?}");
        assert!(!s.contains("hk-SECRET-TOKEN"), "token leaked in Debug: {s}");
        assert!(s.contains("***"), "expected a redaction marker: {s}");
        assert!(s.contains("api.hanzo.ai"), "the non-secret api should survive: {s}");

        assert!(!format!("{:?}", Routing::Anthropic { key: "sk-ant-SECRET".into() }).contains("sk-ant-SECRET"));
        assert!(!format!("{:?}", Routing::OpenAI { key: "sk-proj-SECRET".into() }).contains("sk-proj-SECRET"));

        // `Route` composes the redacting `Debug`, so wrapping never re-exposes it.
        let r = Route::Via(Routing::Gateway { api: "x".into(), token: "hk-INNER".into(), model: "enso".into(), small_fast_model: "enso-flash".into(), context_window: 1_000_000 });
        assert!(!format!("{r:?}").contains("hk-INNER"), "Route::Via leaked the inner secret");
    }

    /// `Route::via()` yields the credential only for `Via`; the two no-credential
    /// outcomes both read as `None` (what the banner/status line consume).
    #[test]
    fn route_via_exposes_only_the_resolved_credential() {
        assert!(Route::Via(Routing::OpenAI { key: "sk-x".into() }).via().is_some());
        assert!(Route::Inherit.via().is_none());
        assert!(Route::FailClosed.via().is_none());
    }
}
