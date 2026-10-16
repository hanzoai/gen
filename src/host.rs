//! What a session needs from the program it runs in.
//!
//! Gen launches agents; it signs nobody in and stores no credential. Hanzo IAM
//! owns the account, and the Hanzo CLI owns IAM's tokens and the user's settings.
//! So the account, what is on file, the keys a user saved, and the cloud's usage
//! meter all come from the host: the `hanzo` CLI in its own process when Gen is
//! its library, or the `hanzo` command on PATH when Gen runs as `gen` ([`Command`]).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The brand a session signs in under unless `--brand` names another.
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

/// What the host has on file for a session. It carries no secret, which is what
/// lets the CLI hand it over on stdout (`hanzo config profile`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

impl Profile {
    /// What a session runs on when the host cannot say what it has on file: the
    /// public cloud, and NOT linked — a setting that could not be read may be an
    /// opt-out, and streaming a session someone opted out of is not recoverable.
    pub fn unread() -> Profile {
        Profile {
            api: hanzo_client::API.into(),
            local: false,
            org: None,
            provider: None,
            identities: Vec::new(),
            link: false,
            theme: "auto".into(),
        }
    }
}

/// The program Gen runs in.
pub trait Host: Send + Sync {
    /// What is on file: the cloud, the selections, the persisted defaults. A
    /// moment's wait, so a host can read it under the same lock its account
    /// refresh writes under.
    fn profile(&self) -> Pending<'_, Profile>;

    /// The account signed in for `brand`, its access token fresh. `None` when
    /// nobody is signed in, never some other account.
    fn account<'a>(&'a self, brand: &'a str) -> Pending<'a, Result<Option<Account>>>;

    /// The command that prints `brand`'s access token, fresh: what an agent that
    /// outlives one token (they last an hour) runs for the next. Empty when there
    /// is none to run, and the agent keeps the token it was started with.
    fn refresh(&self, brand: &str) -> Vec<String>;

    /// The key the user stored for `provider`, read only when a route needs it.
    fn key(&self, provider: Provider) -> Result<Option<String>>;

    /// `GET /v1/usage/summary` for `query` (`range=custom`, `start=…`, `end=…`) as
    /// `brand`'s account, as its `data`; a refusal or a fault is the reason, never
    /// an empty document, and never another brand's numbers.
    fn meter<'a>(&'a self, brand: &'a str, query: &'a [String]) -> Pending<'a, std::result::Result<Value, String>>;
}

/// The Hanzo CLI's arguments that print `brand`'s access token. Every host asks
/// the CLI the same way, so the brand rides every token a session is handed.
pub fn token(brand: &str) -> Vec<String> {
    ["auth", "token", "--brand", brand].map(String::from).to_vec()
}

/// One part of an `owner/name` a token claims, refused if it could leave its
/// slot. The ONE check: the CLI files credentials under these parts, and Gen
/// labels its records with them, so both ask this.
///
/// A claim is attacker-influenced data: an `owner` of `../hanzo` or `a/b` would
/// let a forged token address ANOTHER identity's storage slot. Structure over
/// trust — and ONLY structure: IAM mints usernames like "Grace Hopper" or "José",
/// so the rule is a deny-list of what actually escapes a slot (separators,
/// control bytes, non-space whitespace), never an allowlist of blessed ASCII.
pub fn claim(field: &str, v: &str) -> Result<()> {
    if v.is_empty() {
        bail!("token claim `{field}` is empty");
    }
    if v.len() > 128 {
        bail!("token claim `{field}` is too long ({} > 128)", v.len());
    }
    if !v.starts_with(|c: char| c.is_alphanumeric()) {
        bail!("token claim `{field}` must start with a letter or digit: {v:?}");
    }
    if v.ends_with(' ') {
        bail!("token claim `{field}` ends with a space: {v:?}");
    }
    if let Some(bad) = v
        .chars()
        .find(|&c| c == '/' || c == '\\' || c.is_control() || (c.is_whitespace() && c != ' '))
    {
        bail!("token claim `{field}` contains an unsupported character {bad:?}: {v:?}");
    }
    Ok(())
}

/// How long the CLI has to answer one question. A refresh that IAM is slow to
/// answer takes seconds; a CLI that has not answered in this long is not going to.
const PATIENCE: Duration = Duration::from_secs(20);

