//! The coding harness's account table.
//!
//! Every connected account lives in one list: a Hanzo identity (`hanzo:{owner}/{name}`,
//! the shared gateway login) and each native agent the person has signed in
//! (`claude`, `codex`, `agy`, `cursor`). The harness may use any of them. Which
//! one a bare session uses is `harness` in `~/.hanzo/settings.json`, or
//! `HANZO_HARNESS_ACCOUNT` for a single run. Each session appends one line to
//! `~/.hanzo/usage.jsonl` naming the account it actually used.
//!
//! The list and the log are ordinary files under a directory this module is
//! handed, so a test never writes the real home.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::backend::BackendKind;

/// One account the harness can run as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// `claude` / `codex` / `agy` / `cursor`, or `hanzo:{owner}/{name}`.
    pub id: String,
    /// `native` or `hanzo`. A Hanzo identity is the shared gateway login.
    pub kind: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct NativeList {
    #[serde(default)]
    native: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Usage {
    account: String,
    at: u64,
}

/// Native backends a person can sign in to. `dev` is ours and is not an account.
const NATIVE: &[BackendKind] = &[
    BackendKind::Claude,
    BackendKind::Codex,
    BackendKind::Agy,
    BackendKind::Cursor,
];

/// Which configured agent a bare `hanzo code` runs.
///
/// A harness pin that names a native backend wins over `agent`. A pin of
/// `hanzo` (the gateway) clears that default, so a later Hanzo login does not
/// keep launching the agent that was signed in before it.
pub fn agent_for(harness: Option<&str>, agent: Option<&str>) -> Option<String> {
    match harness.map(str::trim).filter(|s| !s.is_empty()) {
        Some(h) if BackendKind::parse(h).is_ok() && h != "dev" => Some(h.to_string()),
        Some(h) if h == "hanzo" || h.starts_with("hanzo:") => None,
        _ => agent.filter(|s| !s.is_empty()).map(str::to_string),
    }
}

/// The account this session actually ran as, which is what the usage log records.
pub fn used_account(
    backend: BackendKind,
    inherited: bool,
    hanzo: Option<(&str, &str)>,
) -> String {
    if inherited && NATIVE.contains(&backend) {
        return backend.as_str().to_string();
    }
    match hanzo {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() => {
            format!("hanzo:{owner}/{name}")
        }
        _ => "hanzo".to_string(),
    }
}

/// Every account the harness may use: the Hanzo identities, then each native
/// agent that has been connected. Order is stable; duplicates are dropped.
pub fn inventory(identities: &[(&str, &str)], native: &[String]) -> Vec<Account> {
    let mut out = Vec::new();
    for (owner, name) in identities {
        let id = format!("hanzo:{owner}/{name}");
        if out.iter().any(|a: &Account| a.id == id) {
            continue;
        }
        out.push(Account {
            id,
            kind: "hanzo",
        });
    }
    for name in native {
        let Ok(kind) = BackendKind::parse(name) else {
            continue;
        };
        if !NATIVE.contains(&kind) {
            continue;
        }
        let id = kind.as_str().to_string();
        if out.iter().any(|a: &Account| a.id == id) {
            continue;
        }
        out.push(Account {
            id,
            kind: "native",
        });
    }
    out
}

/// Pick one connected account. `wanted` of `hanzo` means the active identity
/// (`active` is `owner/name`) when that identity is connected, otherwise the
/// first Hanzo identity. `None` is the same choice, so an unpinned harness
/// still has exactly one account to use.
pub fn choose<'a>(
    accounts: &'a [Account],
    wanted: Option<&str>,
    active: Option<&str>,
) -> Result<&'a Account> {
    let wanted = wanted.map(str::trim).filter(|s| !s.is_empty());
    let picked = match wanted {
        Some("hanzo") | None => hanzo_account(accounts, active),
        Some(id) if id.starts_with("hanzo:") => accounts.iter().find(|a| a.id == id),
        Some(id) => {
            let canonical = native_kind(id).map(|k| k.as_str().to_string());
            accounts
                .iter()
                .find(|a| Some(a.id.as_str()) == canonical.as_deref() || a.id == id)
        }
    };
    match picked {
        Some(account) => Ok(account),
        None => {
            let have = if accounts.is_empty() {
                "none".to_string()
            } else {
                accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ")
            };
            bail!(
                "harness account {} is not connected (connected: {have})",
                wanted.unwrap_or("hanzo")
            )
        }
    }
}

/// The same spellings `hanzo auth login` accepts (`chatgpt` → codex, `agent` → cursor).
fn native_kind(name: &str) -> Option<BackendKind> {
    let canonical = crate::iam::native::find(name)
        .map(|agent| agent.backend)
        .or_else(|| BackendKind::parse(name).ok().map(|k| k.as_str()))?;
    let kind = BackendKind::parse(canonical).ok()?;
    NATIVE.contains(&kind).then_some(kind)
}

fn hanzo_account<'a>(accounts: &'a [Account], active: Option<&str>) -> Option<&'a Account> {
    if let Some(active) = active {
        let id = format!("hanzo:{active}");
        if let Some(found) = accounts.iter().find(|a| a.id == id) {
            return Some(found);
        }
    }
    accounts.iter().find(|a| a.kind == "hanzo")
}

