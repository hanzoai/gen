//! Canonical in-process `hanzo-gen` harness driver for `hanzod`, Hanzo Desktop,
//! and Mobile hosts.
//!
//! Embeds the exact same `hanzo-gen` pipeline (`resolve_or_install_mcp_direct`,
//! `kit::equip_with` + `hanzoskills.com`, `resolve_routing` across Hanzo Cloud
//! and local OpenAI-compatible inference engines, `backend.build(&spec)`,
//! structured JSONL stream capture, long-horizon `supervise` recovery, and
//! watch-channel interruption) directly in Rust without shelling out to `hanzo`
//! CLI for [`Host`].

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use crate::backend::{
    self, resolve, resolve_or_install_mcp_direct, Approval, BackendKind, Mode, Route, Routing, Spec,
};
use crate::context::Snapshot;
use crate::event::{Kind, Status, Usage};
use crate::host::{self, Account, Host, Pending, Profile, Provider};
use crate::root::Root;
use crate::routing::{gateway_models, resolve_routing};
use crate::session::SessionClient;
use crate::settings::Settings;
use crate::stream::{cut, reason_of};
use crate::supervise::{supervise_opts, SuperviseEnd, SuperviseOpts};
use crate::{home, kit};

/// Dynamic IAM bearer callback invoked on every embedded turn so rotated tokens
/// from `hanzo-identity` / `hanzod` take effect immediately.
pub type TokenFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// In-process [`Host`] implementation for daemons (`hanzod`), Desktop, and Mobile
/// runtimes that manage IAM tokens and local inference endpoints directly in Rust.
#[derive(Clone)]
pub struct StaticHost {
    pub api: String,
    pub local: bool,
    pub org: Option<String>,
    pub owner: String,
    pub name: String,
    pub token_fn: TokenFn,
    pub refresh_cmd: Vec<String>,
    pub hanzo_key: Option<String>,
}

impl std::fmt::Debug for StaticHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticHost")
            .field("api", &self.api)
            .field("local", &self.local)
            .field("org", &self.org)
            .field("owner", &self.owner)
            .field("name", &self.name)
            .field("refresh_cmd", &self.refresh_cmd)
            .field("hanzo_key", &self.hanzo_key.as_ref().map(|_| "***"))
            .finish()
    }
}

impl StaticHost {
    /// Construct a Cloud-backed [`StaticHost`] reading fresh IAM tokens from `token_fn`.
    pub fn cloud(
        api: impl Into<String>,
        owner: impl Into<String>,
        name: impl Into<String>,
        token_fn: TokenFn,
    ) -> Self {
        Self {
            api: api.into(),
            local: false,
            org: None,
            owner: owner.into(),
            name: name.into(),
            token_fn,
            refresh_cmd: Vec::new(),
            hanzo_key: None,
        }
    }

    /// Construct a local OpenAI-compatible engine [`StaticHost`] (e.g. `hanzod`
    /// engine at `"http://127.0.0.1:36900"`).
    pub fn local(api: impl Into<String>) -> Self {
        Self {
            api: api.into(),
            local: true,
            org: None,
            owner: "local".into(),
            name: "user".into(),
            token_fn: Arc::new(|| None),
            refresh_cmd: Vec::new(),
            hanzo_key: None,
        }
    }

    pub fn with_org(mut self, org: Option<String>) -> Self {
        self.org = org;
        self
    }

    pub fn with_refresh_cmd(mut self, refresh_cmd: Vec<String>) -> Self {
        self.refresh_cmd = refresh_cmd;
        self
    }

    pub fn with_hanzo_key(mut self, hanzo_key: Option<String>) -> Self {
        self.hanzo_key = hanzo_key;
        self
    }
}