/// Why the CLI did not answer.
#[derive(Debug)]
enum Asked {
    /// There is no `hanzo` to ask.
    Missing,
    /// It answered no, in its own words.
    Refused(String),
    /// It could not be asked, or did not answer in time.
    Failed(String),
}

impl std::fmt::Display for Asked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Asked::Missing => f.write_str("the Hanzo CLI is not installed (`curl -fsSL hanzo.sh | sh` installs it)"),
            Asked::Refused(said) | Asked::Failed(said) => f.write_str(said),
        }
    }
}

/// The host `gen` runs under: the `hanzo` command on PATH, which holds the Hanzo
/// IAM account, refreshes its token, reads the user's settings and reads the cloud
/// for it. Gen asks it the way Hanzo Dev does (`hanzo auth token`), so signing in
/// to one signs in to both.
pub struct Command {
    program: Option<PathBuf>,
    profile: Profile,
}

impl Command {
    /// The `hanzo` PATH finds, and what it has on file.
    pub async fn found() -> Command {
        Command::at(crate::exe::find("hanzo")).await
    }

    /// Ask `program` (`None`: there is no CLI, so signed out and unlinked), and
    /// read what it has on file once, for the whole session.
    pub async fn at(program: Option<PathBuf>) -> Command {
        let mut c = Command { program, profile: Profile::unread() };
        let read = match c.ask(&["config".into(), "profile".into()]).await {
            Ok(out) => serde_json::from_str(&out).map_err(|e| Asked::Failed(format!("its profile did not read: {e}"))),
            Err(e) => Err(e),
        };
        match read {
            Ok(p) => c.profile = p,
            Err(Asked::Missing) => {}
            Err(why) => crate::warn(&format!(
                "the Hanzo CLI did not say what it has on file ({why}), so this session stays local on {} — `hanzo upgrade` brings a CLI that does",
                c.profile.api
            )),
        }
        c
    }

    /// Run the CLI and keep what it printed, or why it printed nothing. Both
    /// streams are captured, so a credential never reaches a terminal or a log.
    async fn ask(&self, args: &[String]) -> std::result::Result<String, Asked> {
        self.ask_within(args, PATIENCE).await
    }

    async fn ask_within(&self, args: &[String], patience: Duration) -> std::result::Result<String, Asked> {
        let program = self.program.as_ref().ok_or(Asked::Missing)?;
        let run = tokio::process::Command::from(crate::exe::command(program))
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(patience, run).await {
            Err(_) => {
                return Err(Asked::Failed(format!(
                    "`hanzo {}` did not answer in {}s",
                    args.join(" "),
                    patience.as_secs_f32()
                )))
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(Asked::Missing),
            Ok(Err(e)) => return Err(Asked::Failed(format!("`{}` did not start: {e}", program.display()))),
            Ok(Ok(out)) => out,
        };
        if !out.status.success() {
            let said = String::from_utf8_lossy(&out.stderr);
            let said: String = said.split_whitespace().collect::<Vec<_>>().join(" ");
            let said = said.strip_prefix("Error: ").unwrap_or(&said);
            return Err(Asked::Refused(said.chars().take(240).collect()));
        }
        String::from_utf8(out.stdout).map_err(|_| Asked::Failed(format!("`{}` printed something that is not text", program.display())))
    }
}

impl Host for Command {
    fn profile(&self) -> Pending<'_, Profile> {
        Box::pin(async move { self.profile.clone() })
    }