/// Remember a native sign-in. Connecting the same agent twice does not add a
/// second row, and does not drop any other connected agent.
pub fn connect(dir: &Path, backend: &str) -> Result<()> {
    let kind = native_kind(backend).ok_or_else(|| {
        anyhow::anyhow!("{backend} is not a connected-account kind")
    })?;
    let path = dir.join("accounts.json");
    let mut list = read_native(&path);
    let id = kind.as_str();
    if !list.native.iter().any(|n| n == id) {
        list.native.push(id.to_string());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&list)?;
    crate::private::write(&path, format!("{body}\n").as_bytes())
        .map_err(|e| anyhow::anyhow!("could not write {} ({e})", path.display()))
}

pub fn connected(dir: &Path) -> Vec<String> {
    read_native(&dir.join("accounts.json")).native
}

/// Append one usage line. The file is the record of which connected account
/// each session used.
pub fn record(dir: &Path, account: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("usage.jsonl");
    let line = serde_json::to_string(&Usage {
        account: account.to_string(),
        at: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    })?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn usage(dir: &Path) -> Vec<String> {
    let Ok(body) = std::fs::read_to_string(dir.join("usage.jsonl")) else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|line| serde_json::from_str::<Usage>(line).ok())
        .map(|u| u.account)
        .collect()
}

/// Record a native sign-in in the real `~/.hanzo` directory.
pub fn connect_home(backend: &str) -> Result<()> {
    connect(&home()?, backend)
}

/// Check that a harness pin names a connected account, then record the account
/// this session used. Returns the usage log so far. A pin of `None` records
/// without requiring a selection.
pub fn track(
    identities: &[(&str, &str)],
    active: Option<&str>,
    pin: Option<&str>,
    used: &str,
) -> Result<Vec<String>> {
    let dir = home()?;
    let accounts = inventory(identities, &connected(&dir));
    if pin.is_some() {
        choose(&accounts, pin, active)?;
    }
    record(&dir, used)?;
    Ok(usage(&dir))
}

fn home() -> Result<PathBuf> {
    super::settings::Settings::path()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .ok_or_else(|| anyhow::anyhow!("no home directory — harness accounts are not available"))
}

fn read_native(path: &Path) -> NativeList {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or(NativeList { native: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hanzo_pin_does_not_keep_the_previous_agent() {
        assert_eq!(agent_for(Some("hanzo"), Some("claude")), None);
        assert_eq!(agent_for(Some("hanzo:hanzo/z"), Some("claude")), None);
        assert_eq!(agent_for(Some("cursor"), Some("dev")).as_deref(), Some("cursor"));
        assert_eq!(agent_for(None, Some("codex")).as_deref(), Some("codex"));
    }

    #[test]
    fn an_inherited_native_session_is_that_account() {
        assert_eq!(
            used_account(BackendKind::Claude, true, Some(("hanzo", "z"))),
            "claude"
        );
        assert_eq!(
            used_account(BackendKind::Dev, false, Some(("hanzo", "z"))),
            "hanzo:hanzo/z"
        );
        assert_eq!(used_account(BackendKind::Agy, true, None), "agy");
    }

    /// The whole table, on disk: every connected account can be selected, and
    /// each selection is a usage line. This is the harness an end-to-end run
    /// consults, without a network or a real home directory.
    #[test]
    fn an_e2e_harness_can_use_every_connected_account() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for backend in ["claude", "chatgpt", "antigravity", "agent"] {
            connect(root, backend).unwrap();
        }
        // Connecting Claude again must not duplicate it, or drop the others.
        connect(root, "claude").unwrap();

        let identities = [("hanzo", "z"), ("lux", "a")];
        let accounts = inventory(&identities, &connected(root));
        let ids: Vec<&str> = accounts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["hanzo:hanzo/z", "hanzo:lux/a", "claude", "codex", "agy", "cursor"]
        );

        for id in &ids {
            let chosen = choose(&accounts, Some(id), Some("hanzo/z")).unwrap();
            assert_eq!(chosen.id, *id);
            record(root, &chosen.id).unwrap();
        }
        // `hanzo` is the shared login: the active identity, not a fifth agent.
        let shared = choose(&accounts, Some("hanzo"), Some("lux/a")).unwrap();
        assert_eq!(shared.id, "hanzo:lux/a");
        record(root, &shared.id).unwrap();

        assert_eq!(choose(&accounts, Some("chatgpt"), None).unwrap().id, "codex");
        assert_eq!(choose(&accounts, Some("agent"), None).unwrap().id, "cursor");

        let unpinned = choose(&accounts, None, Some("hanzo/z")).unwrap();
        assert_eq!(unpinned.id, "hanzo:hanzo/z");

        let err = choose(&accounts, Some("nobody"), None).unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");

        let logged = usage(root);
        assert_eq!(logged.len(), ids.len() + 1);
        for id in &ids {
            assert!(logged.contains(&(*id).to_string()), "missing {id} in {logged:?}");
        }
    }
}