impl Host for StaticHost {
    fn profile(&self) -> Pending<'_, Profile> {
        let identities = if !self.owner.trim().is_empty() && !self.name.trim().is_empty() {
            vec![(self.owner.clone(), self.name.clone())]
        } else {
            Vec::new()
        };
        let profile = Profile {
            api: self.api.clone(),
            local: self.local,
            org: self.org.clone(),
            provider: None,
            identities,
            link: false,
            theme: "auto".into(),
        };
        Box::pin(async move { profile })
    }

    fn account<'a>(&'a self, _brand: &'a str) -> Pending<'a, Result<Option<Account>>> {
        let tok = (self.token_fn)()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let owner = if self.owner.trim().is_empty() {
            "hanzo".to_string()
        } else {
            self.owner.clone()
        };
        let name = if self.name.trim().is_empty() {
            "agent".to_string()
        } else {
            self.name.clone()
        };
        Box::pin(async move {
            Ok(tok.map(|token| Account {
                owner,
                name,
                token,
            }))
        })
    }

    fn refresh(&self, _brand: &str) -> Vec<String> {
        self.refresh_cmd.clone()
    }

    fn key(&self, provider: Provider) -> Result<Option<String>> {
        Ok(match provider {
            Provider::Hanzo => self
                .hanzo_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
            Provider::Anthropic | Provider::OpenAI => None,
        })
    }

    fn meter<'a>(
        &'a self,
        _brand: &'a str,
        _query: &'a [String],
    ) -> Pending<'a, std::result::Result<Value, String>> {
        Box::pin(async { Err("embedded host does not query CLI usage meter".into()) })
    }
}

/// Request parameters for a single supervised in-process `hanzo-gen` turn.
pub struct EmbeddedTurnRequest {
    pub root: Root,
    pub cwd: PathBuf,
    pub prompt: String,
    pub backend: Option<BackendKind>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub instructions: Option<String>,
    /// Resumes the exact prior `dev` / `claude` / `codex` thread across multi-turn
    /// `/v1/dev/sessions/:id/turns` or `/steer`.
    pub resume_backend_session: Option<String>,
    /// Optional existing Cloud `/v1/agents/sessions` ID to mirror live events into.
    pub link_session_id: Option<String>,
    /// Attach native `hanzo-mcp` + `hanzoskills.com` skills (default `true`).
    pub mcp: bool,
    /// Optional cancellation watch receiver allowing `/interrupt` or `/steer` to
    /// cleanly stop the running turn while preserving its `backend_session_id`.
    pub cancel: Option<tokio::sync::watch::Receiver<bool>>,
}

impl EmbeddedTurnRequest {
    /// Construct a request defaulting to `BackendKind::Dev` and `mcp: true`.
    pub fn new(root: Root, cwd: impl Into<PathBuf>, prompt: impl Into<String>) -> Self {
        Self {
            root,
            cwd: cwd.into(),
            prompt: prompt.into(),
            backend: None,
            model: None,
            effort: None,
            instructions: None,
            resume_backend_session: None,
            link_session_id: None,
            mcp: true,
            cancel: None,
        }
    }
}

/// One structured action observed on the `hanzo-gen` stream during an embedded turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedAction {
    /// Action category: `"tool_call"`, `"spawn"`, `"message"`, or `"log"`.
    pub kind: String,
    pub name: String,
    pub summary: String,
    pub failed: bool,
}

/// Structured result of an in-process `hanzo-gen` turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedTurnResult {
    pub ok: bool,
    /// Terminal disposition: `"done"`, `"paused"`, `"error"`, or `"interrupted"`.
    pub status: String,
    pub backend: String,
    pub backend_session_id: Option<String>,
    pub model: String,
    pub text: Option<String>,
    pub messages: Vec<String>,
    pub actions: Vec<EmbeddedAction>,
    pub tool_calls: u64,
    pub usage: Usage,
    pub error: Option<String>,
}

const ACTION_SUMMARY_MAX: usize = 240;

fn summarize_value(v: &Value) -> String {
    let raw = match v {
        Value::Null => String::new(),
        Value::String(s) => s.lines().next().unwrap_or("").to_string(),
        Value::Object(map) => {
            if let Some(cmd) = map.get("command").and_then(Value::as_str) {
                cmd.lines().next().unwrap_or("").to_string()
            } else if let Some(path) = map.get("path").and_then(Value::as_str) {
                path.to_string()
            } else if let Some(q) = map.get("query").and_then(Value::as_str) {
                q.to_string()
            } else {
                v.to_string()
            }
        }
        other => other.to_string(),
    };
    cut(raw.trim(), ACTION_SUMMARY_MAX)
}