    fn account<'a>(&'a self, brand: &'a str) -> Pending<'a, Result<Option<Account>>> {
        Box::pin(async move {
            let out = match self.ask(&token(brand)).await {
                Ok(out) => out,
                Err(Asked::Missing) => return Ok(None),
                // Nobody signed in is a session that runs locally and says so;
                // anything else the CLI refused (a session that ended, a brand it
                // does not know) is its own words, and the session stops on them.
                Err(Asked::Refused(said)) if said.contains("not signed in") => return Ok(None),
                Err(why) => bail!("{why}"),
            };
            let token = out.trim();
            if token.is_empty() || token.len() > 16_384 || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
                bail!("`hanzo auth token` printed something that is not a token");
            }
            match claims(token) {
                Some((owner, name)) => Ok(Some(Account { owner, name, token: token.to_string() })),
                None => bail!("`hanzo auth token` printed a token that names no account"),
            }
        })
    }

    fn refresh(&self, brand: &str) -> Vec<String> {
        match &self.program {
            Some(p) => std::iter::once(p.display().to_string()).chain(token(brand)).collect(),
            None => Vec::new(),
        }
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

    fn meter<'a>(&'a self, brand: &'a str, query: &'a [String]) -> Pending<'a, std::result::Result<Value, String>> {
        Box::pin(async move {
            // `hanzo usage summary` speaks as the CLI's default brand and takes no
            // other, so another brand's meter is not read here rather than read wrong.
            if brand != BRAND {
                return Err(format!("the meter is read for {brand} by `hanzo dev --brand {brand}`, not by gen"));
            }
            let mut args = vec!["usage".to_string(), "summary".to_string()];
            for pair in query {
                let (k, v) = pair.split_once('=').ok_or_else(|| format!("not a query pair: {pair}"))?;
                args.extend([format!("--{k}"), v.to_string()]);
            }
            let out = self.ask(&args).await.map_err(|e| e.to_string())?;
            serde_json::from_str(&out).map_err(|e| format!("the usage summary did not read: {e}"))
        })
    }
}

