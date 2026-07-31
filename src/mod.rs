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
pub mod context;
mod control;
mod dev;
mod event;
mod home;
pub mod session;
mod settings;
pub mod target;
mod theme;
mod tier;
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

use backend::{resolve, resolve_mcp, Approval, BackendKind, Backend, Launch, Mode, Route, Routing, Spec};
use context::{ResumeRecord, Snapshot};
use control::{Act, Command};
use event::{Kind, Mapped, Status, Usage};
use session::SessionClient;
use settings::Settings;

/// Parsed `hanzo code` invocation.
pub struct Options {
    pub backend: String,
    pub link: bool,
    pub no_link: bool,
    pub route: bool,
    pub mcp: bool,
    pub project_mcp: bool,
    /// Opt OUT of auto-approve (`--ask` / `--safe`): the backend runs in its own
    /// ask-for-permission mode. Wins over `~/.hanzo/settings.json` `autoApprove`.
    pub ask: bool,
    /// Escalate PAST auto-approve to a full bypass that also drops the sandbox
    /// (`--no-sandbox`): `dev` `--dangerously-bypass-approvals-and-sandbox`. A
    /// deliberate per-invocation act, never a persisted default (fail-secure).
    pub no_sandbox: bool,
    pub resume: Option<String>,
    pub brand: String,
    /// Claude theme to apply (None → the persisted `code.theme`; "none" → skip).
    pub theme: Option<String>,
    /// The gateway model to name for this run (`--model`). None → an exported env
    /// (`ANTHROPIC_MODEL`), then `~/.hanzo/settings.json`, then the built-in default
    /// (see [`resolve_model`]). Applies ONLY to a gateway route; a direct provider
    /// route names no model.
    pub model: Option<String>,
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

/// The gateway's default coding + small/fast models. Claude Code's built-in
/// default (`claude-fable-5`) is NOT in the gateway catalog, so a routed run that
/// names no model 400s ("model … is not available"); `dev`/codex's built-in
/// default is likewise absent. We name Hanzo's own models, which the catalog
/// carries as bare ids — so a default `hanzo code` works with no per-machine env.
/// `enso` is the default; `enso-ultra` is the flagship a user can pick with
/// `--model enso-ultra`. Overridable in `~/.hanzo/settings.json` (see [`Settings`]).
pub(super) const DEFAULT_MODEL: &str = "enso";
pub(super) const DEFAULT_SMALL_FAST_MODEL: &str = "enso-flash";

/// The context window `hanzo code` requests on the gateway route by default: the
/// real 1M window Hanzo's frontier models serve. A coding backend pointed at a
/// custom gateway can't verify this and self-clamps to the standard 200K, so the
/// gateway route NAMES the window (Claude `[1m]` suffix, `dev` `model_catalog_json`).
/// Overridable in `~/.hanzo/settings.json` `contextWindow`; below the standard
/// window it's a no-op (opting out of the extended window).
pub(super) const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;

/// Resolve the auto-approve decision by precedence: an explicit CLI flag wins,
/// then `~/.hanzo/settings.json` `autoApprove`, then the built-in default (ON).
/// PURE + testable; the impure settings read lives in [`run`].
///
/// `--no-sandbox` (escalate) and `--ask`/`--safe` (opt out) are mutually exclusive
/// at the clap layer, so at most one is set. The persisted `autoApprove` can turn
/// the default OFF but can NEVER reach `Bypass`: dropping the sandbox is always a
/// deliberate per-invocation `--no-sandbox`, never a stored default (fail-secure).
fn resolve_approval(ask: bool, no_sandbox: bool, settings: Option<bool>) -> Approval {
    if no_sandbox {
        Approval::Bypass
    } else if ask {
        Approval::Ask
    } else if settings.unwrap_or(true) {
        Approval::Auto
    } else {
        Approval::Ask
    }
}

/// Resolve one gateway model id by precedence — the first NON-EMPTY of: an explicit
/// `--model` flag, the user's own exported env, the `~/.hanzo/settings.json` value,
/// else the built-in default. PURE + testable; the impure reads live in [`run`].
///
/// No allowlist: the gateway is the sole authority on validity, so a bad id simply
/// 400s with the gateway's own message rather than being rejected client-side.
fn resolve_model(flag: Option<&str>, user_env: Option<&str>, settings: Option<&str>, default: &str) -> String {
    [flag, user_env, settings]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or(default)
        .to_string()
}

/// The resolved gateway model selection for a run — carried into `Routing::Gateway`
/// so the model mapping lives in exactly ONE place and can never reach a direct route.
struct GatewayModels {
    model: String,
    small_fast_model: String,
    context_window: u64,
}

/// Resolve both gateway models once, reading the user's exported env and the
/// native `~/.hanzo/settings.json`. The `ANTHROPIC_*` vars are honored ONLY for
/// the Claude backend, whose model auth reads them; `dev`/codex reads neither, so
/// its model is `--model` > settings > built-in default (and it has no small/fast
/// model at all).
fn gateway_models(backend: BackendKind, model_flag: Option<&str>, settings: &Settings) -> GatewayModels {
    let claude = backend == BackendKind::Claude;
    let env = |k: &str| claude.then(|| std::env::var(k).ok()).flatten();
    GatewayModels {
        model: resolve_model(
            model_flag,
            env("ANTHROPIC_MODEL").as_deref(),
            settings.model.as_deref(),
            DEFAULT_MODEL,
        ),
        small_fast_model: resolve_model(
            None,
            env("ANTHROPIC_SMALL_FAST_MODEL").as_deref(),
            settings.small_fast_model.as_deref(),
            DEFAULT_SMALL_FAST_MODEL,
        ),
        context_window: settings.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
    }
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
    settings: &Settings,
    route: bool,
    backend: BackendKind,
    api: &str,
    bearer: Option<&str>,
    model_flag: Option<&str>,
) -> Result<Route> {
    if !route {
        // `--no-route`: the backend uses its OWN account; we touch no env.
        return Ok(Route::Inherit);
    }
    // The model rides ONLY the gateway route (resolved once here). Both gateway
    // credentials build the SAME variant, so one closure keeps the mapping DRY.
    let models = gateway_models(backend, model_flag, settings);
    let gateway = |token: String| Routing::Gateway {
        api: api.to_string(),
        token,
        model: models.model.clone(),
        small_fast_model: models.small_fast_model.clone(),
        context_window: models.context_window,
    };
    let provider = cfg.auth.provider.as_deref();
    for cred in route_plan(backend, provider, bearer.is_some()) {
        match cred {
            Cred::Bearer => {
                if let Some(token) = bearer {
                    return Ok(Route::Via(gateway(token.to_string())));
                }
            }
            Cred::HanzoKey => {
                if let Some(token) = provider::key(Provider::Hanzo)? {
                    return Ok(Route::Via(gateway(token)));
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
             (`hanzo auth use {other}` to go back to the original session.)",
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
        warn("not signed in — run `hanzo auth login` to link this session. Continuing locally (no cloud stream).");
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

    // The native `~/.hanzo/settings.json` — the ONE home for the coding agent's
    // defaults (model, small/fast model, auto-approve, MCP on/off, context window).
    // Loaded once, read below; every value still yields to an explicit CLI flag /
    // process env. Best-effort: a missing file is created with the defaults, a bad
    // one degrades to them.
    let settings = Settings::load();

    // Auto-approve: run the agent's actions without a per-action prompt by default
    // (the confirmed default). `--ask`/`--safe` opt out; `--no-sandbox` escalates to
    // a full bypass; else `~/.hanzo/settings.json` `autoApprove` decides, ON by
    // default. This governs ONLY the backend's permission mode — never the trust
    // gate: the repo `.mcp.json`/settings stay untrusted (`--strict-mcp-config` +
    // `--setting-sources user`) regardless, so auto-approve can't reopen the
    // bearer-exfil vector. Each backend maps the decision to its own flags.
    let approval = resolve_approval(opts.ask, opts.no_sandbox, settings.auto_approve);

    // MCP: attach hanzo-mcp by default. `--no-mcp` forces it off (flag wins);
    // otherwise `~/.hanzo/settings.json` `mcp` decides, defaulting ON. A missing
    // server warns but never blocks.
    let mcp = if opts.mcp && settings.mcp.unwrap_or(true) {
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
    let routing = resolve_routing(cfg, &settings, opts.route, kind, &api, bearer.as_deref(), opts.model.as_deref())?;
    // A SELECTED provider with no usable key fails closed: the backend clears its
    // model-auth env (below), and we say WHY rather than let the route silently
    // vanish into an inherited endpoint. `provider` is always `Some` here — it is
    // what makes the outcome `FailClosed` rather than `Inherit`.
    if matches!(routing, Route::FailClosed) {
        if let Some(p) = cfg.auth.provider.as_deref() {
            warn(&format!(
                "selected provider `{p}` has no usable key — run `hanzo auth login` (or pass `--no-route` \
                 to use the backend's own account). Model calls will NOT route, and the child's \
                 inherited model credentials are cleared."
            ));
        }
    }

    // Seed Hanzo's OWN Claude config home (`~/.hanzo/claude`) with the first-run
    // defaults a wrapped session needs — but only on a route that relocates it, so
    // `--no-route` never writes Hanzo's defaults into the user's own install. This
    // is the one WRITE; `claude::build` and the transcript pointer only READ the
    // same pure function, so they cannot disagree about where the home is.
    if kind == BackendKind::Claude {
        home::prepare(&routing);
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
    // Native — writes `<config home>/themes` + selects it; never patches Claude. The
    // guard restores the prior theme when this session ends (any exit path). `dev`
    // has no Claude themes. Held to end-of-run so plain `claude` keeps its theme.
    // Takes the ROUTE because that is what decides which home this session reads.
    let _theme_guard = (kind == BackendKind::Claude)
        .then(|| theme::apply(&routing, opts.theme.as_deref(), &cfg.code.theme));

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
        approval,
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
    // The route also decides which config home the backend wrote its transcript in,
    // so keep it for the pointer recorded after the child exits — the supervisor
    // consumes `spec`. One value, read twice; never re-derived.
    let route = spec.routing.clone();
    let launch = backend.build(&spec)?;

    // The session id we watch for the interactive transcript tail.
    let watch_sid = resume_handle.clone().or(preset_session);

    match mode {
        Mode::Headless => {
            let (outcome, status) =
                supervise(&*backend, spec, launch, structured, client.clone(), session_id.clone())
                    .await?;
            if let (Some(c), Some(id)) = (&client, &session_id) {
                let transcript = outcome
                    .backend_session
                    .as_ref()
                    .and_then(|bs| backend.transcript_path(&route, &cwd, bs))
                    .map(|p| p.display().to_string());
                finalize(c, id, &outcome, status, &snapshot, &api, &who, transcript).await;
                report_link(id);
            }
        }
        Mode::Interactive => {
            let ok = run_interactive(&*backend, launch, client.clone(), session_id.clone(), &route, &cwd, watch_sid).await?;
            if let (Some(c), Some(id)) = (&client, &session_id) {
                // Interactive per-event stream arrives via the transcript tail;
                // the resume handle here is what we pre-set / resumed.
                let bs = resume_handle.or_else(|| preset_session_of(&spec));
                let outcome = Outcome { backend_session: bs, ..Default::default() };
                let transcript = outcome
                    .backend_session
                    .as_ref()
                    .and_then(|s| backend.transcript_path(&route, &cwd, s))
                    .map(|p| p.display().to_string());
                // An interactive session SUSPENDS rather than completes: the
                // transcript and its resume handle outlive the TUI, so the same
                // id is reopenable. A crash is still an error.
                let status = if ok { Status::Paused } else { Status::Error };
                finalize(c, id, &outcome, status, &snapshot, &api, &who, transcript).await;
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
/// and set the final status.
///
/// The status is DECIDED BY THE CALLER, not inferred here. That is the whole
/// point: an uncommanded run derives it from the exit code, an interactive run
/// suspends to `paused`, and a remotely-commanded run takes it from the command —
/// so a `stop` lands as a clean `done` even though the signalled child exits 143.
/// Folding those three readings into one `bool` is what made a commanded stop
/// look like a crash. All cloud writes are best-effort (a hiccup never crashes
/// the CLI).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finalize(
    client: &SessionClient,
    session_id: &str,
    outcome: &Outcome,
    status: Status,
    snapshot: &Snapshot,
    api: &str,
    // `identity` is the owner of this cloud session, for the LOCAL resume record
    // ONLY. It is deliberately not part of `Snapshot`: the snapshot is emitted to
    // cloud, and the CLI never sends an org — cloud derives it from the JWT.
    identity: &str,
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

    let _ = client.set_status(session_id, status).await;
}

// ---- process execution ----

/// Run ONE headless turn, watching for a steering command while it streams.
///
/// The turn ends either because the backend finished (stdout hits EOF) or because
/// a command arrived and we signalled the child. In BOTH cases we keep reading to
/// EOF before returning: an interrupted Claude still writes `[Request interrupted
/// by user for tool use]` and a final `result`, and those are the most useful
/// events in the whole run — dropping them to exit a millisecond sooner would
/// throw away the record of what the interrupt actually stopped.
///
/// Only the FIRST actionable command is applied per turn (`acted.is_none()`), so
/// a double-click on the dashboard cannot signal a child twice.
async fn run_turn(
    backend: &dyn Backend,
    structured: bool,
    launch: Launch,
    client: Option<SessionClient>,
    session_id: Option<String>,
    control: Option<&mut tokio::sync::mpsc::Receiver<Command>>,
) -> Result<(Outcome, bool, Option<Act>)> {
    let Launch { mut command, cleanup } = launch;
    command.stdin(Stdio::inherit()).stderr(Stdio::inherit());
    if structured {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::inherit());
    }
    let mut child = command.spawn().map_err(spawn_err)?;
    let pid = child.id();
    let mut acted: Option<Act> = None;

    let outcome = match (structured, control) {
        (false, _) => Outcome::default(),
        (true, control) => {
            let stdout = child.stdout.take().expect("piped stdout");
            let reader = tokio::io::BufReader::new(stdout);
            let streaming = run_stream(backend, reader, client, session_id, true);
            tokio::pin!(streaming);
            match control {
                // Unlinked: nothing can steer this run, so there is nothing to
                // select on. The privacy gate is structural here too.
                None => streaming.await?,
                Some(rx) => loop {
                    tokio::select! {
                        out = &mut streaming => break out?,
                        Some(cmd) = rx.recv(), if acted.is_none() => {
                            let act = cmd.act();
                            if let Some(sig) = act.signal() {
                                // A signal we cannot deliver (Windows has no
                                // SIGINT for another process) must still stop the
                                // run — fall back to the platform's own kill
                                // rather than let a remote stop silently do
                                // nothing.
                                if let Some(p) = pid {
                                    if control::send(p, sig).is_err() {
                                        let _ = child.start_kill();
                                    }
                                }
                            }
                            if act.ends_turn() {
                                acted = Some(act);
                            }
                        }
                    }
                },
            }
        }
    };

    let status = child.wait().await.context("waiting for backend")?;
    drop(cleanup);
    Ok((outcome, status.success(), acted))
}

/// Drive a headless run to completion under remote control, returning the
/// accumulated outcome and the status to finalize the cloud session with.
///
/// A `message` command makes this loop: the child is interrupted, the spec is
/// rebuilt with the backend's OWN resume handle plus the new instruction, and the
/// next turn continues the SAME conversation — native `--resume`, never injected
/// keystrokes. The cloud session id never changes across a steer, so the
/// dashboard watches one unbroken session while the turns underneath it restart.
async fn supervise(
    backend: &dyn Backend,
    mut spec: Spec,
    mut launch: Launch,
    structured: bool,
    client: Option<SessionClient>,
    session_id: Option<String>,
) -> Result<(Outcome, Status)> {
    // The drain runs only for a linked run — same structural auth gate as the
    // event stream. An unlinked run holds no client, so nothing can steer it.
    let stop = Arc::new(AtomicBool::new(false));
    let mut rx = match (&client, &session_id) {
        (Some(c), Some(id)) => Some(control::drain(c.clone(), id.clone(), stop.clone())),
        _ => None,
    };

    let mut acc = Outcome::default();
    let status = loop {
        let (out, ok, act) =
            run_turn(backend, structured, launch, client.clone(), session_id.clone(), rx.as_mut())
                .await?;

        // Fold this turn into the run. The backend session id is set once — it is
        // the SAME conversation across every steer — while usage accumulates and
        // the newest summary wins.
        acc.backend_session = acc.backend_session.or(out.backend_session);
        acc.usage.merge(out.usage);
        acc.saw_error = out.saw_error;
        if out.final_summary.is_some() {
            acc.final_summary = out.final_summary;
        }

        match act {
            // Commanded end: the COMMAND decides the status, not the exit code —
            // a stop exits 143 and is still a clean `done`, never an `error`.
            Some(Act::End { status, .. }) => break status,
            Some(Act::Steer { prompt, .. }) => {
                // Resume the conversation the run has BUILT UP — unless there
                // isn't one yet. A turn that never disclosed a session id was
                // interrupted before the backend finished starting, so there is
                // no transcript to preserve and a fresh launch loses nothing;
                // relaunching either way is what makes a steer land whenever the
                // human clicks it, rather than only after start-up completes.
                spec.resume = acc.backend_session.clone();
                spec.task = Some(prompt);
                // `--resume` and `--session-id` are mutually exclusive, so a
                // resumed turn drops the pre-set id.
                if spec.resume.is_some() {
                    spec.preset_session = None;
                }
                launch = backend.build(&spec)?;
            }
            // Uncommanded end: the backend finished (or failed) on its own.
            None | Some(Act::Ignore) => {
                break if ok { Status::Done } else { Status::Error };
            }
        }
    };

    stop.store(true, Ordering::Relaxed);
    Ok((acc, status))
}

async fn run_interactive(
    backend: &dyn Backend,
    launch: Launch,
    client: Option<SessionClient>,
    session_id: Option<String>,
    // The route this run launched with — it names the config home the transcript
    // is written in, so the tail must resolve against the same one.
    route: &Route,
    cwd: &Path,
    watch_sid: Option<String>,
) -> Result<bool> {
    let Launch { mut command, cleanup } = launch;
    command.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let mut child = command.spawn().map_err(spawn_err)?;

    // Linked interactive per-event streaming rides the backend transcript tail.
    let stop = Arc::new(AtomicBool::new(false));
    let tail = match (&client, &session_id, &watch_sid) {
        (Some(c), Some(id), Some(sid)) => backend.transcript_path(route, cwd, sid).map(|path| {
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
            "model routing: off (sign in with `hanzo auth login` to route + meter model calls)".to_string()
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
    // A COPY-PASTEABLE command and nothing else: `hanzo --resume <id>` now parses at
    // the top level (a bare-`hanzo` code flag — see `main::Cli`), so the whole line is
    // a valid invocation with no label prefix to accidentally copy. Bare id (no `sess_`).
    let short = id.strip_prefix("sess_").unwrap_or(id);
    println!("{}", format!("hanzo --resume {short}").magenta());
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
        assert!(note.contains("hanzo auth use hanzo/z"), "{note}");
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
        finalize(&client, "sess_9", &outcome, Status::Done, &snapshot, "https://api.hanzo.ai", "hanzo/z", None).await;

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
        finalize(&client, "sess_i", &outcome, Status::Paused, &snapshot, "https://api.hanzo.ai", "hanzo/z", None).await;
        assert!(mock.requests().iter().any(|r| r.method == "PATCH" && r.json()["status"] == "paused"));

        let mock2 = MockCloud::start().await;
        let client2 = SessionClient::new(&mock2.base_url(), "T").unwrap();
        finalize(&client2, "sess_e", &outcome, Status::Error, &snapshot, "https://api.hanzo.ai", "hanzo/z", None).await;
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
        fn transcript_path(&self, _: &Route, _: &Path, _: &str) -> Option<PathBuf> {
            None
        }
    }

    fn dummy_spec() -> Spec {
        Spec {
            mode: Mode::Headless,
            task: Some("t".into()),
            cwd: PathBuf::from("."),
            routing: Route::Inherit,
            approval: Approval::Auto,
            mcp: None,
            structured: true,
            preset_session: None,
            trust_project: false,
            resume: None,
            passthrough: vec![],
        }
    }

    #[tokio::test]
    async fn run_turn_spawns_a_real_child_and_forwards_end_to_end() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let fixture = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sid-1","model":"m"}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t","name":"Bash","input":{"command":"ls"}}]}}"#, "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":5,"output_tokens":2},"result":"ok"}"#, "\n"
        );
        let fake = FakeBackend { fixture: fixture.into() };
        let launch = fake.build(&dummy_spec()).unwrap();
        let (out, ok, act) =
            run_turn(&fake, true, launch, Some(client), Some("sess_e2e".into()), None).await.unwrap();

        assert!(ok, "child exited zero");
        assert!(act.is_none(), "an unsteered turn ends on its own terms");
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
            ask: false,
            no_sandbox: false,
            resume: None,
            brand: "hanzo".into(),
            task: None,
            theme: None,
            model: None,
            passthrough: vec![],
        };
        // Routing ON (gateway), stream OFF — the exact case LOW-1 flagged:
        // --no-link but model calls still ship code to the gateway.
        let gw = Routing::Gateway { api: "https://api.hanzo.ai".into(), token: "T".into(), model: "enso".into(), small_fast_model: "enso-flash".into(), context_window: 1_000_000 };
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
            ask: false,
            no_sandbox: false,
            resume: None,
            brand: "hanzo".into(),
            task: None,
            theme: None,
            model: None,
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

    /// The model precedence — `--model` flag, then exported env, then
    /// `~/.hanzo/settings.json`, then the built-in default — skipping any EMPTY
    /// tier. This is the whole policy; the impure env read in `gateway_models` just
    /// supplies the middle argument.
    #[test]
    fn resolve_model_precedence() {
        // Flag wins over everything, even a set env + settings.
        assert_eq!(resolve_model(Some("enso-ultra"), Some("env"), Some("file"), "enso"), "enso-ultra");
        // No flag → the user's exported env is PRESERVED over settings + default.
        assert_eq!(resolve_model(None, Some("env-model"), Some("file"), "enso"), "env-model");
        // No flag, no env → the persisted settings value.
        assert_eq!(resolve_model(None, None, Some("file-model"), "enso"), "file-model");
        // Nothing set → the built-in default.
        assert_eq!(resolve_model(None, None, None, "enso"), "enso");
        // Empty / whitespace-only tiers name no model and are skipped.
        assert_eq!(resolve_model(Some("  "), Some(""), Some("file"), "enso"), "file");
        assert_eq!(resolve_model(Some(""), Some("  "), Some("  "), "enso"), "enso");
    }

    /// The auto-approve precedence: an explicit CLI flag wins, then the persisted
    /// `autoApprove`, then the built-in default (ON → `Auto`). The escalation
    /// (`--no-sandbox` → `Bypass`) and the opt-out (`--ask`/`--safe` → `Ask`) both
    /// beat the persisted setting; the setting can turn the default OFF but can
    /// NEVER reach `Bypass` — dropping the sandbox is always a per-invocation act.
    #[test]
    fn resolve_approval_precedence() {
        // No flags: the persisted setting decides, defaulting ON (Auto).
        assert_eq!(resolve_approval(false, false, None), Approval::Auto);
        assert_eq!(resolve_approval(false, false, Some(true)), Approval::Auto);
        assert_eq!(resolve_approval(false, false, Some(false)), Approval::Ask);
        // `--ask`/`--safe` opt out, winning over a persisted `true`.
        assert_eq!(resolve_approval(true, false, Some(true)), Approval::Ask);
        assert_eq!(resolve_approval(true, false, None), Approval::Ask);
        // `--no-sandbox` escalates, winning over any setting — including `false`
        // (the flag is explicit; the setting can never itself reach Bypass).
        assert_eq!(resolve_approval(false, true, Some(false)), Approval::Bypass);
        assert_eq!(resolve_approval(false, true, Some(true)), Approval::Bypass);
        assert_eq!(resolve_approval(false, true, None), Approval::Bypass);
    }

    /// The context window rides the gateway route by precedence `settings > default`,
    /// resolved once in `gateway_models` and carried only inside `Routing::Gateway`.
    #[test]
    fn gateway_models_resolves_the_context_window() {
        use BackendKind::Dev;
        // Unset → the built-in 1M default.
        let none = settings::Settings::default();
        assert_eq!(gateway_models(Dev, None, &none).context_window, DEFAULT_CONTEXT_WINDOW);
        // A settings value wins over the default (e.g. opting the window to standard).
        let file = settings::Settings { context_window: Some(200_000), ..Default::default() };
        assert_eq!(gateway_models(Dev, None, &file).context_window, 200_000);
    }

    /// `gateway_models` reads the exported `ANTHROPIC_*` env for the CLAUDE backend
    /// (whose model auth honors them), but NOT for `dev` (codex reads neither), and
    /// a `--model` flag overrides the env either way. This is the only path that
    /// touches process env, so it owns `ANTHROPIC_MODEL` for the test's lifetime.
    #[test]
    fn gateway_models_reads_env_for_claude_only() {
        use BackendKind::{Claude, Dev};
        let none = settings::Settings::default();

        std::env::set_var("ANTHROPIC_MODEL", "user-exported");
        std::env::set_var("ANTHROPIC_SMALL_FAST_MODEL", "user-fast");

        // Claude: the exported env is preserved (no flag), on BOTH models.
        let g = gateway_models(Claude, None, &none);
        assert_eq!(g.model, "user-exported");
        assert_eq!(g.small_fast_model, "user-fast");

        // A `--model` flag overrides the exported env; the small/fast still tracks env.
        let g = gateway_models(Claude, Some("enso-ultra"), &none);
        assert_eq!(g.model, "enso-ultra");
        assert_eq!(g.small_fast_model, "user-fast");

        // dev/codex reads NO `ANTHROPIC_*` env → the built-in default, ignoring the
        // exported Claude vars entirely.
        let g = gateway_models(Dev, None, &none);
        assert_eq!(g.model, DEFAULT_MODEL);
        assert_eq!(g.small_fast_model, DEFAULT_SMALL_FAST_MODEL);

        // Settings supply the value when neither flag nor env is present (dev path,
        // no env read): the file's model wins over the built-in default.
        let file = settings::Settings { model: Some("file-model".into()), ..Default::default() };
        assert_eq!(gateway_models(Dev, None, &file).model, "file-model");

        std::env::remove_var("ANTHROPIC_MODEL");
        std::env::remove_var("ANTHROPIC_SMALL_FAST_MODEL");
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

/// The remote-control proofs: the session channel's INBOUND half, driven end to
/// end against the mock control plane and REAL child processes.
///
/// These do not stub the supervisor — `supervise` is the function under test,
/// signals are delivered to a real pid, and the commands arrive over HTTP from
/// the same drain contract cloud serves. What is stubbed is only Claude itself,
/// because a test must not need an API key; the shapes it emits are copied
/// verbatim from a recorded run (see `INTERRUPTED`).
#[cfg(test)]
mod control_tests {
    use super::*;
    use crate::commands::code::claude::Claude;
    use crate::commands::code::testmock::MockCloud;
    use std::sync::Mutex;
    use tokio::io::AsyncWriteExt;

    /// A REAL recorded interrupt, trimmed to the three lines that matter.
    /// Captured from `claude -p … --output-format stream-json` interrupted
    /// mid-`Bash` by SIGINT: Claude aborts the tool, says so in a user turn, and
    /// still writes a final `result` carrying `terminal_reason:"aborted_tools"`.
    /// That last line is why an interrupted run is still worth streaming.
    const INTERRUPTED: &str = concat!(
        r#"{"type":"system","subtype":"init","session_id":"36d41361-9768-4c07-98cb-4a15984c5bf1","model":"claude-sonnet-5"}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]},"session_id":"36d41361-9768-4c07-98cb-4a15984c5bf1"}"#,
        "\n",
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools","session_id":"36d41361-9768-4c07-98cb-4a15984c5bf1","result":null,"num_turns":3,"total_cost_usd":0.10867639999999999,"duration_ms":11232}"#,
        "\n",
    );

    async fn reader_of(fixture: &str) -> impl AsyncBufRead + Unpin {
        let (r, mut w) = tokio::io::duplex(1 << 20);
        w.write_all(fixture.as_bytes()).await.unwrap();
        drop(w);
        tokio::io::BufReader::new(r)
    }

    /// A backend that runs a scripted shell per turn and records every `Spec` it
    /// was asked to build — which is how the steer test proves the SECOND turn
    /// resumed the FIRST turn's session id.
    struct ScriptBackend {
        scripts: Vec<&'static str>,
        /// `(resume, task)` per build, in order.
        built: Arc<Mutex<Vec<(Option<String>, Option<String>)>>>,
    }

    impl ScriptBackend {
        fn new(scripts: Vec<&'static str>) -> (Self, Arc<Mutex<Vec<(Option<String>, Option<String>)>>>) {
            let built = Arc::new(Mutex::new(Vec::new()));
            (ScriptBackend { scripts, built: built.clone() }, built)
        }
    }

    impl Backend for ScriptBackend {
        fn label(&self) -> &'static str {
            "claude"
        }
        fn version(&self) -> Option<String> {
            None
        }
        fn build(&self, spec: &Spec) -> Result<Launch> {
            let mut b = self.built.lock().unwrap();
            let idx = b.len().min(self.scripts.len() - 1);
            b.push((spec.resume.clone(), spec.task.clone()));
            let mut command = tokio::process::Command::new("sh");
            command.arg("-c").arg(self.scripts[idx]);
            Ok(Launch { command, cleanup: Vec::new() })
        }
        /// The real Claude parser — so these tests exercise the SAME mapping the
        /// production stream does, on the same recorded shapes.
        fn parse(&self, line: &str) -> Vec<Mapped> {
            Claude.parse(line)
        }
        fn transcript_path(&self, _: &Route, _: &Path, _: &str) -> Option<PathBuf> {
            None
        }
    }

    fn spec_for(task: &str) -> Spec {
        Spec {
            mode: Mode::Headless,
            task: Some(task.to_string()),
            cwd: std::env::temp_dir(),
            routing: Route::Inherit,
            approval: Approval::Auto,
            mcp: None,
            structured: true,
            preset_session: None,
            trust_project: false,
            resume: None,
            passthrough: Vec::new(),
        }
    }

    /// Emit an init line disclosing a backend session id, then hang forever.
    /// `exec` matters: it replaces the shell so the pid we signal IS the process
    /// holding the stdout pipe — otherwise a surviving grandchild would keep the
    /// pipe open and the stream would never see EOF.
    const HANGS: &str = concat!(
        r#"echo '{"type":"system","subtype":"init","session_id":"bsid-1","model":"m"}'; "#,
        "exec sleep 30",
    );

    /// Runs briefly and finishes on its OWN terms, announcing it did.
    const COMPLETES: &str = concat!(
        r#"echo '{"type":"system","subtype":"init","session_id":"bsid-1","model":"m"}'; "#,
        "sleep 2; ",
        r#"echo '{"type":"result","subtype":"success","is_error":false,"session_id":"bsid-1","result":"finished","num_turns":1}'"#,
    );

    async fn supervise_with(
        mock: &MockCloud,
        session: &str,
        scripts: Vec<&'static str>,
    ) -> (Outcome, Status, Arc<Mutex<Vec<(Option<String>, Option<String>)>>>, std::time::Duration) {
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let (backend, built) = ScriptBackend::new(scripts);
        let spec = spec_for("original task");
        // This `build` is the turn-0 invocation the orchestrator performs before
        // handing the launch to the supervisor, so `built[0]` is turn 0 and any
        // further entry is a steer's relaunch.
        let launch = backend.build(&spec).unwrap();
        let started = std::time::Instant::now();
        let (out, status) = supervise(
            &backend,
            spec,
            launch,
            true,
            Some(client),
            Some(session.to_string()),
        )
        .await
        .unwrap();
        (out, status, built, started.elapsed())
    }

    // ---- the event path, from a recorded interrupt --------------------------

    /// The interrupt's OWN events reach the channel with the right kinds and the
    /// right identity — including the abort notice and the terminal `result`.
    /// This is the shape a dashboard renders when a human clicks interrupt.
    #[tokio::test]
    async fn interrupt_events_surface_on_the_channel_with_correct_shape() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let reader = reader_of(INTERRUPTED).await;

        let out = run_stream(&Claude, reader, Some(client), Some("sess_1".into()), false)
            .await
            .unwrap();

        // The resume handle is the whole reason an interrupted session survives.
        assert_eq!(out.backend_session.as_deref(), Some("36d41361-9768-4c07-98cb-4a15984c5bf1"));
        assert!(out.saw_error, "aborted_tools is a non-ok terminal");

        let posts: Vec<_> = mock
            .requests()
            .into_iter()
            .filter(|r| r.path == "/v1/agents/sessions/sess_1/events")
            .collect();
        assert!(!posts.is_empty(), "the interrupt must be streamed, not swallowed");

        // Every event is routed to OUR session and carries the bearer, never an org.
        for r in &posts {
            assert_eq!(r.header("authorization").as_deref(), Some("Bearer T"));
            assert!(r.header("x-org-id").is_none(), "the CLI must never assert an org");
        }
        let texts: Vec<String> = posts.iter().map(|r| r.body.clone()).collect();
        let joined = texts.join("\n");
        assert!(
            joined.contains("[Request interrupted by user for tool use]"),
            "the abort notice must reach the dashboard; got: {joined}"
        );
    }

    // ---- one test per control op -------------------------------------------

    /// `stop` ACTUALLY terminates the child: a run scripted to sleep 30s ends in
    /// well under that, and the session finalizes `done` — not `error`, even
    /// though a signalled child exits non-zero.
    #[tokio::test]
    async fn stop_terminates_the_child_and_finalizes_done() {
        let mock = MockCloud::start_with_control(&[(1, "stop", "")]).await;
        let (_out, status, built, elapsed) =
            supervise_with(&mock, "sess_1", vec![HANGS]).await;

        assert!(
            elapsed < Duration::from_secs(25),
            "stop must kill the child, not wait it out (took {elapsed:?})"
        );
        assert_eq!(status, Status::Done, "a commanded stop is finished, not failed");
        assert_eq!(built.lock().unwrap().len(), 1, "stop must not relaunch");

        // And the terminal status actually reached cloud.
        let patches: Vec<_> = mock
            .requests()
            .into_iter()
            .filter(|r| r.method == "PATCH" && r.path == "/v1/agents/sessions/sess_1")
            .collect();
        assert!(patches.is_empty(), "supervise itself does not PATCH; finalize does");
    }

    /// `pause` halts the operation but leaves the session ALIVE: the status is
    /// `paused` (non-terminal, so cloud will reopen it) and the backend's resume
    /// handle survived, which is what makes reopening possible at all.
    #[tokio::test]
    async fn interrupt_halts_the_operation_without_destroying_the_session() {
        // Delayed one poll so the backend has certainly disclosed its session id:
        // this test is about the handle SURVIVING the interrupt, so the interrupt
        // must land after there is a handle to survive.
        let mock = MockCloud::start_with_delayed_control(1, &[(1, "pause", "")]).await;
        let (out, status, built, elapsed) = supervise_with(&mock, "sess_1", vec![HANGS]).await;

        assert!(elapsed < Duration::from_secs(25), "pause must interrupt promptly");
        assert_eq!(status, Status::Paused);
        assert_ne!(status, Status::Done, "pause must NOT close the session");
        assert_ne!(status, Status::Error, "an interrupt is not a failure");
        assert_eq!(
            out.backend_session.as_deref(),
            Some("bsid-1"),
            "the resume handle must survive an interrupt — without it the session is unreachable"
        );
        assert_eq!(built.lock().unwrap().len(), 1, "pause must not relaunch");
    }

    /// `message` steers: the child is interrupted, then the SAME conversation
    /// continues via the backend's native `--resume` carrying the new prompt.
    /// The cloud session id never changes across the steer.
    #[tokio::test]
    async fn steer_resumes_the_same_session_with_the_new_prompt() {
        // Delayed by one poll so the first turn has certainly disclosed its
        // session id — this test asserts the RESUME-carrying path specifically.
        let mock =
            MockCloud::start_with_delayed_control(1, &[(1, "message", "now do X instead")]).await;
        let (_out, status, built, _) =
            supervise_with(&mock, "sess_1", vec![HANGS, COMPLETES]).await;

        let built = built.lock().unwrap().clone();
        assert_eq!(built.len(), 2, "a steer must run a SECOND turn; got {built:?}");

        // Turn 1: the original task, no resume.
        assert_eq!(built[0].0, None, "the first turn resumes nothing");
        assert_eq!(built[0].1.as_deref(), Some("original task"));

        // Turn 2: the SAME backend session id, and the steer's prompt.
        assert_eq!(
            built[1].0.as_deref(),
            Some("bsid-1"),
            "the steer must resume the session the first turn disclosed — a new id would lose the context"
        );
        assert_eq!(built[1].1.as_deref(), Some("now do X instead"));

        // The second turn finished on its own terms.
        assert_eq!(status, Status::Done);

        // Every control drain addressed the SAME cloud session throughout.
        let drains: Vec<_> = mock
            .requests()
            .into_iter()
            .filter(|r| r.path.contains("/control"))
            .collect();
        assert!(!drains.is_empty());
        for r in &drains {
            assert!(
                r.path.starts_with("/v1/agents/sessions/sess_1/control"),
                "the steer must not move the session: {}",
                r.path
            );
        }
    }

    /// The other legitimate ordering: a steer that lands BEFORE the backend has
    /// disclosed a session id. There is no transcript to preserve yet, so the
    /// steer must still land — as a fresh launch carrying the new instruction,
    /// never as a dropped command.
    #[tokio::test]
    async fn a_steer_that_beats_startup_still_lands_as_a_fresh_turn() {
        let mock = MockCloud::start_with_control(&[(1, "message", "actually do Y")]).await;
        // This script NEVER discloses a session id — the worst case, deterministically.
        let (_out, status, built, _) =
            supervise_with(&mock, "sess_1", vec!["exec sleep 30", COMPLETES]).await;

        let built = built.lock().unwrap().clone();
        assert_eq!(built.len(), 2, "the steer must still run a second turn; got {built:?}");
        assert_eq!(built[1].0, None, "nothing was disclosed, so there is nothing to resume");
        assert_eq!(
            built[1].1.as_deref(),
            Some("actually do Y"),
            "the human's instruction must survive even when it beats the backend's start-up"
        );
        assert_eq!(status, Status::Done);
    }

    /// `resume` against an already-running session is a no-op — it must not
    /// signal the child or restart anything.
    #[tokio::test]
    async fn resume_does_not_disturb_a_running_session() {
        let mock = MockCloud::start_with_control(&[(1, "resume", "")]).await;
        let (out, status, built, _) = supervise_with(&mock, "sess_1", vec![COMPLETES]).await;

        assert_eq!(built.lock().unwrap().len(), 1, "resume must not relaunch");
        assert_eq!(status, Status::Done);
        assert_eq!(
            out.final_summary.as_deref(),
            Some("finished"),
            "the run must have completed on its own — a resume must not cut it short"
        );
    }

    // ---- adversarial: cross-org control ------------------------------------

    /// A principal may not observe another org's session. The drain for a
    /// foreign id is refused (cloud 404s it), and the refusal is an ERROR here —
    /// never an empty-but-successful page that would read as "no commands".
    #[tokio::test]
    async fn draining_another_orgs_session_is_refused() {
        let mock = MockCloud::start_org_scoped("sess_ours", &[(1, "stop", "")]).await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();

        // Ours: readable.
        let page = client.drain_control("sess_ours", 0).await.unwrap();
        assert_eq!(page.commands.len(), 1, "our own commands are ours to read");

        // Theirs: refused, loudly.
        let err = client
            .drain_control("sess_theirs", 0)
            .await
            .expect_err("a foreign session must not be readable");
        assert!(err.to_string().contains("404"), "got: {err}");

        // And the detail read is refused the same way — there is no seam that
        // leaks the existence of another org's session.
        let err = client.get("sess_theirs").await.expect_err("foreign detail must be refused");
        assert!(err.to_string().contains("404"), "got: {err}");
    }

    /// The adversarial end-to-end: a `stop` sitting in ANOTHER org's queue must
    /// not reach our child. The supervisor polls, is refused, and the run
    /// completes on its own — proving cross-org control is impossible, not
    /// merely discouraged.
    ///
    /// The assertion is the completion marker: a child that had been signalled
    /// would die during its `sleep` and never emit it.
    #[tokio::test]
    async fn another_orgs_stop_cannot_terminate_our_child() {
        // The mock owns `sess_ours` and holds a `stop` for it. We supervise
        // `sess_theirs` — the attacker's aim — and must get nothing.
        let mock = MockCloud::start_org_scoped("sess_ours", &[(1, "stop", "")]).await;
        let (out, status, built, _) =
            supervise_with(&mock, "sess_theirs", vec![COMPLETES]).await;

        assert_eq!(
            out.final_summary.as_deref(),
            Some("finished"),
            "the child ran to completion — a leaked cross-org stop would have killed it mid-sleep"
        );
        assert_eq!(status, Status::Done, "ended on its own terms, not by command");
        assert_eq!(built.lock().unwrap().len(), 1);
        assert_eq!(
            out.backend_session.as_deref(),
            Some("bsid-1"),
            "and the run was otherwise entirely normal"
        );
    }

    /// A refused drain must not be mistaken for "no commands, carry on" in a way
    /// that silently advances the cursor — a later legitimate command would then
    /// be skipped. The cursor only moves on a SUCCESSFUL page.
    #[tokio::test]
    async fn a_refused_drain_does_not_advance_the_cursor() {
        let mock = MockCloud::start_org_scoped("sess_ours", &[(1, "stop", "")]).await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        assert!(client.drain_control("sess_theirs", 0).await.is_err());

        // Our own queue is untouched by the foreign attempt.
        let page = client.drain_control("sess_ours", 0).await.unwrap();
        assert_eq!(page.cursor, 1);
        assert_eq!(page.commands[0].command, "stop");
    }

    /// The cursor contract: an applied command is never redelivered.
    #[tokio::test]
    async fn the_drain_cursor_never_redelivers_an_applied_command() {
        let mock = MockCloud::start_with_control(&[(1, "pause", ""), (2, "stop", "")]).await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();

        let first = client.drain_control("sess_1", 0).await.unwrap();
        assert_eq!(first.commands.len(), 2);
        assert_eq!(first.cursor, 2);

        let second = client.drain_control("sess_1", first.cursor).await.unwrap();
        assert!(second.commands.is_empty(), "already-applied commands must not repeat");
        assert_eq!(second.cursor, 2);
    }

    /// An unlinked run cannot be steered at all — the privacy gate is structural,
    /// exactly as it is for the outbound stream.
    #[tokio::test]
    async fn an_unlinked_run_is_unsteerable() {
        let (backend, built) = ScriptBackend::new(vec![COMPLETES]);
        let spec = spec_for("t");
        let launch = backend.build(&spec).unwrap();
        let _ = &built;

        let (out, status) = supervise(&backend, spec, launch, true, None, None).await.unwrap();
        assert_eq!(status, Status::Done);
        assert_eq!(out.final_summary.as_deref(), Some("finished"));
    }
}
