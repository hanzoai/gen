//! The org's integrations in a local run — its skills and its MCP tools — carried
//! the way the cloud coding agent carries them (hanzo-inc/cloud
//! `apps/coding/kit.go` `equipped`, `apps/coding/sandboxrunner.go` `equip`).
//!
//! `GET {api}/v1/tool/kit` answers the caller's kit — the skills and MCP servers an
//! admin of their org put in place, less what the caller muted (their layer only
//! narrows the admin's) — which is the one read the cloud run makes too. Each
//! skill is written as `<name>/SKILL.md` where the harness reads its user's
//! skills, the same roots the sandbox writes:
//!
//! ```text
//! dev, codex   ~/.agents/skills         the Agent Skills user root
//! claude       <config home>/skills     ~/.hanzo/claude routed, ~/.claude on --no-route
//! ```
//!
//! The tools are ONE MCP server, `{api}/v1/mcp`, authenticated with the CLI's IAM
//! bearer ([`Remote`]), on a Hanzo-routed run only. The bearer goes to the active
//! network's own origin and nowhere else, over https — plain http only on the
//! `local` network.
//!
//! THESE ROOTS ARE SHARED WITH THE PERSON'S OWN SKILLS. The sandbox writes into a
//! throwaway home; this writes into a real one. So what this writes is recorded
//! OUTSIDE the root, in a ledger (`~/.hanzo/kit-written.json`) of each file and
//! the digest it was written with. A file is this module's to rewrite or remove
//! only while the ledger names it and it still reads as written: one the person
//! edited, or that was never written here, is theirs and is never touched. Each
//! run reconciles the root — a skill the kit no longer carries is withdrawn, and
//! with no kit every org skill is — so when the banner says "off", the disk agrees.
//!
//! Read on each run through a [`TTL`] cache keyed by network and identity, so
//! back-to-back runs cost one request and a switch-off reaches this machine within
//! a minute. No kit — signed out, refused, unreachable — never stops a run: it
//! starts without one and the banner says why, once.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hanzo_client::{Http, Method, Request, Transport};

use super::backend::{BackendKind, Remote, Route};
use super::home;
use crate::root::Root;

/// Seconds a fetched kit stands in for a fresh one.
const TTL: i64 = 60;
/// How long a run waits for the kit before it starts without one.
const WAIT: Duration = Duration::from_secs(2);
/// The most skills, and bytes of them, one run carries: the tool app's bounds
/// (apps/tools `maxSkillDocs`, `maxKitBytes`), held here too.
const MAX_SKILLS: usize = 16;
const MAX_BYTES: usize = 512 << 10;
/// The ledger of what this wrote, beside the cache and outside every root.
const LEDGER: &str = "kit-written.json";
/// The one file of a skill a harness reads.
const DOC: &str = "SKILL.md";

/// What a run carries of its org's kit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Kit {
    pub skills: Vec<Skill>,
    /// The org's MCP servers this caller has on, by printable name. They are
    /// reached through `/v1/mcp`; the names are for the banner.
    pub servers: Vec<String>,
    /// Carried skills left out: over the bound, or no harness could take them.
    pub omitted: Vec<String>,
}

/// One skill's SKILL.md under its bare name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Skill {
    pub name: String,
    pub content: String,
}

/// The kit as `GET /v1/tool/kit` answers it (hanzo-inc/cloud `client.Kit`): already
/// the caller's. Fields this does not read are ignored.
#[derive(Deserialize)]
struct Wire {
    #[serde(default)]
    skills: Vec<Skill>,
    #[serde(default)]
    servers: Vec<Server>,
    #[serde(default)]
    omitted: Vec<String>,
}

/// One MCP server of the kit, by its id. Its tools are reached through `/v1/mcp`.
#[derive(Deserialize)]
struct Server {
    #[serde(default)]
    name: String,
}

