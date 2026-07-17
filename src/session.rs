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
    pub async fn register(&self, agent: &str, title: &str) -> Result<Registered> {
        let body = json!({ "agent": agent, "title": title, "status": Status::Running.as_str() });
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
    pub async fn set_status(&self, id: &str, status: Status) -> Result<()> {
        let body = json!({ "status": status.as_str() });
        self.send(reqwest::Method::PATCH, &format!("/v1/agents/sessions/{id}"), Some(&body))
            .await?;
        Ok(())
    }

    /// Fetch a session's current server-side status (for resume decisions).
    pub async fn get(&self, id: &str) -> Result<Info> {
        let v = self.send(reqwest::Method::GET, &format!("/v1/agents/sessions/{id}"), None).await?;
        serde_json::from_value(v).context("parsing session info")
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

        let reg = client.register("claude", "fix the bug").await.unwrap();
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

    #[tokio::test]
    async fn non_2xx_is_an_error_not_a_silent_success() {
        let mock = MockCloud::start_status(403).await;
        let client = SessionClient::new(&mock.base_url(), "T").unwrap();
        let err = client.register("claude", "t").await.unwrap_err();
        assert!(err.to_string().contains("403"), "got: {err}");
    }
}
