//! The cloud session control-plane client: `/v1/agents/sessions`.
//!
//! One concern — talk to cloud's live agent-session registry over HTTPS with the
//! CLI's hanzo.id bearer. The session is org-scoped SERVER-SIDE: the gateway
//! validates the token and decides the org. This client adds only the `--org`
//! selection as `X-Org-Id`, which the gateway honors only within the token's
//! signed membership — cross-tenant attribution is refused at the gateway, not
//! trusted from here. See `cloud/clients/agents/sessions.go`.

use anyhow::{Context, Result};
use hanzo_client::{Http, Method, Request, Transport};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::event::{Kind, Status};

#[derive(Clone)]
pub struct SessionClient {
    wire: Http,
    api: String, // base origin, no trailing slash (e.g. https://api.hanzo.ai)
    token: String,
    /// The org selected with `--org`, sent as `X-Org-Id` (`Config::org`).
    org: Option<String>,
    /// Set while the last event append was lost: see [`Self::event`]. Shared by
    /// every clone, since they all forward to the one cloud.
    lost: Arc<AtomicBool>,
}

/// The result of registering a session — cloud mints the id.
#[derive(Debug, Clone)]
pub struct Registered {
    pub id: String,
}

/// A session's current server-side truth, enough to decide resume semantics.
#[derive(Debug, Clone, Deserialize)]
pub struct Info {
    pub status: String,
}

impl Info {
    /// Cloud forbids reopening a terminal session (`patchSession` is monotonic),
    /// so a resume must fork a new session off a terminal one instead of reusing.
    pub fn is_terminal(&self) -> bool {
        self.status == Status::Done.as_str() || self.status == Status::Error.as_str()
    }
}

/// Fallback reason attached if a caller moves a session to `error` without saying why.
const DEFAULT_ERROR_REASON: &str = "session ended in error without a reported reason";

impl SessionClient {
    pub fn new(api: &str, token: &str) -> Result<Self> {
        let wire = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building session http client")?;
        Ok(Self {
            wire: Http::new(wire),
            api: api.trim_end_matches('/').to_string(),
            token: token.to_string(),
            org: None,
            lost: Arc::default(),
        })
    }

    /// Act in `org` when one was selected (`--org`); `None` is the bearer's own.
    pub fn with_org(mut self, org: Option<&str>) -> Self {
        self.org = org.map(str::to_string);
        self
    }

    /// Register a new root session. `title` is truncated server-side; `actor` is
    /// derived server-side from the validated principal (we never send it).
    ///
    /// `host` and `cwd` are WHERE this session runs, and they are not optional
    /// extras: the console's roster is grouped by machine, so a row registered
    /// without a host is filed under "unknown machine" and stays there for the
    /// life of the session. Every session — a wrapped agent or a linked shell —
    /// knows the machine it is on, so every session says so. Callers get the pair
    /// from the same [`Snapshot`](super::context::Snapshot) they report as
    /// context, which is why there is no second register to forget it in.
    pub async fn register(
        &self,
        agent: &str,
        title: &str,
        host: &str,
        cwd: &str,
    ) -> Result<Registered> {
        let body = json!({
            "agent": agent, "title": title, "host": host, "cwd": cwd,
            "status": Status::Running.as_str(),
        });
        let v = self.send(Method::POST, "/v1/agents/sessions", Some(&body)).await?;
        let id = v
            .get("id")
            .and_then(Value::as_str)
            .context("register response missing id")?
            .to_string();
        Ok(Registered { id })
    }

