//! Hanzo Gen — the agentic harness. It launches a coding agent (`dev`, the
//! default, or Claude Code, Codex, Cursor and the rest in [`agents`]) so a
//! developer's terminal session is (opt-in) linked, live-streamed and tracked in
//! Hanzo cloud, with the Hanzo MCP toolset attached and model usage metered
//! universally through the Hanzo gateway. `hanzo dev` and `gen` are this crate;
//! the program around it is its [`Host`].
//!
//! Three things are wired natively:
//!   1. Session link + live stream — register on `/v1/agents/sessions`, forward
//!      the backend's structured events, and mark the terminal status. ON by
//!      default when signed in (streams the user's OWN session to their OWN org,
//!      derived server-side from the JWT `owner`); `--no-link`, or a persisted
//!      `code.link = false`, opts out. Structurally silent when unauthenticated.
//!   2. Hanzo MCP — attached in-session (Claude `--mcp-config`, `dev` `-c`),
//!      with the org's kit beside it: its skills written where the backend reads
//!      them, and the caller's cloud tools as one MCP server (see [`kit`]).
//!   3. hanzo.id auth + universal usage — model calls route through
//!      api.hanzo.ai so tokens/cost meter into cloud_usage/o11y regardless of
//!      which account/machine the dev is on.
//!
//! Sessions are PORTABLE: the register carries a no-secret context snapshot, the
//! backend's own resume handle + a transcript pointer are persisted, and
//! `--resume <sessionId>` restores cwd/repo/ref and relaunches the backend with
//! its native resume against the same cloud session.

pub mod agents;
pub mod args;
pub mod backend;
mod claude;
pub mod context;
mod control;
pub mod contract;
mod dev;
pub mod embedded;
pub mod enso;
// The status vocabulary IS part of the session client's signature (`set_status`),
// so anything that can hold a session — `hanzo link` as much as `hanzo dev` —
// must be able to name the value it is setting.
pub mod event;
pub mod exe;
pub mod harbor;
pub mod harness;
mod home;
pub mod host;
mod kit;
pub mod ledger;
pub mod mcp;
pub mod native;
pub mod private;
pub mod reasoning;
pub mod root;
pub mod runner;
pub mod sample;
pub mod savings;
pub mod session;
pub mod settings;
mod summary;
pub mod target;
mod theme;
pub mod tier;
mod lifecycle;
mod readonly;
mod render;
mod routing;
mod stream;
mod supervise;
#[cfg(test)]
pub(crate) mod testmock;

use anyhow::{anyhow, bail, Result};
use colored::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use args::Args;
pub use enso::enso_glyph;
pub use host::{Account, Host, Profile, Provider};
pub use reasoning::ReasoningEffort;
pub use root::Root;

#[cfg(test)]
pub(crate) use lifecycle::resolve_cloud_session;
pub(crate) use lifecycle::{finalize, register_session, run_routed_agent};
pub(crate) use routing::{DEFAULT_CONTEXT_WINDOW, DEFAULT_MODEL, DEFAULT_SMALL_FAST_MODEL};
use backend::{resolve, resolve_or_install_mcp, BackendKind, Mode, Route, Spec};
use context::{ResumeRecord, Snapshot};
use event::{Kind, Status, Usage};
use render::{banner, report_link, session_title};
use routing::{resolve_approval, resolve_routing};
use session::SessionClient;
use settings::Settings;
use stream::{cut, reason_of, run_interactive, Outcome, REASON_MAX};
use supervise::supervise;

/// every warning looks the same and none of them lands on stdout, where it would
/// corrupt what a caller is piping (`hanzo auth token`).
pub fn warn(msg: &str) {
    warn_to(&mut std::io::stderr(), msg);
}

fn warn_to(w: &mut impl std::io::Write, msg: &str) {
    // Never panic on a closed stderr: a warning from the beat task must not end the beat.
    let _ = writeln!(w, "{} {}", "warning:".yellow().bold(), msg);
}

