//! `hanzo code` — wrap a local coding agent (Claude Code or `dev`) so a
//! developer's terminal session is (opt-in) linked, live-streamed and tracked in
//! Hanzo cloud, with the Hanzo MCP toolset attached and model usage metered
//! universally through the Hanzo gateway.
//!
//! Three things are wired natively:
//!   1. Session link + live stream — register on `/v1/agents/sessions`, forward
//!      the backend's structured events, and mark the terminal status. ON by
//!      default when signed in (streams the user's OWN session to their OWN org,
//!      derived server-side from the JWT `owner`); `--no-link`, or a persisted
//!      `code.link = false`, opts out. Structurally silent when unauthenticated.
//!   2. Hanzo MCP — attached in-session (Claude `--mcp-config`, `dev` `-c`).
//!   3. hanzo.id auth + universal usage — model calls route through
//!      api.hanzo.ai so tokens/cost meter into cloud_usage/o11y regardless of
//!      which account/machine the dev is on.
//!
//! Sessions are PORTABLE: the register carries a no-secret context snapshot, the
//! backend's own resume handle + a transcript pointer are persisted, and
//! `--resume <sessionId>` restores cwd/repo/ref and relaunches the backend with
//! its native resume against the same cloud session.

mod backend;
mod claude;
mod context;
mod dev;
mod event;
mod session;
mod target;
mod theme;
#[cfg(test)]
pub(crate) mod testmock;

use anyhow::{anyhow, Context, Result};
use colored::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncSeekExt};

use crate::config::Config;
use crate::iam::identity::Identity;
use crate::iam::provider::{self, Provider};
use crate::{commands::network, iam::store};

use backend::{resolve, resolve_mcp, BackendKind, Backend, Launch, Mode, Route, Routing, Spec};
use context::{ResumeRecord, Snapshot};
use event::{Kind, Mapped, Status, Usage};
use session::SessionClient;

/// Parsed `hanzo code` invocation.
pub struct Options {
    pub backend: String,
    pub link: bool,
    pub no_link: bool,
    pub route: bool,
    pub mcp: bool,
    pub project_mcp: bool,
    pub resume: Option<String>,
    pub brand: String,
    /// Claude theme to apply (None → the persisted `code.theme`; "none" → skip).
    pub theme: Option<String>,
    pub task: Option<String>,
    pub passthrough: Vec<String>,
}

/// Decide whether to stream to cloud: an explicit `--no-link` always wins, then
/// `--link`, else the persisted default (`code.link`, ON by default). This only
/// decides INTENT — the caller still gates on auth, so an unauthenticated run
/// never streams regardless of what this returns.
pub(crate) fn effective_link(link: bool, no_link: bool, persisted: bool) -> bool {
    if no_link {
        false
    } else if link {
        true
    } else {
        persisted
    }
}

/// The auth gate for registering this machine as a cloud run-target — the SAME
/// structural gate as the session link. Link INTENT alone is not enough: without a
/// bearer nothing is built and nothing reaches cloud, so an unauthenticated run
/// never registers a target, exactly as it never streams a session.
pub(crate) fn links_target(do_link: bool, has_bearer: bool) -> bool {
    do_link && has_bearer
}

/// Resolve the working directory, turning a missing/`ENOENT` cwd into a CLEAR
/// message instead of the cryptic `resolving current dir` chain.
///
/// A fresh or odd environment must never die cryptically: `std::env::current_dir`
/// fails when the process's cwd was deleted or is unreadable, and a bare `hanzo`
/// there is exactly the first thing a new user might hit. Pure over the
/// `io::Result` so the message is unit-testable without touching the real cwd.
fn cwd_or_friendly(r: std::io::Result<PathBuf>) -> Result<PathBuf> {
    r.map_err(|e| {
        anyhow!(
            "current directory is unavailable ({e}) — it may have been deleted, or you may lack \
             permission to it. `cd` into a directory that exists and run `hanzo` again."
        )
    })
}

/// A credential source to try for a routed run, in preference order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cred {
    /// The active identity's hanzo.id bearer — already in hand, no Vault read.
    Bearer,
    /// A stored `hk-` Hanzo gateway key.
    HanzoKey,
    /// A stored `sk-ant-` Anthropic key (direct to api.anthropic.com).
    AnthropicKey,
    /// A stored `sk-` OpenAI key (direct to api.openai.com).
    OpenAIKey,
}

/// The ordered credential preference for a routed run, from the active provider,
/// the backend, and whether a bearer is held. PURE + testable — the impure
/// resolver walks this and reads the Vault lazily, so the precedence lives in
/// exactly one place.
///
/// A DIRECT provider is tried ONLY when it matches the backend it can drive
/// (Anthropic↔Claude, OpenAI↔dev); the Hanzo gateway (bearer, then `hk-` key) is
/// always the fallback, so a signed-in user keeps routing even if a direct key
/// is absent or paired with the wrong backend.
fn route_plan(backend: BackendKind, provider: Option<&str>, has_bearer: bool) -> Vec<Cred> {
    let mut plan = Vec::new();
    match (provider, backend) {
        (Some("anthropic"), BackendKind::Claude) => plan.push(Cred::AnthropicKey),
        (Some("openai"), BackendKind::Dev) => plan.push(Cred::OpenAIKey),
        _ => {}
    }
    if has_bearer {
        plan.push(Cred::Bearer);
    }
    plan.push(Cred::HanzoKey);
    plan
}

/// Resolve the routing for this run by walking [`route_plan`] and taking the
/// first credential actually held. Provider keys are read from the Vault LAZILY
/// (only as the plan reaches them), so the common gateway path (bearer in hand)
/// does zero extra keychain reads, and `--no-route` does none at all.
///
/// Returns a [`Route`] — the three-way decision the backend needs to treat the
/// child's model-auth env correctly. `--no-route` ⇒ `Inherit`; a resolved
/// credential ⇒ `Via`; nothing resolved ⇒ [`unresolved_route`] (fail closed iff a
/// provider was selected, else inherit an unconfigured backend's own account).
fn resolve_routing(
    cfg: &Config,
    route: bool,
    backend: BackendKind,
    api: &str,
    bearer: Option<&str>,
) -> Result<Route> {
    if !route {
        // `--no-route`: the backend uses its OWN account; we touch no env.
        return Ok(Route::Inherit);
    }
    let provider = cfg.auth.provider.as_deref();
    for cred in route_plan(backend, provider, bearer.is_some()) {
        match cred {
            Cred::Bearer => {
                if let Some(token) = bearer {
                    return Ok(Route::Via(Routing::Gateway { api: api.to_string(), token: token.to_string() }));
                }
            }
            Cred::HanzoKey => {
                if let Some(token) = provider::key(Provider::Hanzo)? {
                    return Ok(Route::Via(Routing::Gateway { api: api.to_string(), token }));
                }
            }
            Cred::AnthropicKey => {
                if let Some(key) = provider::key(Provider::Anthropic)? {
                    return Ok(Route::Via(Routing::Anthropic { key }));
                }
            }
            Cred::OpenAIKey => {
                if let Some(key) = provider::key(Provider::OpenAI)? {
                    return Ok(Route::Via(Routing::OpenAI { key }));
                }
            }
        }
    }
    Ok(unresolved_route(provider.is_some()))
}