impl From<Wire> for Kit {
    fn from(w: Wire) -> Kit {
        let mut kit = Kit::default();
        let mut bytes = 0;
        for skill in w.skills {
            if kit.skills.iter().any(|s| s.name == skill.name) {
                continue;
            }
            let fits = kit.skills.len() < MAX_SKILLS && bytes + skill.content.len() <= MAX_BYTES;
            if !writable(&skill.name) || skill.content.trim().is_empty() || !fits {
                kit.omitted.extend(printable(&skill.name));
                continue;
            }
            bytes += skill.content.len();
            kit.skills.push(skill);
        }
        kit.omitted.extend(w.omitted.iter().filter_map(|n| printable(n)));
        kit.servers = w.servers.iter().filter_map(|s| printable(&s.name)).collect();
        kit
    }
}

/// A skill name the tool plane admits, which is also the directory it is written
/// into: one lowercase path segment, `^[a-z0-9][a-z0-9_-]{0,63}$`. Nothing else
/// reaches the filesystem, so no name can climb out of the root.
pub(super) fn writable(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=64).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

/// A server-supplied name as the terminal may show it: letters, digits, space and
/// `._-`, at most 64. A control sequence in a name must never reach the screen.
fn printable(s: &str) -> Option<String> {
    let clean: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'))
        .take(64)
        .collect();
    let clean = clean.trim();
    (!clean.is_empty()).then(|| clean.to_string())
}

/// Why a run has no kit.
#[derive(Debug, Clone, PartialEq)]
pub enum Miss {
    /// No IAM bearer on this machine.
    SignedOut,
    /// The server answered and refused, with its status.
    Refused(u16),
    /// The server could not be reached within [`WAIT`].
    Unreachable,
    /// A 2xx whose body is not a kit.
    Unreadable,
    /// The network's api is not https (and the network is not `local`), so the
    /// bearer is not sent.
    Insecure,
}

impl std::fmt::Display for Miss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Miss::SignedOut => write!(f, "not signed in — `hanzo auth login`"),
            Miss::Refused(code @ (401 | 403)) => write!(f, "refused ({code}) — `hanzo auth login`"),
            Miss::Refused(code) => write!(f, "refused ({code})"),
            Miss::Unreachable => write!(f, "cloud unreachable"),
            Miss::Unreadable => write!(f, "unreadable answer"),
            Miss::Insecure => write!(f, "the network's api is not https"),
        }
    }
}

/// Who a run's kit is for. The org is derived server-side from the bearer, and
/// `who` keys the cache.
pub struct Caller<'a> {
    /// The active network's api origin: the one place the bearer is sent.
    pub api: &'a str,
    /// `owner/name` — the cache is never read across identities.
    pub who: &'a str,
    pub token: &'a str,
    /// The active network is `local`, the one that may speak plain http.
    pub local: bool,
}

/// Whether the bearer may go to `api`: https with a host and no userinfo, or
/// plain http on the `local` network.
fn trusted(api: &str, local: bool) -> bool {
    let Ok(url) = reqwest::Url::parse(api) else { return false };
    let scheme = url.scheme() == "https" || (local && url.scheme() == "http");
    scheme && url.host_str().is_some_and(|h| !h.is_empty()) && url.username().is_empty() && url.password().is_none()
}

/// Where the kit is cached: `~/.hanzo/kit.json`, owner-only.
pub fn cache(at: &Root) -> PathBuf {
    at.hanzo().join("kit.json")
}

/// Where `kind` reads its user's skills under `route`, or `None` for a backend
/// that carries no kit (the agents of [`super::agents`] read neither).
pub fn root(at: &Root, kind: BackendKind, route: &Route) -> Option<PathBuf> {
    match kind {
        BackendKind::Dev | BackendKind::Codex => Some(at.home().join(".agents").join("skills")),
        BackendKind::Claude => Some(home::of(at, route).join("skills")),
        BackendKind::Interpreter | BackendKind::Tool(_) => None,
    }
}

#[derive(Serialize, Deserialize)]
struct Cached {
    api: String,
    who: String,
    at: i64,
    kit: Kit,
}