    /// Append one event to a session's ordered log.
    ///
    /// The append names itself with a `requestId`, so the wire may make it again
    /// after a 5xx (`hanzo_client`'s one retry policy repeats a POST only when it is
    /// named) and the platform stores the turn once. Without the name a turn cloud
    /// refused for a moment (503, its audit trail busy) was lost from the transcript.
    ///
    /// The stream forwards each turn in line with reading the agent, so those tries
    /// hold the agent's output while they wait. A cloud that stays down is therefore
    /// waited on once: after a turn is lost the next go unnamed (one try) until one
    /// lands.
    pub async fn event(&self, id: &str, kind: Kind, payload: Value) -> Result<()> {
        let mut body = json!({ "kind": kind.as_str(), "payload": payload });
        if !self.lost.load(Ordering::Relaxed) {
            body["requestId"] = json!(format!("{:032x}", rand::random::<u128>()));
        }
        let sent = self
            .send(Method::POST, &format!("/v1/agents/sessions/{id}/events"), Some(&body))
            .await;
        self.lost.store(sent.is_err(), Ordering::Relaxed);
        sent.map(drop)
    }

    /// Set the session's status (running/paused/done/error). Cloud refuses to
    /// move a terminal session, so callers must not PATCH a done/error session.
    ///
    /// Reaching a TERMINAL status withdraws the published terminal URL in the
    /// SAME request. Ending and un-publishing are one moment, not two: a session
    /// that says `done` while still advertising a URL invites the console to
    /// frame a tunnel nobody is serving, and any caller that has to remember two
    /// calls is a caller that will one day make only the first. There is
    /// deliberately no way to spell "close it but keep the URL".
    pub async fn set_status(&self, id: &str, status: Status) -> Result<()> {
        self.set_ended(id, status, None).await
    }