/// Convert a normalized [`crate::event::Mapped::Event`] into an [`EmbeddedAction`]
/// for tool calls, subagent spawns, and actionable error/todo logs.
pub(crate) fn action_from_event(kind: Kind, payload: &Value) -> Option<EmbeddedAction> {
    match kind {
        Kind::ToolCall => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let summary = payload.get("input").map(summarize_value).unwrap_or_default();
            let failed = payload
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_some_and(|c| c != 0)
                || payload.get("status").and_then(Value::as_str) == Some("failed")
                || payload.get("error").is_some_and(|e| !e.is_null())
                || payload.get("isError").and_then(Value::as_bool) == Some(true);
            Some(EmbeddedAction {
                kind: "tool_call".to_string(),
                name,
                summary,
                failed,
            })
        }
        Kind::Spawn => {
            let name = payload
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or("agent")
                .to_string();
            let summary = payload
                .pointer("/input/prompt")
                .or_else(|| payload.get("input"))
                .map(summarize_value)
                .unwrap_or_default();
            Some(EmbeddedAction {
                kind: "spawn".to_string(),
                name,
                summary,
                failed: false,
            })
        }
        Kind::Log => {
            let tag = payload.get("type").and_then(Value::as_str).unwrap_or("");
            let is_err = tag == "error" || payload.get("isError").and_then(Value::as_bool) == Some(true);
            if !is_err && tag != "todo" {
                return None;
            }
            let summary = payload
                .get("text")
                .or_else(|| payload.get("output"))
                .map(summarize_value)
                .unwrap_or_default();
            Some(EmbeddedAction {
                kind: "log".to_string(),
                name: if tag.is_empty() { "log".to_string() } else { tag.to_string() },
                summary,
                failed: is_err,
            })
        }
        Kind::Message | Kind::Status | Kind::Control => None,
    }
}

fn is_loopback_http(api: &str) -> bool {
    let trimmed = api.trim();
    trimmed.starts_with("http://127.0.0.1")
        || trimmed.starts_with("http://localhost")
        || trimmed.starts_with("http://[::1]")
}