/// Parsed `hanzo dev` invocation.
pub struct Options {
    /// The backend, ALREADY resolved by [`backend::select`] — the one place any
    /// spelling (`dev --dev`, `--backend dev`, `hanzo dev`) becomes
    /// a backend. It arrives here as a value, not a string to be parsed again.
    pub backend: BackendKind,
    pub link: bool,
    pub no_link: bool,
    /// Run on Hanzo's metered cloud models (`--cloud`). Off, a third-party tool
    /// talks to its own account directly; `dev`, whose account is Hanzo's, routes
    /// through the gateway either way.
    pub cloud: bool,
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
    /// Reasoning effort level (`--effort` / `--reasoning-effort`): `auto`,
    /// `off` (`none`), `minimal`, `low`, `medium`, `high`, `xhigh`, `max`.
    pub effort: Option<ReasoningEffort>,
    pub task: Option<String>,
    pub passthrough: Vec<String>,
    /// Where this session keeps what it writes, decided by the launcher.
    pub root: Root,
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


const REGISTER_WITHIN: Duration = Duration::from_secs(30);

/// The first wait between registration attempts; each next one doubles it.
const REGISTER_WAIT: Duration = Duration::from_millis(500);


/// The reader's configured coding agent, for resolving a bare `hanzo`.
///
/// A narrow accessor rather than a public settings module: the caller needs exactly
/// this one value, and `select` is a pure function that takes it as an argument.
pub fn configured_agent(root: &Root) -> Option<String> {
    let from_env = std::env::var("HANZO_HARNESS_ACCOUNT").ok();
    let settings = Settings::load(root);
    harness::agent_for(
        from_env.as_deref().or(settings.harness.as_deref()),
        settings.agent.as_deref(),
    )
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
    active: Option<&Account>,
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

pub async fn run(host: Arc<dyn Host>, mut opts: Options) -> Result<()> {
    let kind = opts.backend;
    let backend = resolve(kind);
    let mode = if opts.task.is_some() { Mode::Headless } else { Mode::Interactive };
    let profile = host.profile().await;
    let root = opts.root.clone();
    let api = profile.api.clone();
    // The one network whose api may be plain http (the kit's bearer check).
    let local = profile.local;

    // Auth: the ACTIVE identity's hanzo.id bearer, from the host (never
    // argv/logged). The identity rides along because the cloud session this run
    // registers is org-scoped to its `owner` — the two must not drift.
    let identity = host.account(&opts.brand).await?;
    let bearer = identity.as_ref().map(|a| a.token.clone());

    // `owner/name` for the LOCAL resume record — never sent to cloud.
    let who = identity.as_ref().map(Account::to_string).unwrap_or_default();

    let mut do_link = effective_link(opts.link, opts.no_link, profile.link);
    if do_link && bearer.is_none() {
        crate::warn("not signed in — run `hanzo auth login` to link this session. Continuing locally (no cloud stream).");
        do_link = false;
    }

    // Resume: restore cwd + the backend's own resume handle from the local store.
    let (cwd, resume_handle, resume_from) = match &opts.resume {
        Some(raw) => {
            // Accept the id with or without the `sess_` prefix (the resume line prints
            // the bare form), so `hanzo --resume <id>` matches either way.
            let id = &(if raw.starts_with("sess_") { raw.clone() } else { format!("sess_{raw}") });
            let rec = ResumeRecord::load(&root, id)?.ok_or_else(|| {
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
                    crate::warn(&format!(
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
                    crate::warn(&note);
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
    let settings = Settings::load(&root);
    let effort = routing::resolve_effort(
        opts.effort,
        std::env::var("HANZO_REASONING_EFFORT").ok().as_deref(),
        settings.effort.as_deref(),
    );
    opts.effort = effort;

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
    // server warns but never blocks. The same switch governs the cloud's tools.
    let mcp_on = opts.mcp && settings.mcp.unwrap_or(true);
    // Where the proxy records each tool call, for the summary to say what the
    // toolset did. Named here because the server is attached before the cloud
    // session exists.
    let ledger = ledger::path(&root, &format!("{}-{:08x}", chrono::Utc::now().format("%Y%m%dT%H%M%S"), rand::random::<u32>()));
    let mcp = if mcp_on {
        let m = resolve_or_install_mcp(&root, &cwd, Some(&ledger)).await;
        if m.is_none() {
            crate::warn("hanzo-mcp not found — install it with `hanzo upgrade mcp` (or `curl -fsSL hanzo.sh/mcp | sh`); continuing without the Hanzo toolset.");
        }
        m
    } else {
        None
    };
    // What the summary says of the toolset: the ledger it reads, or why there is none.
    let takes = match kind {
        BackendKind::Tool(a) => a.takes_mcp(opts.cloud),
        _ => true,
    };
    let tools: std::result::Result<std::path::PathBuf, String> = match (&mcp, takes, ledger) {
        (_, false, _) => Err(format!("{} takes no MCP server for one run", kind.as_str())),
        (None, _, _) => Err("not attached (`--no-mcp`, or hanzo-mcp is not installed)".into()),
        (Some(_), true, l) => Ok(l),
    };

    // Routing: which model endpoint this run's calls go to, and with what
    // credential — the Hanzo gateway (metered) for a Hanzo login, or a provider's
    // OWN API for a stored OpenAI/Anthropic key. A third-party tool is direct unless `--cloud`.
    let active_org = profile
        .org
        .as_deref()
        .or_else(|| identity.as_ref().map(|a| a.owner.as_str()));
    let routing = resolve_routing(
        &*host,
        profile.provider.as_deref(),
        &settings,
        opts.cloud,
        kind,
        &api,
        bearer.as_deref(),
        active_org,
        opts.model.as_deref(),
    )?;
    // Local usage, one line per session, for whichever connected account this
    // run actually used. A missing home does not stop the session.
    let used = harness::used_account(
        kind,
        matches!(routing, Route::Inherit),
        identity.as_ref().map(|id| (id.owner.as_str(), id.name.as_str())),
    );
    let pin = std::env::var("HANZO_HARNESS_ACCOUNT")
        .ok()
        .or_else(|| settings.harness.clone());
    let active = identity.as_ref().map(|id| format!("{}/{}", id.owner, id.name));
    // Every identity the host holds, and the one signed in: a host with no index
    // of its own still has that one, and the inventory drops the repeat.
    let ids: Vec<(&str, &str)> = profile
        .identities
        .iter()
        .map(|(owner, name)| (owner.as_str(), name.as_str()))
        .chain(identity.as_ref().map(|id| (id.owner.as_str(), id.name.as_str())))
        .collect();
    if let Err(e) = harness::track(&root, &ids, active.as_deref(), pin.as_deref(), &used) {
        crate::warn(&e.to_string());
    }
    // A SELECTED provider with no usable key fails closed: the backend clears its
    // model-auth env (below), and we say WHY rather than let the route silently
    // vanish into an inherited endpoint. `provider` is always `Some` here — it is
    // what makes the outcome `FailClosed` rather than `Inherit`.
    if matches!(routing, Route::FailClosed) {
        if let Some(p) = profile.provider.as_deref() {
            crate::warn(&format!(
                "selected provider `{p}` has no usable key — run `hanzo auth login` (or run without `--cloud` \
                 to use the tool's own account). Model calls will NOT route, and the child's \
                 inherited model credentials are cleared."
            ));
        }
    }

    // Seed Hanzo's OWN Claude config home (`~/.hanzo/claude`) with the first-run
    // defaults a wrapped session needs — but only on a route that relocates it, so
    // a direct run never writes Hanzo's defaults into the user's own install. This
    // is the one WRITE; `claude::build` and the transcript pointer only READ the
    // same pure function, so they cannot disagree about where the home is.
    if kind == BackendKind::Claude {
        home::prepare(&root, &routing);
    }

    // For a linked interactive Claude run, pre-set the session id so its
    // transcript can be tailed; otherwise the resume handle names it.
    let preset_session = if do_link && mode == Mode::Interactive && kind == BackendKind::Claude && opts.resume.is_none() {
        Some(uuid_v4())
    } else {
        None
    };

    let snapshot = Snapshot::capture(&root, &cwd, backend.label(), backend.version(&root));

    let client = if do_link {
        Some(SessionClient::new(&api, bearer.as_deref().unwrap())?.with_org(profile.org.as_deref()))
    } else {
        None
    };

    // The org's kit and the cloud session are two reads of the cloud that nothing
    // between them depends on, and the backend does not start until both are in:
    // they are asked together. A linked run never starts without its session: the
    // registration is retried with backoff and fails the run, saying why, if the cloud
    // does not answer.
    //
    // The kit is the skills an admin activated (and the person left on) written
    // where THIS backend reads them — which for Claude is the home the route just
    // chose — and the caller's tools as one MCP server on the IAM bearer. Read on
    // every run through a short cache; no kit never stops a run, and the banner
    // says so once.
    let equip = async {
        match kit::root(&root, kind, &routing) {
            Some(skills) => {
                let cache = kit::cache(&root);
                let hs_cache = kit::hanzoskills_cache(&root);
                let hs_api = kit::hanzoskills_api();
                let auto = hs_api.as_deref().map(|skills_api| kit::AutoSkills {
                    cache: &hs_cache,
                    api: skills_api,
                    cwd: &cwd,
                    task: opts.task.as_deref(),
                });
                let caller = identity.as_ref().zip(bearer.as_deref()).map(|(_, token)| kit::Caller {
                    api: &api,
                    who: &who,
                    token,
                    local,
                });
                // The cloud's tools ride only a Hanzo-routed run (Spec::cloud), so the
                // banner never claims a server the backend will not be handed.
                Some(
                    kit::equip_with(
                        &cache,
                        &skills,
                        caller.as_ref(),
                        mcp_on && routing.hanzo(),
                        auto.as_ref(),
                    )
                    .await,
                )
            }
            None => None,
        }
    };

    // Cloud session (linked only). Resolve reuses a non-terminal resumed session
    // (same id) or forks a new one off a terminal / fresh session.
    let link = async {
        let Some(c) = client.as_ref() else { return Ok(None) };
        let title = session_title(&opts);
        let (id, forked_from) =
            register_session(c, &snapshot, &title, resume_from.as_deref(), REGISTER_WITHIN, REGISTER_WAIT).await?;
        // The "where it runs" context snapshot (no secrets).
        let _ = c.event(&id, Kind::Status, snapshot.context_payload(forked_from.as_deref())).await;
        Ok::<_, anyhow::Error>(Some(id))
    };
    let (equipped, session_id) = tokio::join!(equip, link);
    let session_id = session_id?;

    // Register/refresh this machine as a cloud run-target so mission-control knows
    // WHICH computer the session runs on and whether it can take more work. DETACHED
    // and BEST-EFFORT: capability + live-metrics probing and the cloud write happen
    // off the critical path and can NEVER block or fail the coding session. Gated on
    // the same structural auth check as the session link (`links_target`) — an
    // unauthenticated run holds no bearer, spawns nothing here, and reaches cloud not
    // at all.
    // A BEAT, not a single register: cloud reads liveness from when a machine last
    // wrote, so announcing once and going quiet reads offline ninety seconds into a
    // session that is still running. The guard beats until this run returns.
    let _machine = links_target(do_link, bearer.is_some())
        .then(|| target::beat(host.clone(), &root, &opts.brand, &api, &snapshot.machine_id, &snapshot.host));

    // Claude theme (Dracula dark / Alucard light, auto by the user's preference).
    // Native — writes `<config home>/themes` + selects it; never patches Claude. The
    // guard restores the prior theme when this session ends (any exit path). `dev`
    // has no Claude themes. Held to end-of-run so plain `claude` keeps its theme.
    // Takes the ROUTE because that is what decides which home this session reads.
    let _theme_guard = (kind == BackendKind::Claude)
        .then(|| theme::apply(&root, &routing, opts.theme.as_deref(), &profile.theme));

    banner(
        &opts,
        backend.label(),
        &cwd,
        &api,
        routing.via(),
        bearer.is_some(),
        session_id.as_deref(),
    );

    let structured = client.is_some() && session_id.is_some();
    let spent = summary::Run::begin(&routing);
    let spec = Spec {
        mode,
        task: opts.task.clone(),
        cwd: cwd.clone(),
        routing,
        effort,
        approval,
        mcp,
        remote: equipped.and_then(|e| e.remote),
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
        refresh: host.refresh(&opts.brand),
        root: root.clone(),
    };
    // The route also decides which config home the backend wrote its transcript in,
    // so keep it for the pointer recorded after the child exits — the supervisor
    // consumes `spec`. One value, read twice; never re-derived.
    let route = spec.routing.clone();
    let launch = match backend.build(&spec) {
        Ok(launch) => launch,
        Err(e) => {
            if let (Some(c), Some(id)) = (&client, &session_id) {
                let why = cut(&format!("{e:#}"), REASON_MAX);
                finalize(c, id, &Outcome::default(), Status::Error, &snapshot, &api, &who, None, &root, Some(&why)).await;
            }
            return Err(e);
        }
    };

    // The session id we watch for the interactive transcript tail.
    let watch_sid = resume_handle.clone().or(preset_session);

    match mode {
        Mode::Headless => {
            let run =
                supervise(&*backend, spec, launch, structured, client.clone(), session_id.clone())
                    .await;
            // A run that could not COMPLETE still ended. Propagating the error
            // straight out — which is what `?` here used to do — leaves the row
            // this run registered claiming to be running for good, and the
            // registry has no heartbeat to contradict it.
            let (outcome, status) = match &run {
                Ok((o, s)) => (o.clone(), *s),
                Err(_) => (Outcome::default(), Status::Error),
            };
            let reason = (status == Status::Error).then(|| reason_of(&run, backend.label()));
            if let (Some(c), Some(id)) = (&client, &session_id) {
                let transcript = outcome
                    .backend_session
                    .as_ref()
                    .and_then(|bs| backend.transcript_path(&root, &route, &cwd, bs))
                    .map(|p| p.display().to_string());
                finalize(c, id, &outcome, status, &snapshot, &api, &who, transcript, &root, reason.as_deref()).await;
            }
            spent.end(&*host, &opts.brand, backend.label(), kind == BackendKind::Claude, &outcome.usage, session_id.as_deref(), tools.as_deref().map_err(String::as_str)).await;
            if let Some(id) = &session_id {
                report_link(id);
            }
            run?;
            // A task run is scripted, so its exit status is the answer: an agent
            // that failed fails `hanzo`, and a caller's `&&` or CI step sees it.
            if status == Status::Error {
                let why = reason.unwrap_or_else(|| {
                    format!("{} did not finish the task: it exited with an error", backend.label())
                });
                bail!("{why}");
            }
        }
        Mode::Interactive => {
            let run = run_interactive(&*backend, launch, client.clone(), session_id.clone(), &root, &route, &cwd, watch_sid).await;
            if let (Some(c), Some(id)) = (&client, &session_id) {
                // Interactive per-event stream arrives via the transcript tail;
                // the resume handle here is what we pre-set / resumed.
                let bs = resume_handle.or_else(|| preset_session_of(&spec));
                let outcome = Outcome { backend_session: bs, ..Default::default() };
                let transcript = outcome
                    .backend_session
                    .as_ref()
                    .and_then(|s| backend.transcript_path(&root, &route, &cwd, s))
                    .map(|p| p.display().to_string());
                let status = interactive_end(&run, outcome.backend_session.is_some());
                let reason = match (&run, status) {
                    (Err(e), Status::Error) => Some(cut(&format!("{e:#}"), REASON_MAX)),
                    (_, Status::Error) => Some(format!("{} ended without starting", backend.label())),
                    _ => None,
                };
                finalize(c, id, &outcome, status, &snapshot, &api, &who, transcript, &root, reason.as_deref()).await;
            }
            // The tool's own screen was the stream, so nothing was reported here.
            spent.end(&*host, &opts.brand, backend.label(), kind == BackendKind::Claude, &Usage::default(), session_id.as_deref(), tools.as_deref().map_err(String::as_str)).await;
            if let Some(id) = &session_id {
                report_link(id);
            }
            run?;
        }
    }
    Ok(())
}


/// How an interactive session ends, from how its TUI exited and whether there is
/// a backend session to resume it by.
///
/// A clean exit SUSPENDS a session only when it can be reopened: its transcript
/// and resume handle outlive the TUI, and `--resume` finds them in the local
/// record `finalize` writes. With no handle there is no record, so nothing can
/// ever reopen it — a fresh `dev` TUI that quits, or exits to run its own
/// update — and the session is DONE. Calling it paused told the cloud something
/// was coming back, and the cloud billed it as alive (hanzo-inc/cloud#768).
///
/// A crash, or a TUI that never got off the ground, is an error either way.
fn interactive_end(run: &Result<bool>, resumable: bool) -> Status {
    match run {
        Ok(true) if resumable => Status::Paused,
        Ok(true) => Status::Done,
        _ => Status::Error,
    }
}

fn preset_session_of(spec: &Spec) -> Option<String> {
    spec.preset_session.clone()
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
#[path = "lib_tests.rs"]
mod tests;