/// The `owner` and `name` an IAM access token states, each held to [`claim`]. A
/// label for the records this machine keeps, never a decision: the server
/// verifies every token it is sent, and a claim read here grants nothing.
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
    (claim("owner", &c.owner).is_ok() && claim("name", &c.name).is_ok()).then_some((c.owner, c.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: &str) -> String {
        let b = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        format!("{}.{}.sig", b(r#"{"alg":"RS256"}"#), b(claims))
    }

    #[cfg(unix)]
    fn profile() -> Profile {
        Profile {
            api: "http://localhost:8080".into(),
            local: true,
            org: None,
            provider: Some("hanzo".into()),
            identities: vec![("hanzo".into(), "z".into())],
            link: true,
            theme: "dracula".into(),
        }
    }

    /// A stand-in `hanzo` in a directory of its own: each `(first two words, out,
    /// status)` answers that question, and every call's arguments land in
    /// `<program>.args`, one line per call.
    #[cfg(unix)]
    fn program(answers: &[(&str, &str, i32)]) -> (tempfile::TempDir, PathBuf) {
        let mut sh = String::from("echo \"$@\" >> \"$0.args\"\ncase \"$1 $2\" in\n");
        for (q, out, code) in answers {
            sh.push_str(&format!("  \"{q}\") printf '%s' '{out}'; exit {code} ;;\n"));
        }
        sh.push_str("esac\necho \"Error: unrecognized subcommand '$2'\" >&2\nexit 2\n");
        script(&sh)
    }

    /// A stand-in `hanzo` that runs `body`.
    #[cfg(unix)]
    fn script(body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::exe::stub(dir.path(), "hanzo", body);
        let _ = std::fs::remove_file(dir.path().join("hanzo.args"));
        (dir, path)
    }

    #[cfg(unix)]
    fn calls(p: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(format!("{}.args", p.display())).unwrap_or_default().lines().map(String::from).collect()
    }

    #[test]
    fn an_account_is_the_tokens_own_owner_and_name() {
        assert_eq!(claims(&jwt(r#"{"owner":"hanzo","name":"z"}"#)), Some(("hanzo".into(), "z".into())));
        assert_eq!(claims(&jwt(r#"{"owner":"hanzo","name":"Grace Hopper"}"#)), Some(("hanzo".into(), "Grace Hopper".into())));
        assert_eq!(claims(&jwt(r#"{"owner":"hanzo"}"#)), None, "a token naming no one names no account");
        for bad in ["../x", "a/b", "a\\b", "", " lead", "trail ", "tab\there", "x\u{7}"] {
            let doc = serde_json::json!({ "owner": bad, "name": "z" }).to_string();
            assert_eq!(claims(&jwt(&doc)), None, "{bad:?} cannot name a slot");
        }
        assert_eq!(claims("hk-0123456789"), None, "a gateway key is not an account");
    }

    #[test]
    fn an_account_never_prints_its_token() {
        let a = Account { owner: "hanzo".into(), name: "z".into(), token: "SECRET".into() };
        assert_eq!(a.to_string(), "hanzo/z");
        assert!(!format!("{a:?}").contains("SECRET"));
    }

    /// `gen` signs in as Hanzo Dev does: the CLI's `auth token`, for the session's brand.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_account_is_the_clis_token_for_the_sessions_brand() {
        let token = jwt(r#"{"owner":"hanzo","name":"z"}"#);
        let out = format!("{token}\n");
        let (_dir, p) = program(&[("auth token", out.as_str(), 0)]);
        let host = Command::at(Some(p.clone())).await;
        let a = host.account(BRAND).await.unwrap().expect("signed in");
        assert_eq!((a.owner.as_str(), a.name.as_str(), a.token.as_str()), ("hanzo", "z", token.as_str()));
        host.account("lux").await.unwrap();
        let c = calls(&p);
        assert_eq!(c[c.len() - 2..], ["auth token --brand hanzo", "auth token --brand lux"]);
    }

    /// An agent that outlives a token asks the same `hanzo`, for the same brand —
    /// never `gen`, and never the default brand on a session that named another.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_fresh_token_comes_from_the_cli_for_the_sessions_brand() {
        let (_dir, p) = program(&[]);
        let host = Command::at(Some(p.clone())).await;
        let mut want = vec![p.display().to_string()];
        want.extend(token("lux"));
        assert_eq!(host.refresh("lux"), want);
        assert!(Command::at(None).await.refresh(BRAND).is_empty(), "no CLI, no command to run");
    }

    /// Signed out is a session that runs locally; any other refusal is the CLI's own
    /// words, surfaced; a CLI that is not there is signed out.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_clis_refusal_is_said_in_its_own_words() {
        let (_d1, out) = script("echo 'Error: not signed in — run `hanzo auth login`' >&2\nexit 1\n");
        assert!(Command::at(Some(out)).await.account(BRAND).await.unwrap().is_none());

        let (_d2, ended) = script("echo 'Error: the hanzo/z session has ended; sign in again: hanzo auth login' >&2\nexit 1\n");
        let err = Command::at(Some(ended)).await.account(BRAND).await.unwrap_err().to_string();
        assert_eq!(err, "the hanzo/z session has ended; sign in again: hanzo auth login");

        assert!(Command::at(None).await.account(BRAND).await.unwrap().is_none());
    }

    /// A CLI that hangs costs a session `PATIENCE`, then a sentence — never forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_cli_that_hangs_is_given_up_on() {
        let (_d, hang) = script("exec sleep 3600\n");
        let c = Command { program: Some(hang), profile: Profile::unread() };
        let started = std::time::Instant::now();
        let err = c.ask_within(&token(BRAND), Duration::from_millis(300)).await.unwrap_err().to_string();
        assert!(err.contains("did not answer in 0.3s"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10), "it gave up when it said it would");
    }

    /// `gen` reads what the CLI has on file through the CLI, the one reader of its
    /// config — and a CLI that cannot say leaves the session unlinked.
    #[cfg(unix)]
    #[tokio::test]
    async fn what_is_on_file_is_the_clis_and_unreadable_is_unlinked() {
        let doc = serde_json::to_string(&profile()).unwrap();
        let (_d1, p) = program(&[("config profile", doc.as_str(), 0)]);
        assert_eq!(Command::at(Some(p)).await.profile().await, profile());

        let (_d2, old) = program(&[]); // a CLI from before `config profile`
        let unread = Command::at(Some(old)).await.profile().await;
        assert_eq!(unread, Profile::unread());
        assert!(!unread.link, "a setting that could not be read is never a link");
        assert!(!Command::at(None).await.profile().await.link);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_meter_is_the_clis_usage_summary() {
        let (_dir, p) = program(&[("usage summary", r#"{"llm":{"available":false}}"#, 0)]);
        let q = ["range=custom".to_string(), "start=a".to_string(), "end=b".to_string()];
        let doc = Command::at(Some(p.clone())).await.meter(BRAND, &q).await.unwrap();
        assert_eq!(doc["llm"]["available"], false);
        assert_eq!(calls(&p).last().unwrap(), "usage summary --range custom --start a --end b");
        let missing = Command::at(None).await.meter(BRAND, &q).await.unwrap_err();
        assert!(missing.contains("not installed"), "{missing}");
        let other = Command::at(Some(p.clone())).await.meter("lux", &q).await.unwrap_err();
        assert!(other.contains("lux"), "another brand's meter is never this one's: {other}");
        assert_eq!(calls(&p).iter().filter(|c| c.starts_with("usage")).count(), 1, "and the CLI was not asked for it");
    }
}