/// Execute one supervised `hanzo-gen` turn in-process using the full canonical
/// pipeline (MCP attachment, `hanzoskills.com` + org kit, Cloud/local gateway
/// routing, structured stream capture, long-horizon supervisor recovery, and
/// watch-channel cancellation).
pub async fn run_embedded_turn(
    host: Arc<dyn Host>,
    req: EmbeddedTurnRequest,
) -> Result<EmbeddedTurnResult> {
    let kind = req.backend.unwrap_or(backend::DEFAULT);
    let backend = resolve(kind);
    let profile = host.profile().await;
    let api = profile.api.clone();
    let local = profile.local;

    let identity = host.account(host::BRAND).await?;
    let bearer = identity.as_ref().map(|a| a.token.clone());
    let who = identity.as_ref().map(Account::to_string).unwrap_or_default();
    let settings = Settings::load(&req.root);

    let mcp_on = req.mcp && settings.mcp.unwrap_or(true);
    let mcp = if mcp_on {
        resolve_or_install_mcp_direct(&req.root, &req.cwd).await
    } else {
        None
    };

    let model_opt = req.model.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let active_org = profile
        .org
        .as_deref()
        .or_else(|| identity.as_ref().map(|a| a.owner.as_str()));

    let mut routing = resolve_routing(
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

    // Support unauthenticated local OpenAI-compatible inference engines (e.g. `hanzod`
    // engine on `http://127.0.0.1:36900`) when `local` is true and no cloud key is set.
    if matches!(routing, Route::Inherit) && (local || is_loopback_http(&api)) && !kind.own_account_only() {
        let models = gateway_models(kind, model_opt, &settings);
        routing = Route::Via(Routing::Gateway {
            api: api.clone(),
            token: "local".to_string(),
            org: active_org.map(String::from),
            model: models.model,
            small_fast_model: models.small_fast_model,
            context_window: models.context_window,
        });
    }

    let model_id = match routing.via() {
        Some(Routing::Gateway { model, .. }) => model.clone(),
        _ => model_opt.unwrap_or("default").to_string(),
    };

    if kind == BackendKind::Claude {
        home::prepare(&req.root, &routing);
    }

    let link_id = req
        .link_session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let client = match (&link_id, bearer.as_deref()) {
        (Some(_), Some(token)) => Some(SessionClient::new(&api, token)?.with_org(profile.org.as_deref())),
        _ => None,
    };
    if let (Some(c), Some(sid)) = (&client, link_id.as_deref()) {
        let snapshot = Snapshot::capture(&req.root, &req.cwd, backend.label(), backend.version(&req.root));
        let _ = c.event(sid, Kind::Status, snapshot.context_payload(None)).await;
    }

    let equipped = match kit::root(&req.root, kind, &routing) {
        Some(skills) => {
            let cache = kit::cache(&req.root);
            let hs_cache = kit::hanzoskills_cache(&req.root);
            let hs_api = kit::hanzoskills_api();
            let auto = hs_api.as_deref().map(|skills_api| kit::AutoSkills {
                cache: &hs_cache,
                api: skills_api,
                cwd: &req.cwd,
                task: Some(req.prompt.as_str()),
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

    let task = match req.instructions.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => format!("{extra}\n\n---\n\n{}", req.prompt),
        None => req.prompt,
    };

    let mut passthrough = Vec::new();
    if matches!(kind, BackendKind::Dev | BackendKind::Codex | BackendKind::Interpreter) {
        if local {
            passthrough.push("-c".to_string());
            passthrough.push("model_providers.hanzocode.wire_api=\"chat\"".to_string());
        }
        if let Some(eff) = req
            .effort
            .as_deref()
            .map(str::trim)
            .filter(|s| matches!(*s, "low" | "medium" | "high"))
        {
            passthrough.push("-c".to_string());
            passthrough.push(format!("model_reasoning_effort=\"{eff}\""));
        }
    }

    let resume = req
        .resume_backend_session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let spec = Spec {
        mode: Mode::Headless,
        task: Some(task),
        cwd: req.cwd,
        routing,
        approval: Approval::Auto,
        mcp,
        remote: equipped.and_then(|e| e.remote),
        structured: true,
        preset_session: None,
        trust_project: false,
        resume,
        passthrough,
        refresh: host.refresh(host::BRAND),
        root: req.root,
    };

    let mirror_sid = client.as_ref().and(link_id);
    let launch = match backend.build(&spec) {
        Ok(launch) => launch,
        Err(e) => {
            if let (Some(c), Some(sid)) = (&client, mirror_sid.as_deref()) {
                let _ = c
                    .record_error(sid, &crate::stream::cut(&format!("{e:#}"), crate::stream::REASON_MAX))
                    .await;
            }
            return Err(e);
        }
    };
    let (outcome, end) = match supervise_opts(
        &*backend,
        spec,
        launch,
        true,
        client.clone(),
        mirror_sid.clone(),
        SuperviseOpts {
            render: false,
            null_stdio: true,
            cancel: req.cancel,
        },
    )
    .await
    {
        Ok(res) => res,
        Err(e) => {
            if let (Some(c), Some(sid)) = (&client, mirror_sid.as_deref()) {
                let _ = c
                    .record_error(sid, &crate::stream::cut(&format!("{e:#}"), crate::stream::REASON_MAX))
                    .await;
            }
            return Err(e);
        }
    };

    if let (Some(c), Some(sid)) = (&client, mirror_sid.as_deref()) {
        if !outcome.usage.is_empty() {
            let mut p = serde_json::to_value(&outcome.usage).unwrap_or_else(|_| json!({}));
            p["type"] = json!("usage");
            let _ = c.event(sid, Kind::Log, p).await;
        }
    }

    let (ok, status_str, error) = match end {
        SuperviseEnd::Interrupted => (false, "interrupted".to_string(), None),
        SuperviseEnd::Status(Status::Done) => (true, "done".to_string(), None),
        SuperviseEnd::Status(Status::Paused) => (true, "paused".to_string(), None),
        SuperviseEnd::Status(Status::Running) => (true, "running".to_string(), None),
        SuperviseEnd::Status(Status::Error) => {
            let why = reason_of(&Ok((outcome.clone(), Status::Error)), backend.label());
            if let (Some(c), Some(sid)) = (&client, mirror_sid.as_deref()) {
                let _ = c.record_error(sid, &why).await;
            }
            (false, "error".to_string(), Some(why))
        }
    };

    let text = outcome
        .messages
        .last()
        .cloned()
        .or_else(|| ok.then(|| outcome.final_summary.clone()).flatten());

    Ok(EmbeddedTurnResult {
        ok,
        status: status_str,
        backend: backend.label().to_string(),
        backend_session_id: outcome.backend_session,
        model: model_id,
        text,
        messages: outcome.messages,
        actions: outcome.actions,
        tool_calls: outcome.tool_calls,
        usage: outcome.usage,
        error,
    })
}

#[cfg(test)]
mod tests;
