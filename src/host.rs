//! What a session needs from the program it runs in.
//!
//! Gen launches agents; it signs nobody in and stores no credential. Hanzo IAM
//! owns the account, and the Hanzo CLI owns IAM's tokens. So the account, the
//! keys a user saved, and the cloud's usage meter all come from the host: the
//! `hanzo` CLI in its own process when Gen is its library, or the `hanzo`
//! command on PATH when Gen runs as `gen` ([`Command`]).

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;

use anyhow::Result;
use base64::Engine;
use serde_json::Value;

/// The brand a session signs in under unless `--brand` names another, and the
/// one a machine's run-target beats under.
pub const BRAND: &str = "hanzo";

/// A host's answer that takes a moment: a credential refresh, a cloud read.
pub type Pending<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The model provider a stored key authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// A Hanzo gateway key (`sk-` / `hk-`), for api.hanzo.ai.
    Hanzo,
    Anthropic,
    OpenAI,
}

/// The account a session speaks as: who IAM says it is, and its access token.
///
/// `owner` is the org the gateway bills. The token is never printed: `Display`
/// and `Debug` show `owner/name` alone.
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    pub owner: String,
    pub name: String,
    pub token: String,
}

impl std::fmt::Display for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Account({self})")
    }
}

/// What the host has on file for a session.
#[derive(Debug, Clone)]
pub struct Profile {
    /// The origin every cloud call goes to, e.g. `https://api.hanzo.ai`.
    pub api: String,
    /// The local cloud: the one origin that may be plain http.
    pub local: bool,
    /// The org selected for this invocation, sent as `X-Org-Id`.
    pub org: Option<String>,
    /// The model provider the user selected (`hanzo`, `anthropic`, `openai`).
    pub provider: Option<String>,
    /// Every Hanzo identity held, as `(owner, name)`.
    pub identities: Vec<(String, String)>,
    /// Stream a signed-in session to the cloud unless told not to.
    pub link: bool,
    /// The Claude theme a session applies when `--theme` names none.
    pub theme: String,
    /// The command that prints the signed-in account's access token, fresh: an
    /// agent that outlives one token (they last an hour) runs it for the next.
    pub refresh: Vec<String>,
}

/// The program Gen runs in.
pub trait Host: Send + Sync {
    /// What is on file: the cloud, the selections, the persisted defaults.
    fn profile(&self) -> Profile;

    /// The account signed in for `brand`, its access token fresh. `None` when
    /// nobody is signed in, never some other account.
    fn account<'a>(&'a self, brand: &'a str) -> Pending<'a, Result<Option<Account>>>;

    /// The key the user stored for `provider`, read only when a route needs it.
    fn key(&self, provider: Provider) -> Result<Option<String>>;

    /// `GET /v1/usage/summary` for `query` (`range=custom`, `start=…`, `end=…`),
    /// as its `data`; a refusal or a fault is the reason, never an empty document.
    fn meter<'a>(&'a self, query: &'a [String]) -> Pending<'a, std::result::Result<Value, String>>;
}

/// The host `gen` runs under: the `hanzo` command on PATH, which holds the Hanzo
/// IAM account, refreshes its token and reads the cloud for it. Gen asks it the
/// way Hanzo Dev does (`hanzo auth token`), so signing in to one signs in to both.
pub struct Command {
    program: String,
}

impl Default for Command {
    /// The `hanzo` PATH finds now, by its full path, so the agents a session
    /// starts ask the same binary for a fresh token whatever their PATH says.
    fn default() -> Self {
        let program = which::which("hanzo").map(|p| p.display().to_string()).unwrap_or_else(|_| "hanzo".into());
        Command { program }
    }
}

impl Command {
    /// Ask `program` instead of `hanzo`.
    pub fn new(program: impl Into<String>) -> Self {
        Command { program: program.into() }
    }

    /// Run the program and keep what it printed, or why it printed nothing.
    /// Both streams are captured, so a credential never reaches a terminal or a log.
    async fn ask(&self, args: &[String]) -> std::result::Result<String, String> {
        let out = tokio::process::Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => format!(
                    "`{}` is not installed (`curl -fsSL hanzo.sh | sh` installs it)",
                    self.program
                ),
                _ => format!("`{}` did not start: {e}", self.program),
            })?;
        if !out.status.success() {
            let said = String::from_utf8_lossy(&out.stderr);
            let said: String = said.split_whitespace().collect::<Vec<_>>().join(" ");
            return Err(said.chars().take(120).collect());
        }
        String::from_utf8(out.stdout).map_err(|_| format!("`{}` printed something that is not text", self.program))
    }
}

impl Host for Command {
    fn profile(&self) -> Profile {
        Profile {
            api: hanzo_client::API.into(),
            local: false,
            org: None,
            provider: None,
            identities: Vec::new(),
            link: true,
            theme: "auto".into(),
            refresh: vec![self.program.clone(), "auth".into(), "token".into()],
        }
    }