/// The routing outcome when the credential plan resolves NOTHING.
///
/// A SELECTED provider means the user EXPECTS that route, so silently inheriting a
/// shell-set `ANTHROPIC_BASE_URL`/key would ship prompts+code somewhere they never
/// chose — fail CLOSED (the backend then clears its vendor's model-auth env). With
/// NO provider selected there is no such expectation (an unconfigured or
/// signed-out run), so the backend keeps its OWN account, exactly as a bare
/// `claude`/`dev` would — inheriting the shell is the honest, unchanged behavior.
fn unresolved_route(provider_selected: bool) -> Route {
    if provider_selected {
        Route::FailClosed
    } else {
        Route::Inherit
    }
}

/// May the ACTIVE identity re-attach to the cloud session a resume record names?
///
/// THREE things are braided under the word "resume", and only one is org-scoped:
///   1. the backend's conversation (`~/.claude/projects/<slug>/<sid>.jsonl`) — LOCAL
///   2. this CLI's store (cloud-id ↔ backend-sid ↔ cwd)                      — LOCAL
///   3. the cloud session record                                    — ORG-SCOPED
///
/// Across an org boundary (1) and (2) carry perfectly; (3) cannot. The gateway
/// injects the JWT `owner` claim as the org, so `GET /v1/agents/sessions/{id}`
/// for another org's session is refused — that is tenant isolation working
/// correctly, and it must NOT be routed around. So a resume under a different
/// identity keeps the full local conversation and registers a NEW cloud session,
/// billed to the now-active identity from turn one.
///
/// A cloud session id is addressable only from the (identity, cloud) that minted
/// it, so BOTH must match before we hand the id to `resolve_cloud_session`.
///
/// `None` ⇒ re-attach to the recorded id. `Some(reason)` ⇒ register fresh and SAY
/// so. Returning `None` for the target is also what keeps lineage honest:
/// `resolve_cloud_session` writes `resumedFrom` only for an id it is handed, so a
/// blocked resume writes NO pointer. The new org's record must never reference a
/// session it cannot resolve; that lineage lives in the LOCAL store, the only
/// place it is true.
pub(crate) fn cloud_resume_block(
    rec_identity: &str,
    active: Option<&Identity>,
    rec_api: &str,
    active_api: &str,
) -> Option<String> {
    // Unlinked run: nothing reaches cloud, so there is no cloud session to own.
    let active = active?;

    // A session id minted by one cloud means nothing to another: resuming a
    // prod session after `hanzo network use local` would hand a foreign id to a
    // different control plane. Same filter as the run-target store (host+api).
    let (rec_api, active_api) = (rec_api.trim_end_matches('/'), active_api.trim_end_matches('/'));
    if !rec_api.is_empty() && rec_api != active_api {
        return Some(format!(
            "session was created on {rec_api}; you are on {active_api}. A cloud session cannot \
             move between networks, so your local conversation resumes with full context and a \
             NEW cloud session is registered on {active_api}, billed to {}.",
            active.owner
        ));
    }

    if rec_identity == active.to_string() {
        return None;
    }
    Some(match rec_identity {
        "" => format!(
            "this session predates identity tracking, so it cannot be matched to {active}. \
             Resuming your local conversation with full context; a NEW cloud session will be \
             registered and billed to {}.",
            active.owner
        ),
        other => format!(
            "session belongs to {other}; you are now {active}. A cloud session cannot move \
             between orgs, so your local conversation resumes with full context and a NEW cloud \
             session is registered, billed to {} from the first turn. \
             (`hanzo switch {other}` to go back to the original session.)",
            active.owner
        ),
    })
}

