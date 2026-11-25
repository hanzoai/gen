//! Cloud session registration, finalization, and claimed routed-agent execution.

use anyhow::{anyhow, bail, Result};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::backend::{self, resolve, resolve_or_install_mcp, Approval, BackendKind, Mode, Routing, Spec};
use crate::context::{ResumeRecord, Snapshot};
use crate::event::{Kind, Status};
use crate::host::{Account, Host};
use crate::root::Root;
use crate::routing::resolve_routing;
use crate::session::SessionClient;
use crate::settings::Settings;
use crate::stream::{cut, reason_of, Outcome, REASON_MAX};
use crate::supervise::supervise;
use crate::{configured_agent, contract, home, kit, summary};

/// Run a claimed routed task headlessly into an ALREADY-OPENED cloud `session_id`,
/// streaming live events and usage WITHOUT setting a terminal session status
/// (cloud's `routedFinalizer` closes the session after `/report` arrives).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_routed_agent(
    host: Arc<dyn Host>,
    root: &Root,
    brand: &str,
    cwd: &Path,
    session_id: &str,
    prompt: &str,
    model: Option<&str>,
    effort: Option<&str>,
    instructions: Option<&str>,
) -> Result<Option<contract::AgentRun>> {
    let started = std::time::Instant::now();
    let kind = match configured_agent(root) {
        Some(agent) => BackendKind::parse(&agent).unwrap_or(backend::DEFAULT),
        None => backend::DEFAULT,
    };
    let backend = resolve(kind);
    let profile = host.profile().await;
    let api = profile.api.clone();
    let local = profile.local;

    let identity = host.account(brand).await?;
    let bearer = identity.as_ref().map(|a| a.token.clone());
    let who = identity.as_ref().map(Account::to_string).unwrap_or_default();
    let settings = Settings::load(root);

    let mcp_on = settings.mcp.unwrap_or(true);
    let mcp = if mcp_on { resolve_or_install_mcp(root, cwd, None).await } else { None };
    let model_opt = model.map(str::trim).filter(|s| !s.is_empty());
    let active_org = profile
        .org
        .as_deref()
        .or_else(|| identity.as_ref().map(|a| a.owner.as_str()));
    let routing = resolve_routing(
        &*host,
        profile.provider.as_deref(),
        &settings,
        true,
        kind,
        &api,
        bearer.as_deref(),
        active_org,
        model_opt,
    )?;

    let model_id = match routing.via() {
        Some(Routing::Gateway { model, .. }) => model.clone(),
        _ => model_opt.unwrap_or("default").to_string(),
    };
    let model_locked = model_opt.is_some();

    if kind == BackendKind::Claude {
        home::prepare(root, &routing);
    }

    let agent_version = backend.version(root);
    let snapshot = Snapshot::capture(root, cwd, backend.label(), agent_version.clone());
    let client = match bearer.as_deref() {
        Some(token) => Some(SessionClient::new(&api, token)?.with_org(profile.org.as_deref())),
        None => None,
    };
    if let Some(c) = &client {
        let _ = c.event(session_id, Kind::Status, snapshot.context_payload(None)).await;
    }

    let equipped = match kit::root(root, kind, &routing) {
        Some(skills) => {
            let cache = kit::cache(root);
            let hs_cache = kit::hanzoskills_cache(root);
            let hs_api = kit::hanzoskills_api();
            let auto = hs_api.as_deref().map(|skills_api| kit::AutoSkills {
                cache: &hs_cache,
                api: skills_api,
                cwd,
                task: Some(prompt),
            });
            let caller = identity.as_ref().zip(bearer.as_deref()).map(|(_, token)| kit::Caller {
                api: &api,
                who: &who,
                token,
                local,
            });
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
    };

    let task = match instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => format!("{extra}\n\n---\n\n{prompt}"),
        None => prompt.to_string(),
    };
    let mut passthrough = Vec::new();
    if kind == BackendKind::Dev {
        if let Some(eff) = effort.map(str::trim).filter(|s| matches!(*s, "low" | "medium" | "high")) {
            passthrough.push("-c".to_string());
            passthrough.push(format!("model_reasoning_effort=\"{eff}\""));
        }
    }

    let structured = client.is_some();
    let spec = Spec {
        mode: Mode::Headless,
        task: Some(task),
        cwd: cwd.to_path_buf(),
        routing,
        approval: Approval::Auto,
        mcp,
        remote: equipped.and_then(|e| e.remote),
        structured,
        preset_session: None,
        trust_project: false,
        resume: None,
        passthrough,
        refresh: host.refresh(brand),
        root: root.clone(),
    };
    let launch = match backend.build(&spec) {
        Ok(launch) => launch,
        Err(e) => {
            if let Some(c) = &client {
                let _ = c.record_error(session_id, &cut(&format!("{e:#}"), REASON_MAX)).await;
            }
            return Err(e);
        }
    };
    let (outcome, status) = match supervise(
        &*backend,
        spec,
        launch,
        structured,
        client.clone(),
        client.as_ref().map(|_| session_id.to_string()),
    )
    .await
    {
        Ok(res) => res,
        Err(e) => {
            if let Some(c) = &client {
                let _ = c.record_error(session_id, &cut(&format!("{e:#}"), REASON_MAX)).await;
            }
            return Err(e);
        }
    };

    if let Some(c) = &client {
        if !outcome.usage.is_empty() {
            let mut p = serde_json::to_value(&outcome.usage).unwrap_or_else(|_| json!({}));
            p["type"] = json!("usage");
            let _ = c.event(session_id, Kind::Log, p).await;
        }
    }

    if status == Status::Error {
        let why = reason_of(&Ok((outcome.clone(), status)), backend.label());
        if let Some(c) = &client {
            let _ = c.record_error(session_id, &why).await;
        }
        bail!("{why}");
    }

    let elapsed_ms = outcome
        .usage
        .duration_ms
        .unwrap_or_else(|| started.elapsed().as_millis().min(u64::MAX as u128) as u64);
    let mut telemetry = contract::AgentRun::new(
        session_id,
        session_id,
        agent_version.unwrap_or_else(|| backend.label().to_string()),
        model_id,
        model_locked,
        summary::url(session_id),
    );
    telemetry.execution = contract::ExecutionTelemetry {
        mode: "managed".to_string(),
        steps: outcome.usage.num_turns.unwrap_or(0),
        tool_calls: outcome.tool_calls,
        elapsed_ms,
        exit_reason: status.as_str().to_string(),
    };
    telemetry.usage = contract::UsageTelemetry::from_event_usage(&outcome.usage);
    telemetry.verification = contract::VerificationTelemetry {
        checks_executed: Vec::new(),
        checks_passed: !outcome.saw_error && status != Status::Error,
        critic_verdict: None,
        grader_result: None,
    };

    Ok(Some(telemetry))
}

