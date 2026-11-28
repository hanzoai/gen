//! The coding-backend seam: ONE trait every backend satisfies, so the
//! orchestrator (register → spawn → stream → finalize) is identical for all of
//! them: our own `dev` (the default), `claude`, `codex` and `interpreter`, and
//! every agent in [`super::agents`], one file each. They are distinct products,
//! never aliases of one another.
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
use super::agents::Session;
use crate::root::Root;

/// Which coding agent to wrap. Distinct products — never aliases of each
/// other: a user who names one gets THAT agent, or a clear failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Our own coding agent (hanzoai/dev). The default.
    Dev,
    Claude,
    Codex,
    /// Open Interpreter, a Codex fork that kept its grammar.
    Interpreter,
    /// Any agent in [`super::agents::AGENTS`]: Cursor, Antigravity, Gemini, Qwen, …
    Tool(&'static super::agents::Agent),
}

impl BackendKind {
    /// Parse a backend name. Our three are spelled here; every other agent's
    /// spellings live in its row of [`super::agents::AGENTS`].
    pub fn parse(s: &str) -> Result<BackendKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dev" => Ok(BackendKind::Dev),
            "claude" | "claude-code" | "cc" => Ok(BackendKind::Claude),
            "codex" => Ok(BackendKind::Codex),
            "interpreter" | "open-interpreter" | "oi" => Ok(BackendKind::Interpreter),
            other => super::agents::find(other)
                .map(BackendKind::Tool)
                .ok_or_else(|| anyhow::anyhow!("unknown backend '{other}' (expected: {})", expected())),
        }
    }

    /// The canonical spelling, which is the one [`BackendKind::parse`] accepts
    /// first. Kept beside that table so a name can be written and read back.
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Dev => "dev",
            BackendKind::Claude => "claude",
            BackendKind::Codex => "codex",
            BackendKind::Interpreter => "interpreter",
            BackendKind::Tool(a) => a.name,
        }
    }

    /// Does this operand NAME a backend (as opposed to being a task)? Reads the
    /// same table [`BackendKind::parse`] does, so the two can never disagree.
    pub fn names(s: &str) -> bool {
        BackendKind::parse(s).is_ok()
    }

    /// Every backend's canonical name, ours first.
    pub fn all() -> impl Iterator<Item = &'static str> {
        ["dev", "claude", "codex", "interpreter"].into_iter().chain(super::agents::AGENTS.iter().map(|a| a.name))
    }

    /// Whether the agent keeps its own login and never takes the Hanzo route.
    pub fn own_account_only(self) -> bool {
        matches!(self, BackendKind::Tool(a) if !a.routable)
    }
}

/// The backend names offered in help text and in the "unknown backend" error.
pub fn expected() -> String {
    BackendKind::all().collect::<Vec<_>>().join(" | ")
}

/// The default backend when no spelling names one: OUR agent.
pub const DEFAULT: BackendKind = BackendKind::Dev;

/// How a backend was named on the command line. The CLI offers several spellings
/// —
///
/// ```text
/// hanzo dev              hanzo dev --dev       hanzo dev --backend dev
/// hanzo dev --claude
/// hanzo dev --codex
/// hanzo dev --agy
/// hanzo dev --cursor     hanzo dev --agent
/// hanzo claude           (a bare session, the backend named as a word)
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
    /// The second positional — only reachable as a bare `hanzo <backend> <task>`.
    pub tail: Option<String>,
    /// `--backend X`, or whichever of `--claude` / `--codex` / `--dev` was set.
    /// clap's arg group guarantees at most one of those reaches us.
    pub named: Option<String>,
    /// `agent` from `~/.hanzo/settings.json`, if the reader set one. Passed IN
    /// rather than read here so this stays a pure function of the command line
    /// plus one value — the file is the caller's business, and every case below
    /// is testable without a `$HOME`.
    pub configured: Option<String>,
}