pub async fn run(cfg: &mut Config, opts: Options) -> Result<()> {
    let kind = BackendKind::parse(&opts.backend)?;
    let backend = resolve(kind);
    let mode = if opts.task.is_some() { Mode::Headless } else { Mode::Interactive };
    let api = network::active(cfg).api;

    // Auth: the ACTIVE identity's hanzo.id bearer from the OS keychain (never
    // argv/logged). The identity rides along because the cloud session this run
    // registers is org-scoped to its `owner` — the two must not drift.
    let (identity, bearer) = match store::active_token(cfg, &opts.brand)? {
        Some((id, t)) => (Some(id), Some(t.access_token)),
        None => (None, None),
    };

    // `owner/name` for the LOCAL resume record — never sent to cloud.
    let who = identity.as_ref().map(Identity::to_string).unwrap_or_default();

    let mut do_link = effective_link(opts.link, opts.no_link, cfg.code.link);
    if do_link && bearer.is_none() {
        warn("not signed in — run `hanzo login` to link this session. Continuing locally (no cloud stream).");
        do_link = false;
    }

    // Resume: restore cwd + the backend's own resume handle from the local store.
    let (cwd, resume_handle, resume_from) = match &opts.resume {
        Some(raw) => {
            // Accept the id with or without the `sess_` prefix (the resume line prints
            // the bare form), so `hanzo --resume <id>` matches either way.
            let id = &(if raw.starts_with("sess_") { raw.clone() } else { format!("sess_{raw}") });
            let rec = ResumeRecord::load(id)?.ok_or_else(|| {
                anyhow!(
                    "no local record for session {id} on this machine — resume runs where the session was created"
                )
            })?;
            let cwd = PathBuf::from(&rec.cwd);
            // Fail closed: resuming a backend in a directory that has vanished
            // (or was replaced by a file) would run it somewhere unintended.
            if !cwd.is_dir() {
                return Err(anyhow!(
                    "recorded working dir {} no longer exists — resume runs where the session was created",
                    rec.cwd
                ));
            }
            // Confirm the working tree is still the SAME project. A path can be
            // reused by a different checkout; surface that before relaunching.
            if !rec.repo.is_empty() {
                let now = context::Repo::capture(&cwd);
                if now.root != rec.repo.root || now.remote != rec.repo.remote {
                    warn(&format!(
                        "working dir {} is a different repository than when session {id} was recorded — resuming anyway",
                        rec.cwd
                    ));
                }
            }
            // The LOCAL conversation always resumes. The CLOUD id only carries
            // when the active identity, on the active network, owns it — see
            // `cloud_resume_block`.
            let attach = match cloud_resume_block(&rec.identity, identity.as_ref(), &rec.api, &api) {
                None => Some(id.clone()),
                Some(note) => {
                    warn(&note);
                    None
                }
            };
            (cwd, Some(rec.backend_session_id.clone()), attach)
        }
        None => (cwd_or_friendly(std::env::current_dir())?, None, None),
    };

    // MCP: resolve hanzo-mcp; a missing server warns but never blocks.
    let mcp = if opts.mcp {
        let m = resolve_mcp(&cwd);
        if m.is_none() {
            warn("hanzo-mcp not found (install `hanzo-mcp` or `uv`) — continuing without the Hanzo toolset.");
        }
        m
    } else {
        None
    };

    // Routing: which model endpoint this run's calls go to, and with what
    // credential — the Hanzo gateway (metered) for a Hanzo login, or a provider's
    // OWN API for a stored OpenAI/Anthropic key. `--no-route` opts out entirely.
    let routing = resolve_routing(cfg, opts.route, kind, &api, bearer.as_deref())?;
    // A SELECTED provider with no usable key fails closed: the backend clears its
    // model-auth env (below), and we say WHY rather than let the route silently
    // vanish into an inherited endpoint. `provider` is always `Some` here — it is
    // what makes the outcome `FailClosed` rather than `Inherit`.
    if matches!(routing, Route::FailClosed) {
        if let Some(p) = cfg.auth.provider.as_deref() {
            warn(&format!(
                "selected provider `{p}` has no usable key — run `hanzo login` (or pass `--no-route` \
                 to use the backend's own account). Model calls will NOT route, and the child's \
                 inherited model credentials are cleared."
            ));
        }
    }

    // For a linked interactive Claude run, pre-set the session id so its
    // transcript can be tailed; otherwise the resume handle names it.
    let preset_session = if do_link && mode == Mode::Interactive && kind == BackendKind::Claude && opts.resume.is_none() {
        Some(uuid_v4())
    } else {
        None
    };

    let snapshot = Snapshot::capture(&cwd, backend.label(), backend.version());

    // Cloud session (linked only). Resolve reuses a non-terminal resumed session
    // (same id) or forks a new one off a terminal / fresh session.
    let client = if do_link {
        Some(SessionClient::new(&api, bearer.as_deref().unwrap())?)
    } else {
        None
    };
    let mut session_id: Option<String> = None;
    if let Some(c) = &client {
        let title = session_title(&opts);
        match resolve_cloud_session(c, backend.label(), &title, resume_from.as_deref()).await {
            Ok((id, forked_from)) => {
                // The "where it runs" context snapshot (no secrets).
                let _ = c.event(&id, Kind::Status, snapshot.context_payload(forked_from.as_deref())).await;
                session_id = Some(id);
            }
            Err(e) => {
                // Fail-open for availability: never block the dev's work on a
                // cloud hiccup — degrade to a local (unlinked) run.
                warn(&format!("could not register session ({e}); continuing locally."));
            }
        }
    }

    // Register/refresh this machine as a cloud run-target so mission-control knows
    // WHICH computer the session runs on and whether it can take more work. DETACHED
    // and BEST-EFFORT: capability + live-metrics probing and the cloud write happen
    // off the critical path and can NEVER block or fail the coding session. Gated on
    // the same structural auth check as the session link (`links_target`) — an
    // unauthenticated run holds no bearer, spawns nothing here, and reaches cloud not
    // at all.
    if links_target(do_link, bearer.is_some()) {
        if let Some(token) = bearer.clone() {
            let api = api.clone();
            let machine_id = snapshot.machine_id.clone();
            let host = snapshot.host.clone();
            tokio::spawn(async move {
                let machine = context::Machine::capture().await;
                target::sync(&api, &token, &machine_id, &host, &machine).await;
            });
        }
    }

    // Claude theme (Dracula dark / Alucard light, auto by the user's preference).
    // Native — writes ~/.claude/themes + selects it; never patches Claude. The guard
    // restores the user's own theme when this session ends (any exit path). `dev`
    // has no Claude themes. Held to end-of-run so plain `claude` keeps its theme.
    let _theme_guard =
        (kind == BackendKind::Claude).then(|| theme::apply(opts.theme.as_deref(), &cfg.code.theme));

    banner(
        &opts,
        backend.label(),
        &cwd,
        &api,
        routing.via(),
        bearer.is_some(),
        session_id.as_deref(),
        None,
    );

    let structured = client.is_some() && session_id.is_some();
    let spec = Spec {
        mode,
        task: opts.task.clone(),
        cwd: cwd.clone(),
        routing,
        mcp,
        structured,
        preset_session: preset_session.clone(),
        // The `--project-mcp` / `--trust-project` opt-in trusts the repo: it both
        // loads the repo's own `.mcp.json` AND widens Claude's setting sources to
        // include project+local (hooks/statusLine). Off by default — an untrusted
        // repo's settings never load, so its hooks can't fire with the routing key
        // in env.
        trust_project: opts.project_mcp,
        resume: resume_handle.clone(),
        passthrough: opts.passthrough.clone(),
    };
    let launch = backend.build(&spec)?;

    // The session id we watch for the interactive transcript tail.
    let watch_sid = resume_handle.clone().or(preset_session);

    match mode {
        Mode::Headless => {
            let (outcome, ok) = run_headless(&*backend, structured, launch, client.clone(), session_id.clone()).await?;
            if let (Some(c), Some(id)) = (&client, &session_id) {
                let transcript = outcome
                    .backend_session
                    .as_ref()
                    .and_then(|bs| backend.transcript_path(&cwd, bs))
                    .map(|p| p.display().to_string());
                finalize(c, id, &outcome, ok, &snapshot, &api, &who, false, transcript).await;
                report_link(id);
            }
        }
        Mode::Interactive => {
            let ok = run_interactive(&*backend, launch, client.clone(), session_id.clone(), &cwd, watch_sid).await?;
            if let (Some(c), Some(id)) = (&client, &session_id) {
                // Interactive per-event stream arrives via the transcript tail;
                // the resume handle here is what we pre-set / resumed.
                let bs = resume_handle.or_else(|| preset_session_of(&spec));
                let outcome = Outcome { backend_session: bs, ..Default::default() };
                let transcript = outcome
                    .backend_session
                    .as_ref()
                    .and_then(|s| backend.transcript_path(&cwd, s))
                    .map(|p| p.display().to_string());
                finalize(c, id, &outcome, ok, &snapshot, &api, &who, true, transcript).await;
                report_link(id);
            }
        }
    }
    Ok(())
}

fn preset_session_of(spec: &Spec) -> Option<String> {
    spec.preset_session.clone()
}

// ---- cloud session resolution ----

/// Resolve the cloud session id for this run. Fresh runs register a new session.
/// A resume reuses the SAME id when the prior session is still live (running/
/// paused) — cloud forbids reopening a terminal one — otherwise it forks a new
/// session that records the id it was `resumedFrom` (lineage).
///
/// Lineage is only ever written for a session we VERIFIED: `GET` succeeded, so
/// the id exists and is ours. Every failure — 403 (another org), 404 (gone), a
/// 5xx, a timeout, DNS — leaves us unable to say either, so we fail closed and
/// register with NO `resumedFrom` rather than record a pointer that may dangle
/// or reference another tenant. The caller's `cloud_resume_block` already
/// withholds ids it knows are foreign; this is the same rule enforced HERE, so
/// the guarantee holds for any caller rather than only for today's single one.
pub(crate) async fn resolve_cloud_session(
    client: &SessionClient,
    agent: &str,
    title: &str,
    resume_from: Option<&str>,
) -> Result<(String, Option<String>)> {
    if let Some(old) = resume_from {
        match client.get(old).await {
            // Live: same-id re-attach, move it back to running.
            Ok(info) if !info.is_terminal() => {
                let _ = client.set_status(old, Status::Running).await;
                return Ok((old.to_string(), None));
            }
            // Terminal and VERIFIED ours: cloud forbids reopening it, so fork a
            // new session and record the lineage we just confirmed.
            Ok(_) => {
                let reg = client.register(agent, title).await?;
                return Ok((reg.id, Some(old.to_string())));
            }
            // Unverified. Do not assert a lineage we could not confirm.
            Err(e) => {
                warn(&format!(
                    "could not verify session {old} ({e}); starting a fresh cloud session with no \
                     resume lineage."
                ));
                let reg = client.register(agent, title).await?;
                return Ok((reg.id, None));
            }
        }
    }
    let reg = client.register(agent, title).await?;
    Ok((reg.id, None))
}

// ---- streaming ----

#[derive(Debug, Default, Clone)]
pub(crate) struct Outcome {
    pub backend_session: Option<String>,
    pub usage: Usage,
    pub saw_error: bool,
    pub final_summary: Option<String>,
}

/// The forward+render sink for a structured event stream. Forwarding fires ONLY
/// when a cloud client AND session id are present — the privacy gate is
/// structural: an unlinked run cannot reach the network from here.
struct Sink {
    client: Option<SessionClient>,
    session_id: Option<String>,
    render: bool,
    out: Outcome,
}