/// `resolve_cloud_session`, retried with doubling waits until `within` has passed: a
/// linked run starts only with its session, so a cloud that is briefly not ready delays
/// the run and one that stays away fails it, saying why and how to run without a link.
pub(crate) async fn register_session(
    client: &SessionClient,
    snapshot: &Snapshot,
    title: &str,
    resume_from: Option<&str>,
    within: Duration,
    first_wait: Duration,
) -> Result<(String, Option<String>)> {
    let mut last = String::new();
    let tried = tokio::time::timeout(within, async {
        let mut wait = first_wait;
        loop {
            match resolve_cloud_session(client, snapshot, title, resume_from).await {
                Ok(registered) => return registered,
                Err(e) => last = format!("{e:#}"),
            }
            tokio::time::sleep(wait).await;
            wait *= 2;
        }
    })
    .await;
    tried.map_err(|_| {
        anyhow!(
            "the cloud did not register the session in {}s ({last}); try again, or pass --no-link to work without a session",
            within.as_secs()
        )
    })
}

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
    snapshot: &Snapshot,
    title: &str,
    resume_from: Option<&str>,
) -> Result<(String, Option<String>)> {
    let register = || client.register(&snapshot.backend, title, &snapshot.host, &snapshot.cwd);
    if let Some(old) = resume_from {
        match client.get(old).await {
            Ok(info) if !info.is_terminal() => {
                let _ = client.set_status(old, Status::Running).await;
                return Ok((old.to_string(), None));
            }
            Ok(_) => {
                let reg = register().await?;
                return Ok((reg.id, Some(old.to_string())));
            }
            Err(e) => {
                crate::warn(&format!(
                    "could not verify session {old} ({e}); starting a fresh cloud session with no \
                     resume lineage."
                ));
                let reg = register().await?;
                return Ok((reg.id, None));
            }
        }
    }
    let reg = register().await?;
    Ok((reg.id, None))
}

/// Close out a linked session: record usage, persist + mirror the resume handle,
/// and set the final status.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finalize(
    client: &SessionClient,
    session_id: &str,
    outcome: &Outcome,
    status: Status,
    snapshot: &Snapshot,
    api: &str,
    identity: &str,
    transcript_path: Option<String>,
    root: &Root,
    reason: Option<&str>,
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
            created_at: chrono::Utc::now().timestamp(),
        };
        let _ = rec.save(root);
        let _ = client.event(session_id, Kind::Status, rec.resume_payload()).await;
    }

    if status == Status::Error {
        let why = reason
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} ended with an error", snapshot.backend));
        let _ = client.fail(session_id, &why).await;
    } else {
        let _ = client.set_ended(session_id, status, None).await;
    }
}