/// Resolve `(backend, task)` from every spelling at once.
///
/// The one rule for the positional operand: it is the BACKEND if it is exactly a
/// backend name, otherwise it is the TASK. So a bare `hanzo claude` picks a backend
/// and `hanzo dev "fix the test"` states a task, with no flag needed for either.
/// `hanzo dev` itself refuses the word form: its tool is a flag.
///
/// Naming the backend twice is an error rather than a precedence rule, because a
/// precedence rule here is one nobody could remember: `hanzo claude --codex`
/// has no defensible winner, so it gets a clean message instead of a silent pick.
pub fn select(sel: Selection) -> Result<(BackendKind, Option<String>)> {
    let positional_names_backend = sel.positional.as_deref().is_some_and(BackendKind::names);

    if positional_names_backend {
        if let Some(named) = &sel.named {
            let pos = sel.positional.as_deref().unwrap_or_default();
            anyhow::bail!(
                "backend named twice: `{pos}` and `--{named}` — name it once \
                 (`hanzo {pos}` or `hanzo --{named}`)"
            );
        }
        let kind = BackendKind::parse(sel.positional.as_deref().unwrap_or_default())?;
        return Ok((kind, sel.tail));
    }

    // The positional is a task (or absent), so a second one has nothing to bind to.
    if let Some(extra) = &sel.tail {
        anyhow::bail!(
            "unexpected argument '{extra}' — a task is ONE argument; quote it \
             (`hanzo dev \"fix the failing test\"`)"
        );
    }

    // Most specific first: a name on the command line, then the reader's
    // configured agent, then ours. A bad value in the file is an ERROR, not a
    // fallback — the three are different products, and quietly running `dev` for
    // someone who asked for `codex` is the failure this ordering exists to avoid.
    let kind = match (sel.named.as_deref(), sel.configured.as_deref()) {
        (Some(name), _) => BackendKind::parse(name)?,
        (None, Some(agent)) => BackendKind::parse(agent)
            .map_err(|e| anyhow::anyhow!("{e} — from `agent` in ~/.hanzo/settings.json"))?,
        (None, None) => DEFAULT,
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
///   stored `sk-` gateway key). The recommended path.
/// - `Anthropic` / `OpenAI` — the vendor's own API, reached directly with the
///   user's own key. Not metered by Hanzo; billed by the vendor.
///
/// `Debug` is hand-written to REDACT the secret (below): a run at `-vvv` wires
/// `trace!`, so a stray `trace!(?routing)` must never print an `sk-ant-…`/`hk-…`/
/// bearer — only the non-secret `api` survives.
#[derive(Clone)]
pub enum Routing {
    /// Route through the Hanzo gateway; `token` is the hanzo.id bearer / `sk-` key.
    Gateway {
        /// The active network's api origin (e.g. https://api.hanzo.ai).
        api: String,
        /// The bearer/key authenticating gateway model calls.
        token: String,
        /// The active Hanzo organization (`X-Org-Id` / account owner), if known,
        /// forwarded as `HANZO_ORG` so `hanzo-mcp` scopes cloud calls to the same org.
        org: Option<String>,
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
            Routing::Gateway { api, .. } => f
                .debug_struct("Gateway")
                .field("api", api)
                .field("token", &"***")
                .finish(),
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
    /// Whether model calls go through the Hanzo gateway: the one route on which the
    /// child already holds the IAM bearer, so the only one that may carry the
    /// cloud's tools ([`Spec::cloud`]).
    pub fn hanzo(&self) -> bool {
        matches!(self, Route::Via(Routing::Gateway { .. }))
    }

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

/// The caller's tools on the cloud, as ONE streamable-HTTP MCP server at
/// `{api}/v1/mcp` ([`super::kit`]). The IAM bearer reaches the child only in its
/// environment, as [`Remote::TOKEN`], which each backend's MCP configuration names
/// and never spells — the same rule the routing credential follows — and only on
/// a Hanzo-routed run ([`Spec::cloud`]).
#[derive(Clone)]
pub struct Remote {
    pub url: String,
    pub token: String,
}

impl Remote {
    /// The server's key in every backend's MCP configuration. `hanzo` is the local
    /// hanzo-mcp and `run` is the cloud agent's ask server, so this is neither.
    pub const NAME: &'static str = "cloud";
    /// The environment variable the bearer rides in.
    pub const TOKEN: &'static str = "HANZO_MCP_TOKEN";
}

impl std::fmt::Debug for Remote {
    /// Redacted, for the reason [`Routing`]'s is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote").field("url", &self.url).field("token", &"***").finish()
    }
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
    /// Optional resolved reasoning effort level (`auto`, `off`/`none`,
    /// `minimal`, `low`, `medium`, `high`, `xhigh`, `max`).
    pub effort: Option<crate::reasoning::ReasoningEffort>,
    /// How much of the agent's actions auto-approve (the resolved `autoApprove`
    /// decision). Orthogonal to routing — it governs the backend's permission mode,
    /// not where model calls go. Each backend maps it to its own flags.
    pub approval: Approval,
    pub mcp: Option<McpAttach>,
    /// The caller's cloud tools, attached beside `mcp` when the org's kit answered.
    pub remote: Option<Remote>,
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
    /// The command that prints the signed-in account's access token, fresh
    /// ([`crate::Host::refresh`]): what an agent that outlives one sign-in
    /// token runs for the next.
    pub refresh: Vec<String>,
    /// Where this run keeps what it writes ([`Root`]).
    pub root: Root,
}

impl Spec {
    /// The cloud tools this run attaches, and so whether the IAM bearer enters
    /// the child's environment: only when model calls route through the Hanzo
    /// gateway, where the child already holds that bearer. A `--no-route` or
    /// direct-provider run never receives it, whatever `remote` says — its agent
    /// runs shell commands without asking, and any of them could read the env.
    pub fn cloud(&self) -> Option<&Remote> {
        self.remote.as_ref().filter(|_| self.routing.hanzo())
    }
}

/// A ready-to-spawn command plus temp files that must outlive the child (e.g. a
/// Claude `--mcp-config` file). Dropping `cleanup` deletes them.
pub struct Launch {
    pub command: tokio::process::Command,
    pub cleanup: Scratch,
    /// When set, every turn of this run is wrapped in a read-only workspace
    /// sandbox before its child starts (routed read-only runs only).
    pub readonly: Option<crate::runner::readonly::Recipe>,
}

/// What a launch made for its one run, removed when the run ends: config files,
/// and the homes of their own that agents are pointed at (so two sessions of one
/// agent never read each other's project or ledger).
#[derive(Default)]
pub struct Scratch {
    pub files: Vec<tempfile::TempPath>,
    /// Held while the session lives, released before its homes go.
    pub locks: Vec<std::fs::File>,
    pub dirs: Vec<tempfile::TempDir>,
}

pub trait Backend: Send + Sync {
    /// The `agent` label recorded on the cloud session ("claude" | "dev").
    fn label(&self) -> &'static str;

    /// Best-effort backend version string for the context snapshot.
    fn version(&self, root: &Root) -> Option<String>;

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
    fn transcript_path(
        &self,
        root: &Root,
        route: &Route,
        cwd: &Path,
        backend_session_id: &str,
    ) -> Option<PathBuf>;
}

/// Resolve a backend kind to its implementation.
pub fn resolve(kind: BackendKind) -> Box<dyn Backend> {
    match kind {
        BackendKind::Dev => Box::new(Agent::DEV),
        BackendKind::Claude => Box::new(Claude),
        BackendKind::Codex => Box::new(Agent::CODEX),
        BackendKind::Interpreter => Box::new(Agent::INTERPRETER),
        BackendKind::Tool(agent) => Box::new(Session::new(agent)),
    }
}

/// Resolve how to launch hanzo-mcp as a stdio server scoped to `cwd`: the native
/// binary hanzo.sh installed beside this one, else `~/.local/bin/hanzo-mcp` under
/// `root`, else the first on the search path. Returns `None` when there is none.
pub fn resolve_mcp_in(root: &Root, cwd: &Path, ledger: Option<&Path>) -> Option<McpAttach> {
    let exe = std::env::current_exe().ok();
    let program = beside(exe.as_deref(), MCP_BIN)
        .or_else(|| local_mcp(root))
        .or_else(|| crate::exe::find(MCP_BIN))?;
    Some(attach_mcp(program, exe, cwd, ledger))
}

/// Resolve how to launch hanzo-mcp as a stdio server scoped to `cwd`: the native
/// binary hanzo.sh installed beside this one, else the first on the search path.
/// Returns `None` when there is none — MCP is an enhancement, so a missing server
/// never blocks the session (the caller warns and continues without it). `ledger`
/// is where the proxy records each tool call for the session summary.
pub fn resolve_mcp(cwd: &Path, ledger: Option<&Path>) -> Option<McpAttach> {
    let exe = std::env::current_exe().ok();
    let program = beside(exe.as_deref(), MCP_BIN).or_else(|| crate::exe::find(MCP_BIN))?;
    Some(attach_mcp(program, exe, cwd, ledger))
}

/// Resolve `hanzo-mcp` if already present, or auto-bootstrap the verified native
/// release binary into `<root>/.local/bin/hanzo-mcp` on first run so `hanzo`
/// always has the full native toolset out of the box.
pub async fn resolve_or_install_mcp(root: &Root, cwd: &Path, ledger: Option<&Path>) -> Option<McpAttach> {
    if let Some(found) = resolve_mcp_in(root, cwd, ledger) {
        return Some(found);
    }
    let installed = ensure_mcp_installed(root).await?;
    let exe = std::env::current_exe().ok();
    Some(attach_mcp(installed, exe, cwd, ledger))
}

/// Resolve or auto-install `hanzo-mcp` directly (without wrapping through
/// `<current_exe> __mcp`), for in-process embedded hosts (`hanzod`, Desktop, Mobile)
/// whose own executable is not the `gen`/`hanzo` CLI `__mcp` entrypoint.
pub async fn resolve_or_install_mcp_direct(root: &Root, cwd: &Path) -> Option<McpAttach> {
    let exe = std::env::current_exe().ok();
    let program = match beside(exe.as_deref(), MCP_BIN)
        .or_else(|| local_mcp(root))
        .or_else(|| crate::exe::find(MCP_BIN))
    {
        Some(found) => found,
        None => ensure_mcp_installed(root).await?,
    };
    Some(McpAttach {
        program: program.display().to_string(),
        args: vec!["--project-dir".into(), cwd.display().to_string()],
    })
}

fn attach_mcp(program: PathBuf, exe: Option<PathBuf>, cwd: &Path, ledger: Option<&Path>) -> McpAttach {
    let direct = McpAttach {
        program: program.display().to_string(),
        args: vec!["--project-dir".into(), cwd.display().to_string()],
    };
    lazy(direct, exe, ledger)
}

fn local_mcp(root: &Root) -> Option<PathBuf> {
    let p = root.home().join(".local").join("bin").join(MCP_BIN);
    p.is_file().then_some(p)
}

/// Default release download base for `hanzo-mcp`.
#[cfg(not(test))]
const MCP_RELEASE_BASE: &str = "https://github.com/hanzoai/mcp/releases/latest/download";

/// Map `(std::env::consts::OS, std::env::consts::ARCH)` to the published
/// `hanzoai/mcp` release target name (matching `hanzo.sh` and `cli/npm/install.js`).
pub fn mcp_release_target(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("linux-amd64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("macos", "x86_64") => Some("darwin-amd64"),
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("windows", "x86_64") => Some("windows-amd64"),
        _ => None,
    }
}

/// Ensure `hanzo-mcp` is installed under `<root>/.local/bin/hanzo-mcp` during a
/// real user session when `HANZO_NO_AUTO_MCP != "1"` and `root` is writable.
#[cfg(not(test))]
pub async fn ensure_mcp_installed(root: &Root) -> Option<PathBuf> {
    if matches!(
        std::env::var("HANZO_NO_AUTO_MCP").ok().as_deref(),
        Some("1" | "true")
    ) {
        return None;
    }
    if !root.home().is_dir() || super::kit::private_dir(&root.hanzo()).is_err() {
        return None;
    }
    let target = mcp_release_target(std::env::consts::OS, std::env::consts::ARCH)?;
    let base = std::env::var("HANZO_MCP_RELEASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| MCP_RELEASE_BASE.to_string());
    match ensure_mcp_installed_from(root, &base, target).await {
        Ok(path) => Some(path),
        Err(e) => {
            crate::warn(&format!("could not auto-install hanzo-mcp ({e})"));
            None
        }
    }
}

/// Unit tests never make unmocked downloads; tests exercise [`ensure_mcp_installed_from`]
/// directly against a local loopback server.
#[cfg(test)]
pub async fn ensure_mcp_installed(_root: &Root) -> Option<PathBuf> {
    None
}

/// Download `hanzo-mcp-<target>.tar.gz` + `.sha256` from `base_url`, verify its
/// SHA-256 digest BEFORE unpacking, and install atomically to
/// `<root>/.local/bin/hanzo-mcp` (mode `0755`).
pub async fn ensure_mcp_installed_from(root: &Root, base_url: &str, target: &str) -> Result<PathBuf> {
    use anyhow::{bail, Context};
    use sha2::{Digest, Sha256};

    if !root.home().is_dir() {
        bail!("root directory {} does not exist", root.home().display());
    }
    let bin_dir = root.home().join(".local").join("bin");
    std::fs::create_dir_all(&bin_dir)
        .with_context(|| format!("creating {}", bin_dir.display()))?;

    let asset = format!("hanzo-mcp-{target}.tar.gz");
    let url = format!("{}/{asset}", base_url.trim_end_matches('/'));
    let sha_url = format!("{url}.sha256");

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .context("building HTTP client for hanzo-mcp bootstrap")?;

    let sum_bytes = client
        .get(&sha_url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("downloading {sha_url}"))?
        .bytes()
        .await
        .with_context(|| format!("reading {sha_url}"))?;
    let want = String::from_utf8_lossy(&sum_bytes)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if want.len() != 64 || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{asset}.sha256 is not a valid SHA-256 digest");
    }

    let tarball = client
        .get(&url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("downloading {url}"))?
        .bytes()
        .await
        .with_context(|| format!("reading {url}"))?;
    let got = format!("{:x}", Sha256::digest(&tarball));
    if got != want {
        bail!("{asset} SHA-256 mismatch (expected {want}, got {got})");
    }

    let exe_name = if target.starts_with("windows") { "hanzo-mcp.exe" } else { "hanzo-mcp" };
    let staged = bin_dir.join(format!(".{exe_name}.{:016x}.tmp", rand::random::<u64>()));
    let unpack_res = (|| -> Result<()> {
        let mut found = false;
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&tarball[..]));
        for entry in archive.entries().context("reading hanzo-mcp archive")? {
            let mut entry = entry.context("reading hanzo-mcp archive entry")?;
            let matches = entry
                .path()
                .ok()
                .and_then(|p| p.file_name().map(|n| n == exe_name))
                .unwrap_or(false);
            if matches {
                entry.unpack(&staged).with_context(|| format!("unpacking {exe_name}"))?;
                found = true;
                break;
            }
        }
        if !found {
            bail!("{asset} did not contain {exe_name}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("setting mode 0755 on {}", staged.display()))?;
        }
        Ok(())
    })();
    if let Err(e) = unpack_res {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }

    let dest = bin_dir.join(exe_name);
    if let Err(e) = std::fs::rename(&staged, &dest) {
        let _ = std::fs::remove_file(&staged);
        return Err(e).with_context(|| format!("installing {}", dest.display()));
    }
    Ok(dest)
}

/// The MCP server's binary name, as hanzo.sh installs it.
pub const MCP_BIN: &str = if cfg!(windows) { "hanzo-mcp.exe" } else { "hanzo-mcp" };

/// `name` in the directory `exe` lives in, when it is there: the tools hanzo.sh
/// installs together are found together, whatever PATH puts first.
fn beside(exe: Option<&Path>, name: &str) -> Option<PathBuf> {
    let p = exe?.canonicalize().ok()?.parent()?.join(name);
    p.is_file().then_some(p)
}

/// The server behind this binary's own lazy proxy ([`super::mcp`]): the agent's wait
/// for it to start is the proxy's, which answers from the last time the server spoke.
/// Without a path to this binary the server is attached as it is.
fn lazy(direct: McpAttach, exe: Option<std::path::PathBuf>, ledger: Option<&Path>) -> McpAttach {
    let Some(exe) = exe else { return direct };
    let mut args = vec!["__mcp".to_string()];
    if let Some(l) = ledger {
        args.extend(["--ledger".to_string(), l.display().to_string()]);
    }
    args.push("--".to_string());
    args.push(direct.program);
    args.extend(direct.args);
    McpAttach { program: exe.display().to_string(), args }
}

/// How long a backend has to say its version at startup.
pub const VERSION_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

#[cfg(test)]
mod tests;