impl Sink {
    async fn handle(&mut self, m: Mapped) {
        match m {
            Mapped::Event { kind, payload } => {
                if self.render {
                    render_event(kind, &payload);
                }
                if let (Some(c), Some(id)) = (&self.client, &self.session_id) {
                    if let Err(e) = c.event(id, kind, payload).await {
                        warn(&format!("stream event dropped: {e}"));
                    }
                }
            }
            Mapped::BackendSession(id) => {
                self.out.backend_session.get_or_insert(id);
            }
            Mapped::Usage(u) => self.out.usage.merge(u),
            Mapped::Terminal { ok, summary } => {
                if !ok {
                    self.out.saw_error = true;
                }
                if summary.is_some() {
                    self.out.final_summary = summary;
                }
            }
        }
    }
}

/// The largest single pre-parse line we will buffer. Cloud caps an event payload
/// at 48 KiB (`event::PAYLOAD_BUDGET`), so any legitimate stream/transcript line
/// is far smaller; a line beyond this is garbage or hostile and is dropped rather
/// than accumulated, so a backend (or MCP output it relays) can never OOM the
/// wrapper with one unbounded, newline-free line.
const MAX_LINE: usize = 1024 * 1024;

/// The most transcript we ingest per poll while tailing, so a single large
/// append is spread across polls instead of being read into memory whole.
const MAX_TAIL_CHUNK: u64 = 8 * 1024 * 1024;

/// Read the next newline-delimited line from `reader`, bounded to `cap` bytes.
/// A line longer than `cap` is discarded — its bytes are still consumed through
/// the terminating newline so the stream stays aligned — and reading resumes at
/// the next line. `Ok(None)` signals EOF. Memory is bounded by `cap` (plus one
/// buffered chunk) regardless of adversarial input.
async fn next_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut overflow = false; // this line already passed `cap`; skip to newline
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            // EOF: yield a final unterminated line only if it fit within `cap`.
            return Ok((!buf.is_empty()).then(|| String::from_utf8_lossy(&buf).into_owned()));
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => {
                if !overflow {
                    buf.extend_from_slice(&chunk[..i]);
                }
                reader.consume(i + 1);
                if overflow {
                    overflow = false;
                    buf.clear(); // dropped the over-long line; start the next one
                    continue;
                }
                return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
            }
            None => {
                let n = chunk.len();
                if !overflow {
                    buf.extend_from_slice(chunk);
                    if buf.len() > cap {
                        overflow = true;
                        buf.clear(); // release memory; keep skipping to '\n'
                    }
                }
                reader.consume(n);
            }
        }
    }
}

/// Drive a backend's structured line stream through parse → forward/render.
pub(crate) async fn run_stream<R: AsyncBufRead + Unpin>(
    backend: &dyn Backend,
    mut reader: R,
    client: Option<SessionClient>,
    session_id: Option<String>,
    render: bool,
) -> Result<Outcome> {
    let mut sink = Sink { client, session_id, render, out: Outcome::default() };
    while let Some(line) = next_bounded_line(&mut reader, MAX_LINE).await.context("reading backend stream")? {
        for m in backend.parse(&line) {
            sink.handle(m).await;
        }
    }
    Ok(sink.out)
}

// ---- finalize ----

/// Close out a linked session: record usage, persist + mirror the resume handle,
/// and set the terminal/suspended status. Interactive runs suspend to `paused`
/// (resumable, same id); headless task runs complete to `done`; a failure is
/// `error`. All cloud writes are best-effort (a hiccup never crashes the CLI).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finalize(
    client: &SessionClient,
    session_id: &str,
    outcome: &Outcome,
    ok: bool,
    snapshot: &Snapshot,
    api: &str,
    // `identity` is the owner of this cloud session, for the LOCAL resume record
    // ONLY. It is deliberately not part of `Snapshot`: the snapshot is emitted to
    // cloud, and the CLI never sends an org — cloud derives it from the JWT.
    identity: &str,
    interactive: bool,
    transcript_path: Option<String>,
) {
    if !outcome.usage.is_empty() {
        let mut p = serde_json::to_value(&outcome.usage).unwrap_or_else(|_| json!({}));
        p["type"] = json!("usage");
        let _ = client.event(session_id, Kind::Log, p).await;
    }

    if let Some(bs) = &outcome.backend_session {
        let rec = ResumeRecord {
            cloud_session_id: session_id.to_string(),
            identity: identity.to_string(),
            backend: snapshot.backend.clone(),
            backend_session_id: bs.clone(),
            cwd: snapshot.cwd.clone(),
            api: api.to_string(),
            machine_id: snapshot.machine_id.clone(),
            repo: snapshot.repo.clone(),
            transcript_path,
            created_at: now(),
        };
        let _ = rec.save();
        let _ = client.event(session_id, Kind::Status, rec.resume_payload()).await;
    }

    let status = if !ok {
        Status::Error
    } else if interactive {
        Status::Paused
    } else {
        Status::Done
    };
    let _ = client.set_status(session_id, status).await;
}

// ---- process execution ----

async fn run_headless(
    backend: &dyn Backend,
    structured: bool,
    launch: Launch,
    client: Option<SessionClient>,
    session_id: Option<String>,
) -> Result<(Outcome, bool)> {
    let Launch { mut command, cleanup } = launch;
    command.stdin(Stdio::inherit()).stderr(Stdio::inherit());
    if structured {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::inherit());
    }
    let mut child = command.spawn().map_err(spawn_err)?;

    let outcome = if structured {
        let stdout = child.stdout.take().expect("piped stdout");
        let reader = tokio::io::BufReader::new(stdout);
        run_stream(backend, reader, client, session_id, true).await?
    } else {
        Outcome::default()
    };
    let status = child.wait().await.context("waiting for backend")?;
    drop(cleanup);
    Ok((outcome, status.success()))
}

async fn run_interactive(
    backend: &dyn Backend,
    launch: Launch,
    client: Option<SessionClient>,
    session_id: Option<String>,
    cwd: &Path,
    watch_sid: Option<String>,
) -> Result<bool> {
    let Launch { mut command, cleanup } = launch;
    command.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let mut child = command.spawn().map_err(spawn_err)?;

    // Linked interactive per-event streaming rides the backend transcript tail.
    let stop = Arc::new(AtomicBool::new(false));
    let tail = match (&client, &session_id, &watch_sid) {
        (Some(c), Some(id), Some(sid)) => backend.transcript_path(cwd, sid).map(|path| {
            tokio::spawn(tail_transcript(path, c.clone(), id.clone(), stop.clone()))
        }),
        _ => None,
    };

    let status = child.wait().await.context("waiting for backend")?;
    stop.store(true, Ordering::Relaxed);
    if let Some(h) = tail {
        let _ = h.await;
    }
    drop(cleanup);
    Ok(status.success())
}