    fn account<'a>(&'a self, brand: &'a str) -> Pending<'a, Result<Option<Account>>> {
        Box::pin(async move {
            let mut args = vec!["auth".to_string(), "token".to_string()];
            if brand != BRAND {
                args.extend(["--brand".to_string(), brand.to_string()]);
            }
            let Ok(out) = self.ask(&args).await else {
                return Ok(None);
            };
            let token = out.trim();
            if token.is_empty() || token.len() > 16_384 || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Ok(None);
            }
            Ok(claims(token).map(|(owner, name)| Account { owner, name, token: token.to_string() }))
        })
    }

    fn key(&self, provider: Provider) -> Result<Option<String>> {
        // `HANZO_API_KEY` is the credential a host hands a run it starts, where
        // there is no CLI to ask (a cloud sandbox). The provider keys a person
        // pastes live in the CLI's vault, which only the CLI reads.
        Ok(match provider {
            Provider::Hanzo => std::env::var("HANZO_API_KEY").ok().filter(|k| !k.trim().is_empty()),
            Provider::Anthropic | Provider::OpenAI => None,
        })
    }

    fn meter<'a>(&'a self, query: &'a [String]) -> Pending<'a, std::result::Result<Value, String>> {
        Box::pin(async move {
            let mut args = vec!["usage".to_string(), "summary".to_string()];
            for pair in query {
                let (k, v) = pair.split_once('=').ok_or_else(|| format!("not a query pair: {pair}"))?;
                args.extend([format!("--{k}"), v.to_string()]);
            }
            let out = self.ask(&args).await?;
            serde_json::from_str(&out).map_err(|e| format!("the usage summary did not read: {e}"))
        })
    }
}

/// The `owner` and `name` an IAM access token states. A label for the records
/// this machine keeps, never a decision: the server verifies every token it is
/// sent, and a claim read here grants nothing.
fn claims(token: &str) -> Option<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Claims {
        owner: String,
        name: String,
    }
    let mut parts = token.split('.');
    let body = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(b), Some(s), None) if !h.is_empty() && !b.is_empty() && !s.is_empty() => b,
        _ => return None,
    };
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body.trim_end_matches('=')).ok()?;
    let c: Claims = serde_json::from_slice(&raw).ok()?;
    let fits = |v: &str| !v.is_empty() && v.len() <= 128 && !v.contains(['/', '\\']) && !v.chars().any(char::is_control);
    (fits(&c.owner) && fits(&c.name)).then_some((c.owner, c.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: &str) -> String {
        let b = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        format!("{}.{}.sig", b(r#"{"alg":"RS256"}"#), b(claims))
    }

    /// A stand-in `hanzo` that prints `out` and exits `code`, in a directory of its own.
    #[cfg(unix)]
    fn program(out: &str, code: i32) -> (tempfile::TempDir, String) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hanzo");
        std::fs::write(&path, format!("#!/bin/sh\necho \"$@\" > \"$0.args\"\nprintf '%s' '{out}'\nexit {code}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A thread of another test that forks while this file is open for writing
        // carries that descriptor until it execs, and the kernel refuses to run a
        // file held open for writing. Run it once that has passed.
        while let Err(e) = std::process::Command::new(&path).output() {
            assert_eq!(e.kind(), std::io::ErrorKind::ExecutableFileBusy, "{e}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let p = path.display().to_string();
        (dir, p)
    }

    #[test]
    fn an_account_is_the_tokens_own_owner_and_name() {
        assert_eq!(claims(&jwt(r#"{"owner":"hanzo","name":"z"}"#)), Some(("hanzo".into(), "z".into())));
        assert_eq!(claims(&jwt(r#"{"owner":"hanzo"}"#)), None, "a token naming no one names no account");
        assert_eq!(claims(&jwt(r#"{"owner":"../x","name":"z"}"#)), None, "a claim cannot address another slot");
        assert_eq!(claims("hk-0123456789"), None, "a gateway key is not an account");
    }

    #[test]
    fn an_account_never_prints_its_token() {
        let a = Account { owner: "hanzo".into(), name: "z".into(), token: "SECRET".into() };
        assert_eq!(a.to_string(), "hanzo/z");
        assert!(!format!("{a:?}").contains("SECRET"));
    }

    /// `gen` signs in as Hanzo Dev does: the CLI's `auth token`, the brand named
    /// only when it is not the default.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_account_is_the_clis_token() {
        let token = jwt(r#"{"owner":"hanzo","name":"z"}"#);
        let (_dir, p) = program(&format!("{token}\n"), 0);
        let host = Command::new(&p);
        let a = host.account(BRAND).await.unwrap().expect("signed in");
        assert_eq!((a.owner.as_str(), a.name.as_str(), a.token.as_str()), ("hanzo", "z", token.as_str()));
        assert_eq!(std::fs::read_to_string(format!("{p}.args")).unwrap().trim(), "auth token");
        host.account("lux").await.unwrap();
        assert_eq!(std::fs::read_to_string(format!("{p}.args")).unwrap().trim(), "auth token --brand lux");
    }

    /// An agent that outlives a token asks the same `hanzo` Gen asked, never `gen`.
    #[test]
    fn a_fresh_token_comes_from_the_cli() {
        assert_eq!(Command::new("/opt/bin/hanzo").profile().refresh, ["/opt/bin/hanzo", "auth", "token"]);
        let found = Command::default().profile().refresh;
        assert_eq!(found[1..], ["auth", "token"]);
        assert!(found[0].ends_with("hanzo") || found[0].ends_with("hanzo.exe"), "{found:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_cli_that_refuses_or_is_missing_is_signed_out() {
        let (_dir, p) = program("", 1);
        assert!(Command::new(&p).account(BRAND).await.unwrap().is_none());
        assert!(Command::new("/nonexistent/hanzo").account(BRAND).await.unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_meter_is_the_clis_usage_summary() {
        let (_dir, p) = program(r#"{"llm":{"available":false}}"#, 0);
        let q = ["range=custom".to_string(), "start=a".to_string(), "end=b".to_string()];
        let doc = Command::new(&p).meter(&q).await.unwrap();
        assert_eq!(doc["llm"]["available"], false);
        assert_eq!(
            std::fs::read_to_string(format!("{p}.args")).unwrap().trim(),
            "usage summary --range custom --start a --end b"
        );
        let missing = Command::new("/nonexistent/hanzo").meter(&q).await.unwrap_err();
        assert!(missing.contains("not installed"), "{missing}");
    }
}