    /// [`Self::set_status`] with the reason a session ended in error: how the
    /// backend's process ended, or why it could not start. The reason rides the
    /// same request as the status, so the row never reads "error" and nothing
    /// else; cloud keeps it only on a session in error, and bounds it.
    pub async fn set_ended(&self, id: &str, status: Status, reason: Option<&str>) -> Result<()> {
        let mut body = json!({ "status": status.as_str() });
        if status.is_terminal() {
            body["terminal"] = json!("");
        }
        if status == Status::Error {
            let why = reason
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_ERROR_REASON);
            body["error"] = json!(why);
        }
        self.send(Method::PATCH, &format!("/v1/agents/sessions/{id}"), Some(&body))
            .await?;
        Ok(())
    }

    /// Append a closing `status` event carrying the error reason so both the
    /// session's event timeline (`endedWhy`) and live stream record why it failed.
    pub async fn record_error(&self, id: &str, reason: &str) -> Result<()> {
        let why = match reason.trim() {
            "" => DEFAULT_ERROR_REASON,
            s => s,
        };
        self.event(
            id,
            Kind::Status,
            json!({
                "type": "ended",
                "status": Status::Error.as_str(),
                "error": why,
            }),
        )
        .await
    }

    /// Close a session in `error`, recording the reason on both a closing `status`
    /// event and the terminal `PATCH` update. The event is best-effort so a
    /// transient event failure never prevents closing the session row.
    pub async fn fail(&self, id: &str, reason: &str) -> Result<()> {
        let why = match reason.trim() {
            "" => DEFAULT_ERROR_REASON,
            s => s,
        };
        let _ = self.record_error(id, why).await;
        self.set_ended(id, Status::Error, Some(why)).await
    }

    /// Publish where this session's live terminal can be WATCHED. Cloud stores
    /// the address and never the connection, so the bytes keep flowing
    /// machine-to-viewer even though the roster lives there. Withdrawal is not a
    /// separate act — it happens when the session ends (see [`Self::set_status`]).
    ///
    /// The URL must be https — cloud refuses anything else, because the console
    /// frames this value and any other scheme is a way to get a javascript: or
    /// file: URL rendered on a signed-in page.
    pub async fn publish_terminal(&self, id: &str, url: &str) -> Result<()> {
        let body = json!({ "terminal": url });
        self.send(
            Method::PATCH,
            &format!("/v1/agents/sessions/{id}"),
            Some(&body),
        )
        .await?;
        Ok(())
    }

    /// Report where this session is working NOW.
    ///
    /// A linked shell is a place a person moves around in, not a directory a run
    /// starts and ends in. `cwd` used to be captured once at register, so the
    /// console kept naming the directory `hanzo link` happened to start in long
    /// after the shell had walked away — the field answered "which work is this"
    /// with an answer that was true once.
    pub async fn set_cwd(&self, id: &str, cwd: &str) -> Result<()> {
        let body = json!({ "cwd": cwd });
        self.send(
            Method::PATCH,
            &format!("/v1/agents/sessions/{id}"),
            Some(&body),
        )
        .await?;
        Ok(())
    }

    /// Fetch a session's current server-side status (for resume decisions).
    pub async fn get(&self, id: &str) -> Result<Info> {
        let v = self.send(Method::GET, &format!("/v1/agents/sessions/{id}"), None).await?;
        serde_json::from_value(v).context("parsing session info")
    }

    /// Drain the steering commands issued since `after` — the INBOUND half of the
    /// session channel (`cloud/apps/agents/sessions_control_drain.go`).
    ///
    /// Read-only and cursor-driven: cloud returns `control` events with
    /// `seq > after`, oldest first, plus the cursor to poll from next, so an
    /// applied command is never redelivered and a reconnect replays exactly what
    /// was missed. Org scoping is the SAME check that guards the session itself —
    /// a session belonging to another org is a clean 404 here, never a page of
    /// someone else's commands.
    pub(crate) async fn drain_control(&self, id: &str, after: i64) -> Result<super::control::Page> {
        let path = format!("/v1/agents/sessions/{id}/control?after={after}");
        let v = self.send(Method::GET, &path, None).await?;
        serde_json::from_value(v).context("parsing control page")
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let mut request =
            Request::new(method, format!("{}{}", self.api, path)).token(&self.token);
        if let Some(org) = &self.org {
            request = request.org(org);
        }
        if let Some(body) = body {
            request = request.body(body.clone());
        }
        Ok(self.wire.send(request).await?.ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testmock::MockCloud;

    #[tokio::test]
    async fn register_sends_bearer_and_agent_and_never_sends_org() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "TOK123").unwrap();

        let reg = client.register("claude", "fix the bug", "evo", "/w").await.unwrap();
        assert!(reg.id.starts_with("sess_"));

        let reqs = mock.requests();
        let r = &reqs[0];
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/v1/agents/sessions");
        // Bearer carries the credential; the org is derived server-side.
        assert_eq!(r.header("authorization").as_deref(), Some("Bearer TOK123"));
        assert!(r.header("x-org-id").is_none(), "with no --org selected, the CLI sends no X-Org-Id");
        assert_eq!(r.json()["agent"], "claude");
        assert_eq!(r.json()["title"], "fix the bug");
        assert_eq!(r.json()["status"], "running");
        // actor is server-derived: the CLI must not attribute it.
        assert!(r.json().get("actor").is_none());
    }

    #[tokio::test]
    async fn a_selected_org_files_the_session_there() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap().with_org(Some("acme"));
        client.register("tmux", "shell", "evo", "/w").await.unwrap();
        let reqs = mock.requests();
        let r = reqs.iter().find(|r| r.method == "POST").unwrap();
        assert_eq!(r.header("x-org-id").as_deref(), Some("acme"), "the session row left the selected org");
    }

    /// EVERY session says which machine it is on. A row registered without a host
    /// is filed under "unknown machine" and can never be grouped afterwards, so
    /// there is exactly one register and it takes the machine as an argument —
    /// no caller can reach a spelling that omits it.
    #[tokio::test]
    async fn every_register_carries_the_machine_and_the_directory() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();

        client.register("claude", "t", "evo", "/src/hanzo").await.unwrap();

        let r = &mock.requests()[0];
        assert_eq!(r.json()["host"], "evo");
        assert_eq!(r.json()["cwd"], "/src/hanzo");
    }

    #[tokio::test]
    async fn event_and_status_hit_the_right_routes() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        client
            .event("sess_1", Kind::ToolCall, json!({"name":"Bash"}))
            .await
            .unwrap();
        client.set_status("sess_1", Status::Done).await.unwrap();

        let reqs = mock.requests();
        assert_eq!(reqs[0].path, "/v1/agents/sessions/sess_1/events");
        assert_eq!(reqs[0].json()["kind"], "tool-call");
        assert_eq!(reqs[1].method, "PATCH");
        assert_eq!(reqs[1].path, "/v1/agents/sessions/sess_1");
        assert_eq!(reqs[1].json()["status"], "done");
    }

    /// A transcript turn the cloud refuses with a 503 (its audit trail could not
    /// record the request, so nothing was applied) is made again, not dropped: the
    /// append names itself with a `requestId`, which is what lets the wire repeat a
    /// POST, and the repeat carries the SAME id so the platform stores the turn once.
    ///
    /// MUTATION: drop `requestId` from the body, and the first assertion reads Err
    /// (the refused turn is lost, as it was when a stream said "event dropped").
    #[tokio::test]
    async fn a_refused_event_is_made_again_under_the_same_request_id() {
        let mock = MockCloud::start_refusing_first(1).await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();

        client
            .event("sess_1", Kind::ToolCall, json!({"name":"Bash"}))
            .await
            .expect("a 503 on a transcript turn is retried, not dropped");

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 2, "one refusal, one repeat");
        let named = |i: usize| reqs[i].json()["requestId"].as_str().unwrap_or_default().to_string();
        assert!(!named(0).is_empty(), "the append names itself");
        assert_eq!(named(0), named(1), "the repeat is the same request");
        assert_eq!(reqs[1].path, "/v1/agents/sessions/sess_1/events");
    }

    /// A cloud that stays down costs the run ONE turn's retries, not every turn's.
    /// The stream forwards each turn in line with reading the agent, so a named
    /// append that waits out the wire's backoff holds the agent's output for that
    /// long. Once a turn has used every try, the next ones go unnamed (one try, as
    /// before) until one lands; then turns are named again.
    ///
    /// MUTATION: always name the append, and the down cloud is asked 4 times for the
    /// second turn too (the count below reads 8, and the run waits on each turn).
    #[tokio::test]
    async fn a_cloud_that_stays_down_is_waited_on_once() {
        // Refuses 5: all 4 tries of the first turn, and the single try of the second.
        let mock = MockCloud::start_refusing_first(5).await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let turn = || client.event("sess_1", Kind::Log, json!({"line":"x"}));

        assert!(turn().await.is_err(), "every try refused");
        assert_eq!(mock.requests().len(), 4, "the named turn used every try");

        assert!(turn().await.is_err());
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 5, "a turn after a lost one is tried once");
        assert!(reqs[4].json().get("requestId").is_none(), "unnamed, so the wire does not repeat it");

        turn().await.expect("cloud is back");
        turn().await.unwrap();
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 7);
        assert!(reqs[6].json()["requestId"].is_string(), "a turn landed, so turns are named again");
    }

    /// A retried turn lands before the turn after it: the stream forwards in line, so
    /// the transcript keeps the agent's order across a refusal.
    ///
    /// MUTATION: forward the next turn while the first is still retrying, and B's
    /// request is recorded before A's repeat.
    #[tokio::test]
    async fn turns_keep_their_order_across_a_retry() {
        let mock = MockCloud::start_refusing_first(1).await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        client.event("sess_1", Kind::Log, json!({"line":"a"})).await.unwrap();
        client.event("sess_1", Kind::Log, json!({"line":"b"})).await.unwrap();

        let reqs = mock.requests();
        let lines: Vec<String> = reqs.iter().map(|r| r.json()["payload"]["line"].as_str().unwrap_or_default().to_string()).collect();
        assert_eq!(lines, ["a", "a", "b"], "refused a, a again, then b");
        assert_eq!(reqs[0].json()["requestId"], reqs[1].json()["requestId"], "a's repeat is a");
        assert_ne!(reqs[1].json()["requestId"], reqs[2].json()["requestId"], "b is its own request");
    }

    /// Two turns are two requests: the id names one append, never the session.
    #[tokio::test]
    async fn each_event_names_itself_apart() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        for _ in 0..2 {
            client.event("sess_1", Kind::Log, json!({"line":"x"})).await.unwrap();
        }
        let reqs = mock.requests();
        assert_ne!(reqs[0].json()["requestId"], reqs[1].json()["requestId"]);
    }

    /// Ending a session and withdrawing its terminal URL are ONE act, in ONE
    /// request. Two requests are two chances for the row to say it finished while
    /// still advertising a shell — or to stop advertising one while claiming to
    /// run — and there is no caller that wants either.
    #[tokio::test]
    async fn ending_a_session_withdraws_its_terminal_in_the_same_request() {
        for status in [Status::Done, Status::Error] {
            let mock = MockCloud::start().await;
            let client = SessionClient::new(&mock.base_url(), "T").unwrap();

            client.set_status("sess_1", status).await.unwrap();

            let reqs = mock.requests();
            assert_eq!(reqs.len(), 1, "the close is one request, not two");
            assert_eq!(reqs[0].json()["status"], status.as_str());
            assert_eq!(reqs[0].json()["terminal"], "", "an ended session is not watchable");
            if status == Status::Error {
                assert_eq!(reqs[0].json()["error"], DEFAULT_ERROR_REASON);
            } else {
                assert!(reqs[0].json().get("error").is_none());
            }
        }
    }

    #[tokio::test]
    async fn fail_records_status_event_and_error_patch_with_reason() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();

        client.fail("sess_err", "dev exited with status 101: panic").await.unwrap();

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 2, "fail emits closing status event then terminal PATCH");
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/v1/agents/sessions/sess_err/events");
        assert_eq!(reqs[0].json()["kind"], "status");
        assert_eq!(reqs[0].json()["payload"]["type"], "ended");
        assert_eq!(reqs[0].json()["payload"]["status"], "error");
        assert_eq!(reqs[0].json()["payload"]["error"], "dev exited with status 101: panic");
        assert_eq!(reqs[1].method, "PATCH");
        assert_eq!(reqs[1].path, "/v1/agents/sessions/sess_err");
        assert_eq!(reqs[1].json()["status"], "error");
        assert_eq!(reqs[1].json()["terminal"], "");
        assert_eq!(reqs[1].json()["error"], "dev exited with status 101: panic");
    }

    #[tokio::test]
    async fn blank_error_reason_still_populates_non_empty_fallback() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();

        client.set_ended("sess_blank", Status::Error, Some("   \n\t ")).await.unwrap();
        let reqs = mock.requests();
        assert_eq!(reqs[0].json()["status"], "error");
        assert_eq!(reqs[0].json()["error"], DEFAULT_ERROR_REASON);
    }

    /// The converse: a session that is merely moving between LIVE states keeps
    /// its terminal. A pause that silently unpublished the shell would blank the
    /// console's frame for a session still very much running in it.
    #[tokio::test]
    async fn a_live_status_leaves_the_terminal_alone() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();

        client.set_status("sess_1", Status::Running).await.unwrap();
        client.set_status("sess_1", Status::Paused).await.unwrap();

        for r in mock.requests() {
            assert!(r.json().get("terminal").is_none(), "sent: {}", r.body);
        }
    }

    #[tokio::test]
    async fn publishing_a_terminal_says_where_to_watch() {
        let mock = MockCloud::start().await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();

        client.publish_terminal("sess_1", "https://x.share.hanzo.app").await.unwrap();

        let r = &mock.requests()[0];
        assert_eq!(r.method, "PATCH");
        assert_eq!(r.path, "/v1/agents/sessions/sess_1");
        assert_eq!(r.json()["terminal"], "https://x.share.hanzo.app");
        // Publishing does not re-assert a status: the session is already running.
        assert!(r.json().get("status").is_none());
    }

    #[tokio::test]
    async fn non_2xx_is_an_error_not_a_silent_success() {
        let mock = MockCloud::start_status(403).await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let err = client.register("claude", "t", "evo", "/w").await.unwrap_err();
        assert!(err.to_string().contains("403"), "got: {err}");
    }
}