/// Follow a Claude transcript JSONL, forwarding newly-appended events to the
/// linked session. Best-effort: parse/forward failures are ignored so the live
/// TUI is never disturbed.
async fn tail_transcript(path: PathBuf, client: SessionClient, session_id: String, stop: Arc<AtomicBool>) {
    let mut pos: u64 = 0;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if let Ok(mut f) = tokio::fs::File::open(&path).await {
            let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
            if len > pos && f.seek(std::io::SeekFrom::Start(pos)).await.is_ok() {
                // Read a bounded slice per poll so a huge single append can't be
                // slurped whole; advance `pos` by what we actually read.
                let want = (len - pos).min(MAX_TAIL_CHUNK);
                let mut chunk: Vec<u8> = Vec::new();
                if (&mut f).take(want).read_to_end(&mut chunk).await.is_ok() {
                    pos += chunk.len() as u64;
                    buf.extend_from_slice(&chunk);
                    while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=nl).collect();
                        let text = String::from_utf8_lossy(&line[..line.len() - 1]);
                        for m in claude::Claude.parse(text.trim_end()) {
                            if let Mapped::Event { kind, payload } = m {
                                let _ = client.event(&session_id, kind, payload).await;
                            }
                        }
                    }
                    // Drop an over-long, newline-free line so a hostile or corrupt
                    // transcript can't grow the buffer without bound.
                    if buf.len() > MAX_LINE {
                        buf.clear();
                    }
                }
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

// ---- presentation ----

#[allow(clippy::too_many_arguments)]
fn banner(
    opts: &Options,
    backend: &str,
    cwd: &Path,
    api: &str,
    routing: Option<&Routing>,
    signed_in: bool,
    session: Option<&str>,
    theme: Option<&str>,
) {
    let _ = (theme, api); // theme applied silently; the route line carries its own host
    println!(
        "{} {} · {} · {}",
        "hanzo code".bold(),
        backend.cyan(),
        cwd.display().to_string().dimmed(),
        opts.resume.as_deref().map(|_| "resume").unwrap_or("start").dimmed(),
    );
    let (route_line, stream_line) = status_lines(opts, routing, signed_in, session);
    let route_line = if routing.is_some() { route_line.green() } else { route_line.dimmed() };
    let stream_line = if session.is_some() { stream_line.green() } else { stream_line.dimmed() };
    println!("  {route_line}");
    println!("  {stream_line}");
}

/// The two status lines — model-routing and session-stream — as PLAIN text.
/// Kept separate from `banner` (which colors + prints) so the wording is
/// unit-testable and stays honest.
///
/// The two are INDEPENDENT: routing decides where prompts + code + tool output
/// go for inference (the gateway, or a provider's own API), streaming decides
/// whether the session is mirrored to mission-control. "off" on one says nothing
/// about the other — so an unlinked run must never imply "local only" while
/// routing still ships code somewhere.
fn status_lines(
    opts: &Options,
    routing: Option<&Routing>,
    signed_in: bool,
    session: Option<&str>,
) -> (String, String) {
    let strip = |u: &str| u.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string();
    let route = match routing {
        Some(Routing::Gateway { api, .. }) => {
            format!("model routing: on → {} (prompts + code go here; usage metered to your org)", strip(api))
        }
        Some(Routing::Anthropic { .. }) => {
            "model routing: on → api.anthropic.com (your Anthropic key; usage billed by Anthropic)".to_string()
        }
        Some(Routing::OpenAI { .. }) => {
            "model routing: on → api.openai.com (your OpenAI key; usage billed by OpenAI)".to_string()
        }
        None if !opts.route => {
            "model routing: off (--no-route; the backend's own model account, code stays with your provider)".to_string()
        }
        None if !signed_in => {
            "model routing: off (sign in with `hanzo login` to route + meter model calls)".to_string()
        }
        None => "model routing: off".to_string(),
    };
    let stream = match session {
        Some(id) => format!("session stream: on → https://hanzo.bot/sessions/{id}"),
        None => "session stream: off (this session is not mirrored to cloud; pass --link to stream it)".to_string(),
    };
    (route, stream)
}

fn report_link(id: &str) {
    // One clean line — bare id (no `sess_`), `hanzo` (bare = a coding session).
    let short = id.strip_prefix("sess_").unwrap_or(id);
    println!("{}", format!("resume: hanzo --resume {short}").magenta());
}

fn render_event(kind: Kind, payload: &Value) {
    match kind {
        Kind::Message => {
            if let Some(t) = payload.get("text").and_then(Value::as_str) {
                if payload.get("role").and_then(Value::as_str) == Some("assistant") {
                    println!("{t}");
                }
            }
        }
        Kind::ToolCall => {
            let name = payload.get("name").and_then(Value::as_str).unwrap_or("tool");
            let brief = payload
                .get("input")
                .map(one_line)
                .unwrap_or_default();
            println!("{}", format!("→ {name} {brief}").dimmed());
        }
        Kind::Spawn => {
            let a = payload.get("agent").and_then(Value::as_str).unwrap_or("agent");
            println!("{}", format!("⇒ spawn {a}").dimmed());
        }
        Kind::Log => {
            match payload.get("type").and_then(Value::as_str) {
                Some("tool-result") => {
                    let n = payload.get("output").and_then(Value::as_str).map(|s| s.len()).unwrap_or(0);
                    println!("{}", format!("← result ({n} bytes)").dimmed());
                }
                Some("reasoning") => {
                    if let Some(t) = payload.get("text").and_then(Value::as_str) {
                        println!("{}", format!("· {}", first_line(t)).dimmed());
                    }
                }
                _ => {
                    if let Some(t) = payload.get("text").and_then(Value::as_str) {
                        println!("{}", t.dimmed());
                    }
                }
            }
        }
        Kind::Status | Kind::Control => {}
    }
}

fn one_line(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    first_line(&s)
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.len() > 100 {
        format!("{}…", &line[..100])
    } else {
        line.to_string()
    }
}

fn session_title(opts: &Options) -> String {
    match &opts.task {
        Some(t) => t.chars().take(120).collect(),
        None => "interactive coding session".to_string(),
    }
}

fn warn(msg: &str) {
    eprintln!("{} {}", "warning:".yellow().bold(), msg);
}

fn spawn_err(e: std::io::Error) -> anyhow::Error {
    anyhow!("failed to launch the coding backend ({e}) — is it installed and on PATH?")
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code::claude::Claude;
    use crate::commands::code::dev::Dev;
    use crate::commands::code::testmock::MockCloud;
    use tokio::io::AsyncWriteExt;

    async fn reader_of(fixture: &str) -> impl AsyncBufRead + Unpin {
        let (r, mut w) = tokio::io::duplex(1 << 20);
        w.write_all(fixture.as_bytes()).await.unwrap();
        drop(w); // EOF
        tokio::io::BufReader::new(r)
    }

    fn id(s: &str) -> Identity {
        // Derived from claims, as everywhere else — there is no other way to
        // build one, which is the point.
        let (owner, name) = s.split_once('/').unwrap();
        Identity::from_access_token(&crate::iam::identity::testjwt::jwt(owner, name)).unwrap()
    }

    /// Same identity: the cloud session is ours, re-attach silently.
    #[test]
    fn resume_as_the_same_identity_reattaches_without_a_note() {
        assert!(cloud_resume_block("hanzo/z", Some(&id("hanzo/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").is_none());
        assert!(cloud_resume_block("admin/z", Some(&id("admin/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").is_none());
    }

    /// Different org: the cloud session CANNOT move (the gateway refuses it, and
    /// that refusal is tenant isolation working). The local conversation carries,
    /// a new cloud session is registered, and we SAY so — never silently.
    #[test]
    fn resume_across_an_org_boundary_registers_fresh_and_says_so() {
        let note = cloud_resume_block("hanzo/z", Some(&id("admin/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").expect("must warn");
        assert!(note.contains("hanzo/z") && note.contains("admin/z"));
        assert!(note.contains("NEW cloud session"), "{note}");
        // Billing is stated plainly — it moves to the active identity's org.
        assert!(note.contains("billed to admin"), "{note}");
        // And the way back is offered rather than done for them.
        assert!(note.contains("hanzo switch hanzo/z"), "{note}");
    }

    /// Same human, same username, DIFFERENT org — the exact `admin/z` vs
    /// `hanzo/z` case. `owner` alone decides; a name match must not re-attach.
    #[test]
    fn the_same_username_in_another_org_is_still_cross_org() {
        assert!(cloud_resume_block("hanzo/z", Some(&id("admin/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").is_some());
        assert!(cloud_resume_block("admin/z", Some(&id("hanzo/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").is_some());
    }

    /// A record predating identity tracking has unknown provenance. It cannot be
    /// PROVEN ours, so it is treated exactly like a cross-org resume: fail closed
    /// on the cloud id, keep the local conversation, and explain.
    #[test]
    fn a_record_of_unknown_provenance_does_not_reattach() {
        let note = cloud_resume_block("", Some(&id("admin/z")), "https://api.hanzo.ai", "https://api.hanzo.ai").expect("must warn");
        assert!(note.contains("predates identity tracking"), "{note}");
        assert!(note.contains("NEW cloud session"), "{note}");
    }

    /// Unlinked run: no bearer, so nothing reaches cloud and there is no session
    /// to own. No note — there is nothing to tell the user about.
    #[test]
    fn an_unauthenticated_resume_has_no_cloud_session_to_reason_about() {
        assert!(cloud_resume_block("hanzo/z", None, "https://api.hanzo.ai", "https://api.hanzo.ai").is_none());
        assert!(cloud_resume_block("", None, "https://api.hanzo.ai", "https://api.hanzo.ai").is_none());
    }

    #[test]
    fn link_gate_no_link_wins_then_link_then_persisted() {
        // `--no-link` always wins — over `--link` AND over a persisted `true`
        // (the new default), so the opt-out is absolute.
        assert!(!effective_link(true, true, true)); // --no-link beats --link
        assert!(!effective_link(false, true, true)); // --no-link beats persisted true
        // `--link` forces on when there is no `--no-link`.
        assert!(effective_link(true, false, false));
        // No flags: the persisted default decides — ON by default now, and a
        // persisted `link = false` is the opt-out.
        assert!(effective_link(false, false, true)); // persisted default (on)
        assert!(!effective_link(false, false, false)); // persisted opt-out
    }

    /// The run-target register uses the SAME structural auth gate as the session
    /// link: an unauthenticated run (no bearer) never builds a cloud request, and
    /// `--no-link` suppresses it even when signed in.
    #[test]
    fn unauthenticated_run_registers_no_target() {
        assert!(!links_target(true, false)); // signed out, link intended -> no target
        assert!(!links_target(false, true)); // signed in, --no-link -> no target
        assert!(links_target(true, true)); // signed in + link -> register
    }

    /// The privacy property that link-by-default must NOT weaken: with no cloud
    /// client (the state an UNAUTHENTICATED run lands in — `run` sets the client
    /// to `None` when there is no bearer), the stream reaches cloud with nothing,
    /// even though a cloud endpoint is live. The gate is structural, not a flag.
    #[tokio::test]
    async fn no_auth_means_no_stream_even_with_cloud_live() {
        let mock = MockCloud::start().await;
        let fixture = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sid","model":"m"}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"private code"}]}}"#, "\n"
        );
        let reader = reader_of(fixture).await;
        // client == None models the unauthenticated run (no bearer -> no client).
        let out = run_stream(&Claude, reader, None, None, false).await.unwrap();
        assert_eq!(out.backend_session.as_deref(), Some("sid")); // parsed locally
        assert!(mock.requests().is_empty(), "no auth -> nothing reaches cloud");
    }

    #[tokio::test]
    async fn linked_stream_forwards_mapped_events_with_correct_kinds() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let fixture = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sid-1","model":"m"}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t","name":"Bash","input":{"command":"ls"}}]}}"#, "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":5,"output_tokens":2},"total_cost_usd":0.01,"result":"ok"}"#, "\n"
        );
        let reader = reader_of(fixture).await;
        let out = run_stream(&Claude, reader, Some(client), Some("sess_1".into()), false).await.unwrap();

        assert_eq!(out.backend_session.as_deref(), Some("sid-1"));
        assert_eq!(out.usage.input_tokens, Some(5));
        assert!(!out.saw_error);

        let kinds: Vec<String> = mock
            .requests()
            .iter()
            .filter(|r| r.path == "/v1/agents/sessions/sess_1/events")
            .map(|r| r.json()["kind"].as_str().unwrap_or("").to_string())
            .collect();
        // session-start log, assistant message, tool-call.
        assert!(kinds.contains(&"log".to_string()));
        assert!(kinds.contains(&"message".to_string()));
        assert!(kinds.contains(&"tool-call".to_string()));
    }

    #[tokio::test]
    async fn unlinked_stream_forwards_nothing_even_with_cloud_available() {
        // A cloud IS listening, but with no client the stream cannot reach it.
        let mock = MockCloud::start().await;
        let fixture = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sid","model":"m"}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"secret code here"}]}}"#, "\n"
        );
        let reader = reader_of(fixture).await;
        let out = run_stream(&Claude, reader, None, None, false).await.unwrap();
        assert_eq!(out.backend_session.as_deref(), Some("sid")); // parsed locally
        assert!(mock.requests().is_empty(), "unlinked run must not send anything to cloud");
    }

    #[tokio::test]
    async fn dev_stream_maps_and_forwards() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let fixture = concat!(
            r#"{"type":"thread.started","thread_id":"th-9"}"#, "\n",
            r#"{"type":"item.completed","item":{"id":"i","type":"command_execution","command":"go build","aggregated_output":"ok","exit_code":0,"status":"completed"}}"#, "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":1,"cached_input_tokens":0}}"#, "\n"
        );
        let reader = reader_of(fixture).await;
        let out = run_stream(&Dev, reader, Some(client), Some("sess_2".into()), false).await.unwrap();
        assert_eq!(out.backend_session.as_deref(), Some("th-9"));
        assert_eq!(out.usage.output_tokens, Some(1));
        let has_toolcall = mock
            .requests()
            .iter()
            .any(|r| r.path.ends_with("/events") && r.json()["kind"] == "tool-call");
        assert!(has_toolcall);
    }

    #[tokio::test]
    async fn resolve_fresh_registers_a_new_session() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let (id, forked) = resolve_cloud_session(&client, "claude", "t", None).await.unwrap();
        assert_eq!(id, "sess_mock");
        assert!(forked.is_none());
        assert!(mock.requests().iter().any(|r| r.method == "POST" && r.path == "/v1/agents/sessions"));
    }

    #[tokio::test]
    async fn resume_nonterminal_reuses_same_id_without_registering() {
        let mock = MockCloud::start_get_status("paused").await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let (id, forked) = resolve_cloud_session(&client, "claude", "t", Some("sess_old")).await.unwrap();
        assert_eq!(id, "sess_old", "must re-attach the SAME id");
        assert!(forked.is_none());
        let reqs = mock.requests();
        assert!(reqs.iter().any(|r| r.method == "GET" && r.path == "/v1/agents/sessions/sess_old"));
        assert!(reqs.iter().any(|r| r.method == "PATCH" && r.json()["status"] == "running"));
        assert!(!reqs.iter().any(|r| r.method == "POST" && r.path == "/v1/agents/sessions"));
    }

    #[tokio::test]
    async fn resume_terminal_forks_a_new_session_with_lineage() {
        let mock = MockCloud::start_get_status("done").await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let (id, forked) = resolve_cloud_session(&client, "claude", "t", Some("sess_old")).await.unwrap();
        assert_eq!(id, "sess_mock");
        assert_eq!(forked.as_deref(), Some("sess_old"));
        assert!(mock.requests().iter().any(|r| r.method == "POST" && r.path == "/v1/agents/sessions"));
    }

    /// MED-2: lineage is only written for a session we VERIFIED.
    ///
    /// Every `GET` failure — 403 (another org), 404 (gone), 5xx/timeout/DNS —
    /// leaves us unable to say the id is ours or even real, so we must register
    /// with NO `resumedFrom` rather than record a pointer that dangles or names
    /// another tenant. Enforced in the FUNCTION, not just at today's call site.
    #[tokio::test]
    async fn an_unverifiable_session_forks_with_no_lineage() {
        for code in [403u16, 404, 500] {
            let mock = MockCloud::start_session_get_failing(code).await;
            let client = SessionClient::new(&mock.base_url(), "T").unwrap();

            let (id, forked) = resolve_cloud_session(&client, "claude", "t", Some("sess_other_org"))
                .await
                .unwrap();

            assert_eq!(id, "sess_mock", "a fresh session is registered ({code})");
            assert_eq!(
                forked, None,
                "must NOT record resumedFrom for an unverified session ({code})"
            );
            // And the id we could not verify never reached cloud as lineage.
            let posted = mock
                .requests()
                .iter()
                .filter(|r| r.method == "POST" && r.path == "/v1/agents/sessions")
                .map(|r| r.json().to_string())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                !posted.contains("sess_other_org"),
                "leaked an unverified id into the register body ({code}): {posted}"
            );
        }
    }

    /// A cloud id minted by one control plane means nothing to another, so
    /// `hanzo network use local` + resume of a prod session must not re-attach.
    #[test]
    fn resume_on_a_different_network_does_not_reattach() {
        let note = cloud_resume_block(
            "hanzo/z",
            Some(&id("hanzo/z")),
            "https://api.hanzo.ai",
            "http://localhost:3690",
        )
        .expect("must warn even though the identity matches");
        assert!(note.contains("api.hanzo.ai") && note.contains("localhost:3690"), "{note}");
        assert!(note.contains("NEW cloud session"), "{note}");

        // A trailing slash is not a different network.
        assert!(cloud_resume_block(
            "hanzo/z",
            Some(&id("hanzo/z")),
            "https://api.hanzo.ai/",
            "https://api.hanzo.ai"
        )
        .is_none());

        // A record predating the api field cannot contradict the active network.
        assert!(cloud_resume_block("hanzo/z", Some(&id("hanzo/z")), "", "https://api.hanzo.ai").is_none());
    }

    #[tokio::test]
    async fn finalize_reports_usage_resume_handle_and_terminal_status() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let snapshot = Snapshot {
            machine_id: "m".into(),
            host: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            cwd: "/w".into(),
            backend: "claude".into(),
            backend_version: None,
            repo: Default::default(),
        };
        let outcome = Outcome {
            backend_session: Some("sid-x".into()),
            usage: Usage { input_tokens: Some(9), ..Default::default() },
            saw_error: false,
            final_summary: None,
        };
        finalize(&client, "sess_9", &outcome, true, &snapshot, "https://api.hanzo.ai", "hanzo/z", false, None).await;

        let reqs = mock.requests();
        // usage log event
        assert!(reqs.iter().any(|r| r.path.ends_with("/events") && r.json()["kind"] == "log" && r.json()["payload"]["type"] == "usage"));
        // resume-handle status event
        assert!(reqs.iter().any(|r| r.path.ends_with("/events") && r.json()["kind"] == "status" && r.json()["payload"]["type"] == "resume" && r.json()["payload"]["backendSessionId"] == "sid-x"));
        // terminal status
        assert!(reqs.iter().any(|r| r.method == "PATCH" && r.json()["status"] == "done"));
        // clean up the resume record this finalize persisted
        let _ = std::fs::remove_file(
            dirs::data_local_dir().unwrap().join("hanzo/code/sessions/sess_9.json"),
        );
    }

    #[tokio::test]
    async fn finalize_interactive_suspends_to_paused_and_failure_is_error() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let snapshot = Snapshot {
            machine_id: "m".into(), host: "h".into(), os: "linux".into(), arch: "x".into(),
            cwd: "/w".into(), backend: "dev".into(), backend_version: None, repo: Default::default(),
        };
        let outcome = Outcome::default();
        finalize(&client, "sess_i", &outcome, true, &snapshot, "https://api.hanzo.ai", "hanzo/z", true, None).await;
        assert!(mock.requests().iter().any(|r| r.method == "PATCH" && r.json()["status"] == "paused"));

        let mock2 = MockCloud::start().await;
        let client2 = SessionClient::new(&mock2.base_url(), "T").unwrap();
        finalize(&client2, "sess_e", &outcome, false, &snapshot, "https://api.hanzo.ai", "hanzo/z", true, None).await;
        assert!(mock2.requests().iter().any(|r| r.method == "PATCH" && r.json()["status"] == "error"));
    }

    #[test]
    fn uuid_is_v4_shaped() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4'); // version nibble
        assert!(matches!(u.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }

    /// A backend whose `build` spawns a REAL child (`cat` of a fixture file) so
    /// the full spawn → pipe stdout → parse → forward path is exercised without a
    /// live `claude`/`dev` binary.
    struct FakeBackend {
        fixture: String,
    }

    impl Backend for FakeBackend {
        fn label(&self) -> &'static str {
            "claude"
        }
        fn version(&self) -> Option<String> {
            None
        }
        fn build(&self, _spec: &Spec) -> Result<Launch> {
            use std::io::Write;
            let mut f = tempfile::Builder::new().suffix(".jsonl").tempfile().unwrap();
            f.write_all(self.fixture.as_bytes()).unwrap();
            let path = f.into_temp_path();
            let mut command = tokio::process::Command::new("cat");
            command.arg(&*path);
            Ok(Launch { command, cleanup: vec![path] })
        }
        fn parse(&self, line: &str) -> Vec<Mapped> {
            Claude.parse(line)
        }
        fn transcript_path(&self, _: &Path, _: &str) -> Option<PathBuf> {
            None
        }
    }

    fn dummy_spec() -> Spec {
        Spec {
            mode: Mode::Headless,
            task: Some("t".into()),
            cwd: PathBuf::from("."),
            routing: Route::Inherit,
            mcp: None,
            structured: true,
            preset_session: None,
            trust_project: false,
            resume: None,
            passthrough: vec![],
        }
    }

    #[tokio::test]
    async fn run_headless_spawns_a_real_child_and_forwards_end_to_end() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let fixture = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sid-1","model":"m"}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t","name":"Bash","input":{"command":"ls"}}]}}"#, "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":5,"output_tokens":2},"result":"ok"}"#, "\n"
        );
        let fake = FakeBackend { fixture: fixture.into() };
        let launch = fake.build(&dummy_spec()).unwrap();
        let (out, ok) =
            run_headless(&fake, true, launch, Some(client), Some("sess_e2e".into())).await.unwrap();

        assert!(ok, "child exited zero");
        assert_eq!(out.backend_session.as_deref(), Some("sid-1"));
        assert_eq!(out.usage.input_tokens, Some(5));
        let kinds: Vec<String> = mock
            .requests()
            .iter()
            .filter(|r| r.path == "/v1/agents/sessions/sess_e2e/events")
            .map(|r| r.json()["kind"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(kinds.contains(&"message".to_string()));
        assert!(kinds.contains(&"tool-call".to_string()));
    }

    #[test]
    fn banner_separates_model_routing_from_session_stream() {
        let opts = Options {
            backend: "claude".into(),
            link: false,
            no_link: true,
            route: true,
            mcp: true,
            project_mcp: false,
            resume: None,
            brand: "hanzo".into(),
            task: None,
            theme: None,
            passthrough: vec![],
        };
        // Routing ON (gateway), stream OFF — the exact case LOW-1 flagged:
        // --no-link but model calls still ship code to the gateway.
        let gw = Routing::Gateway { api: "https://api.hanzo.ai".into(), token: "T".into() };
        let (route, stream) = status_lines(&opts, Some(&gw), true, None);
        assert!(route.contains("model routing: on"), "got: {route}");
        assert!(route.contains("api.hanzo.ai"));
        assert!(route.contains("prompts + code"));
        assert!(stream.contains("session stream: off"), "got: {stream}");
        // Must NOT claim the run is "local only" while routing is on.
        assert!(!stream.to_lowercase().contains("local only"));
        assert!(!route.to_lowercase().contains("local only"));

        // --no-route is explicit and distinct from "off because not signed in".
        let mut o2 = opts;
        o2.route = false;
        let (route2, _) = status_lines(&o2, None, true, None);
        assert!(route2.contains("model routing: off"));
        assert!(route2.contains("--no-route"));

        // Stream ON names the session id and mission-control, not "link".
        let (_, stream_on) = status_lines(&o2, None, true, Some("sess_x"));
        assert!(stream_on.contains("session stream: on"));
        assert!(stream_on.contains("sess_x"));
        // Pin the canonical viewer route: the playground session page is
        // `/sessions/:id` (plural, mirroring cloud's `/v1/agents/sessions/:id`
        // resource and the app's `/collection/:id` house style). A singular
        // `/session/` 404s — the route the app actually serves is `/sessions/`.
        assert!(stream_on.contains("https://hanzo.bot/sessions/sess_x"), "got: {stream_on}");
    }

    /// A direct provider route names the VENDOR endpoint + who bills — never the
    /// gateway, so the user is never misled about where their code + money go.
    #[test]
    fn status_line_names_the_direct_provider_endpoint() {
        let opts = Options {
            backend: "claude".into(),
            link: false,
            no_link: true,
            route: true,
            mcp: true,
            project_mcp: false,
            resume: None,
            brand: "hanzo".into(),
            task: None,
            theme: None,
            passthrough: vec![],
        };
        let anthropic = Routing::Anthropic { key: "sk-ant-x".into() };
        let (route, _) = status_lines(&opts, Some(&anthropic), true, None);
        assert!(route.contains("model routing: on"), "got: {route}");
        assert!(route.contains("api.anthropic.com"), "got: {route}");
        assert!(route.contains("billed by Anthropic"), "got: {route}");
        assert!(!route.contains("api.hanzo.ai"), "a direct route must NOT claim the gateway");
        // The key never appears in the human-facing line.
        assert!(!route.contains("sk-ant-x"));

        let openai = Routing::OpenAI { key: "sk-x".into() };
        let (route, _) = status_lines(&opts, Some(&openai), true, None);
        assert!(route.contains("api.openai.com") && route.contains("billed by OpenAI"), "got: {route}");
    }

    /// The routing precedence: a direct provider is preferred ONLY when it can
    /// drive the backend, and the gateway (bearer, then hk-) is always the tail.
    #[test]
    fn route_plan_prefers_a_matching_direct_provider_else_the_gateway() {
        use BackendKind::{Claude, Dev};
        // Anthropic + Claude → try the Anthropic key first, then gateway.
        assert_eq!(route_plan(Claude, Some("anthropic"), true), vec![Cred::AnthropicKey, Cred::Bearer, Cred::HanzoKey]);
        // OpenAI + dev → the OpenAI key first.
        assert_eq!(route_plan(Dev, Some("openai"), false), vec![Cred::OpenAIKey, Cred::HanzoKey]);
        // Mismatched pairing (OpenAI selected, Claude backend) → NO direct key,
        // fall straight to the gateway.
        assert_eq!(route_plan(Claude, Some("openai"), true), vec![Cred::Bearer, Cred::HanzoKey]);
        assert_eq!(route_plan(Dev, Some("anthropic"), false), vec![Cred::HanzoKey]);
        // No provider selected → the gateway, bearer preferred over a stored key.
        assert_eq!(route_plan(Claude, None, true), vec![Cred::Bearer, Cred::HanzoKey]);
        assert_eq!(route_plan(Claude, None, false), vec![Cred::HanzoKey]);
        // Explicit "hanzo" behaves like the gateway default.
        assert_eq!(route_plan(Claude, Some("hanzo"), true), vec![Cred::Bearer, Cred::HanzoKey]);
    }

    /// LOW-1: when the credential plan resolves NOTHING, a SELECTED provider fails
    /// closed (the backend then clears its model-auth env, and `run` warns), while
    /// an unconfigured/signed-out run inherits the backend's own account —
    /// unchanged, so "continuing locally" still works with a user's own key.
    #[test]
    fn unresolved_route_fails_closed_only_when_a_provider_is_selected() {
        assert!(matches!(unresolved_route(true), Route::FailClosed));
        assert!(matches!(unresolved_route(false), Route::Inherit));
        // Both carry no credential — the banner reads them the same ("off").
        assert!(unresolved_route(true).via().is_none());
        assert!(unresolved_route(false).via().is_none());
    }

    /// A missing / deleted cwd yields a CLEAR message, not the cryptic
    /// `resolving current dir` chain — a fresh or odd environment never dies
    /// mysteriously.
    #[test]
    fn cwd_or_friendly_explains_a_missing_directory() {
        let ok = cwd_or_friendly(Ok(PathBuf::from("/some/dir"))).unwrap();
        assert_eq!(ok, PathBuf::from("/some/dir"));

        let err = cwd_or_friendly(Err(std::io::Error::from(std::io::ErrorKind::NotFound)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("current directory is unavailable"), "got: {err}");
        assert!(err.to_lowercase().contains("cd into a directory") || err.contains("`cd`"), "got: {err}");
        // The old cryptic phrasing must be gone.
        assert!(!err.contains("resolving current dir"), "got: {err}");
    }

    /// MEDIUM-1: one giant newline-free line must NOT be buffered whole (OOM);
    /// it is dropped and the stream recovers, still forwarding the next line.
    #[tokio::test]
    async fn oversize_line_is_dropped_and_stream_recovers() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();

        // 3 MiB with no newline (over MAX_LINE = 1 MiB), then a valid event.
        let mut fixture = "x".repeat(3 * 1024 * 1024);
        fixture.push('\n');
        fixture
            .push_str(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"after"}]}}"#);
        fixture.push('\n');

        // Feed from a spawned writer over a small duplex so the reader drains as
        // it goes (a blocking write_all of 3 MiB into a small buffer would hang).
        let (r, mut w) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = w.write_all(fixture.as_bytes()).await;
        });
        let reader = tokio::io::BufReader::new(r);

        let out = run_stream(&Claude, reader, Some(client), Some("sess_big".into()), false)
            .await
            .unwrap();
        assert!(!out.saw_error);
        // The valid line AFTER the oversize one was still parsed and forwarded.
        assert!(
            mock.requests().iter().any(|r| r.path == "/v1/agents/sessions/sess_big/events"
                && r.json()["kind"] == "message"
                && r.json()["payload"]["text"] == "after"),
            "stream must recover and forward the line after the oversize one"
        );
    }

    #[tokio::test]
    async fn tail_forwards_appended_transcript_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "").unwrap();
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(tail_transcript(path.clone(), client, "sess_t".into(), stop.clone()));

        tokio::time::sleep(Duration::from_millis(120)).await;
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(
                f,
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"live"}}]}}}}"#
            )
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(950)).await;
        stop.store(true, Ordering::Relaxed);
        let _ = handle.await;

        assert!(mock
            .requests()
            .iter()
            .any(|r| r.path == "/v1/agents/sessions/sess_t/events" && r.json()["kind"] == "message"));
    }
}