/// The cached kit, when it was fetched for this network and identity less than
/// [`TTL`] ago. A clock that went backwards reads as stale.
fn cached(path: &Path, api: &str, who: &str, now: i64) -> Option<Kit> {
    let c: Cached = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (c.api == api && c.who == who && (0..TTL).contains(&(now - c.at))).then_some(c.kit)
}

/// `GET {api}/v1/tool/kit` as the caller.
async fn fetch(api: &str, token: &str) -> Result<Kit, Miss> {
    let wire = reqwest::Client::builder().timeout(WAIT).build().map_err(|_| Miss::Unreachable)?;
    let url = format!("{}/v1/tool/kit", api.trim_end_matches('/'));
    let reply = Http::new(wire)
        .send(Request::new(Method::GET, url).token(token))
        .await
        .map_err(|_| Miss::Unreachable)?;
    if !(200..300).contains(&reply.status) {
        return Err(Miss::Refused(reply.status));
    }
    serde_json::from_value::<Wire>(reply.body).map(Kit::from).map_err(|_| Miss::Unreadable)
}

/// The caller's kit: cached when fresh, else fetched and cached.
pub async fn load(cache: &Path, caller: Option<&Caller<'_>>) -> Result<Kit, Miss> {
    let c = caller.ok_or(Miss::SignedOut)?;
    if !trusted(c.api, c.local) {
        return Err(Miss::Insecure);
    }
    let now = chrono::Utc::now().timestamp();
    if let Some(kit) = cached(cache, c.api, c.who, now) {
        return Ok(kit);
    }
    let kit = fetch(c.api, c.token).await?;
    let entry = Cached { api: c.api.to_string(), who: c.who.to_string(), at: now, kit };
    if let Ok(body) = serde_json::to_vec(&entry) {
        if let Some(dir) = cache.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Best-effort: a cache that cannot be written costs a request next run.
        let _ = crate::private::write(cache, &body);
    }
    Ok(entry.kit)
}

/// What reconciling a skills root did.
#[derive(Debug, Default, PartialEq)]
pub struct Synced {
    /// Org skills on disk now.
    pub written: Vec<String>,
    /// Org skills not written because the person has their own of that name.
    pub yours: Vec<String>,
}

/// The ledger: for each root, each skill this wrote and the digest of the
/// SKILL.md it wrote.
type Ledger = BTreeMap<String, BTreeMap<String, String>>;

fn digest(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Whether `<root>/<name>/SKILL.md` is still exactly what this wrote: a real
/// directory holding a regular file whose digest the ledger recorded. A link, an
/// edit or a missing file is the person's.
fn unchanged(root: &Path, name: &str, recorded: &str) -> bool {
    let dir = root.join(name);
    std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir())
        && std::fs::symlink_metadata(dir.join(DOC)).is_ok_and(|m| m.is_file())
        && std::fs::read(dir.join(DOC)).is_ok_and(|b| digest(&b) == recorded)
}

