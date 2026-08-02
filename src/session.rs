//! The cloud session control-plane client: `/v1/agents/sessions`.
//!
//! One concern — talk to cloud's live agent-session registry over HTTPS with the
//! CLI's hanzo.id bearer. The session is org-scoped SERVER-SIDE: the gateway
//! validates the JWT and injects the `owner` claim as the org, so this client
//! never sends (and cannot forge) an org — cross-tenant attribution is refused
//! at the gateway, not trusted from here. See `cloud/clients/agents/sessions.go`.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

use super::event::{Kind, Status};

#[derive(Clone)]
pub struct SessionClient {
    http: Client,
    api: String, // base origin, no trailing slash (e.g. https://api.hanzo.ai)
    token: String,
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

impl SessionClient {
    pub fn new(api: &str, token: &str) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building session http client")?;
        Ok(Self {
            http,
            api: api.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
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
        let v = self.send(reqwest::Method::POST, "/v1/agents/sessions", Some(&body)).await?;
        let id = v
            .get("id")
            .and_then(Value::as_str)
            .context("register response missing id")?
            .to_string();
        Ok(Registered { id })
    }

    /// Append one event to a session's ordered log.
    pub async fn event(&self, id: &str, kind: Kind, payload: Value) -> Result<()> {
        let body = json!({ "kind": kind.as_str(), "payload": payload });
        self.send(reqwest::Method::POST, &format!("/v1/agents/sessions/{id}/events"), Some(&body))
            .await?;
        Ok(())
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
        let mut body = json!({ "status": status.as_str() });
        if status.is_terminal() {
            body["terminal"] = json!("");
        }
        self.send(reqwest::Method::PATCH, &format!("/v1/agents/sessions/{id}"), Some(&body))
            .await?;
        Ok(())
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
            reqwest::Method::PATCH,
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
            reqwest::Method::PATCH,
            &format!("/v1/agents/sessions/{id}"),
            Some(&body),
        )
        .await?;
        Ok(())
    }

    /// Fetch a session's current server-side status (for resume decisions).
    pub async fn get(&self, id: &str) -> Result<Info> {
        let v = self.send(reqwest::Method::GET, &format!("/v1/agents/sessions/{id}"), None).await?;
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
    pub async fn drain_control(&self, id: &str, after: i64) -> Result<super::control::Page> {
        let path = format!("/v1/agents/sessions/{id}/control?after={after}");
        let v = self.send(reqwest::Method::GET, &path, None).await?;
        serde_json::from_value(v).context("parsing control page")
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = format!("{}{}", self.api, path);
        crate::http::send_json(&self.http, method, &url, &self.token, body).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code::testmock::MockCloud;

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
        assert!(r.header("x-org-id").is_none(), "CLI must not send X-Org-Id");
        assert_eq!(r.json()["agent"], "claude");
        assert_eq!(r.json()["title"], "fix the bug");
        assert_eq!(r.json()["status"], "running");
        // actor is server-derived: the CLI must not attribute it.
        assert!(r.json().get("actor").is_none());
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
        }
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