/// Reconcile `root` to exactly `skills`, touching only what `ledger` says this
/// wrote and the person left as written: withdraw each such skill the kit no
/// longer carries (its SKILL.md, then its directory if nothing else is in it),
/// write each carried skill whose name is free or still ours, and leave every
/// other file alone. A file the person edited drops out of the ledger — it is
/// theirs from then on.
pub fn sync(root: &Path, ledger: &Path, skills: &[Skill]) -> io::Result<Synced> {
    let mut out = Synced::default();
    let mut all: Ledger = std::fs::read(ledger).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let key = root.to_string_lossy().to_string();
    let wrote = all.remove(&key).unwrap_or_default();
    let ours = |name: &str| wrote.get(name).is_some_and(|d| writable(name) && unchanged(root, name, d));

    for name in wrote.keys().filter(|n| !skills.iter().any(|s| &s.name == *n)) {
        if ours(name) {
            let dir = root.join(name);
            match std::fs::remove_file(dir.join(DOC)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            // Only if empty: anything the person put beside it stays, and so does
            // the directory holding it.
            let _ = std::fs::remove_dir(&dir);
        }
    }

    let mut now = BTreeMap::new();
    for s in skills {
        // Checked again here, not only on the wire: the cache is a file too.
        if !writable(&s.name) {
            continue;
        }
        let dir = root.join(&s.name);
        match std::fs::symlink_metadata(&dir) {
            Ok(_) if !ours(&s.name) => {
                out.yours.push(s.name.clone());
                continue;
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => private_dir(&dir)?,
            Err(e) => return Err(e),
        }
        crate::private::write(&dir.join(DOC), s.content.as_bytes())?;
        now.insert(s.name.clone(), digest(s.content.as_bytes()));
        out.written.push(s.name.clone());
    }

    if !now.is_empty() {
        all.insert(key, now);
    }
    if !wrote.is_empty() || !all.is_empty() {
        if let Some(dir) = ledger.parent() {
            private_dir(dir)?;
        }
        crate::private::write(ledger, &serde_json::to_vec(&all).map_err(io::Error::other)?)?;
    }
    Ok(out)
}

/// Create `dir` and its parents, owner-only where this creates them; an existing
/// directory keeps its own mode.
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}


mod hanzoskills;
#[cfg(test)]
use hanzoskills::detect_signals;
pub use hanzoskills::{hanzoskills_api, hanzoskills_cache, load_hanzoskills, AutoSkills};

/// Merge org `kit` skills with auto-detected `hanzoskills.com` public skills,
/// giving org skills priority on name collisions and respecting [`MAX_SKILLS`] + [`MAX_BYTES`].
pub fn merge_skills(org: &[Skill], public: &[Skill]) -> Vec<Skill> {
    let mut merged: Vec<Skill> = Vec::new();
    let mut bytes = 0usize;
    for s in org.iter().chain(public.iter()) {
        if !writable(&s.name) || s.content.trim().is_empty() {
            continue;
        }
        if merged.iter().any(|existing| existing.name == s.name) {
            continue;
        }
        if merged.len() >= MAX_SKILLS || bytes + s.content.len() > MAX_BYTES {
            continue;
        }
        bytes += s.content.len();
        merged.push(s.clone());
    }
    merged
}

/// What a run takes from the kit.
pub struct Equipped {
    /// The cloud MCP server to attach: the kit answered and MCP is on.
    pub remote: Option<Remote>,
}

/// Load the kit, reconcile `skills` to it, and decide the MCP server — the whole
/// of a run's kit in one step, so the banner cannot disagree with the disk.
#[cfg(test)]
pub async fn equip(cache: &Path, skills: &Path, caller: Option<&Caller<'_>>, mcp: bool) -> Equipped {
    equip_with(cache, skills, caller, mcp, None).await
}

/// Load both the org's kit (`GET /v1/tool/kit`) and auto-detected `hanzoskills.com`
/// public skills concurrently, merge them (even when signed out or when the org kit
/// is empty), and reconcile `skills` via `kit-written.json`.
pub async fn equip_with(
    cache: &Path,
    skills: &Path,
    caller: Option<&Caller<'_>>,
    mcp: bool,
    auto: Option<&AutoSkills<'_>>,
) -> Equipped {
    let org_fut = load(cache, caller);
    let pub_fut = async {
        match auto {
            Some(a) => load_hanzoskills(a).await,
            None => Vec::new(),
        }
    };
    let (kit, public_skills) = tokio::join!(org_fut, pub_fut);
    let org_skills = kit.as_ref().map(|k| k.skills.as_slice()).unwrap_or_default();
    let carried = merge_skills(org_skills, &public_skills);
    if let Err(e) = sync(skills, &cache.with_file_name(LEDGER), &carried) {
        eprintln!("  skills not written: {e}");
    }
    let remote = match (&kit, caller) {
        (Ok(_), Some(c)) if mcp => Some(Remote {
            url: format!("{}/v1/mcp", c.api.trim_end_matches('/')),
            token: c.token.to_string(),
        }),
        _ => None,
    };
    Equipped { remote }
}

#[cfg(test)]
mod tests;
