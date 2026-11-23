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
fn writable(name: &str) -> bool {
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

/// Default public skills origin (`hanzoskills.com`), overridable via
/// `HANZOSKILLS_API_URL` and disabled when `HANZOSKILLS_DISABLE=1`.
#[cfg(not(test))]
const HANZOSKILLS_API: &str = "https://hanzoskills.com";
/// Seconds cached `hanzoskills.com` index/docs stay fresh (10 minutes).
const HANZOSKILLS_TTL: i64 = 600;
/// Fast startup budget for `hanzoskills.com` auto-detection (1500ms).
const HANZOSKILLS_WAIT: Duration = Duration::from_millis(1500);
/// Maximum auto-detected `hanzoskills.com` skills per session.
const HANZOSKILLS_LIMIT: usize = 4;

/// Where `hanzoskills.com` index + skill docs are cached: `~/.hanzo/hanzoskills.json`.
pub fn hanzoskills_cache(at: &Root) -> PathBuf {
    at.hanzo().join("hanzoskills.json")
}

/// Active `hanzoskills.com` origin for real user sessions, or `None` when disabled.
#[cfg(not(test))]
pub fn hanzoskills_api() -> Option<String> {
    if matches!(
        std::env::var("HANZOSKILLS_DISABLE").ok().as_deref(),
        Some("1" | "true")
    ) {
        return None;
    }
    Some(
        std::env::var("HANZOSKILLS_API_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| HANZOSKILLS_API.to_string()),
    )
}

/// Unit tests never reach the public internet unasked; tests pass [`AutoSkills`]
/// pointing at a local loopback server.
#[cfg(test)]
pub fn hanzoskills_api() -> Option<String> {
    None
}

/// Workspace + prompt context for `hanzoskills.com` auto-detection.
pub struct AutoSkills<'a> {
    pub cache: &'a Path,
    pub api: &'a str,
    pub cwd: &'a Path,
    pub task: Option<&'a str>,
}

/// Stack and prompt signals extracted from `cwd` and `task`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetectedSignals {
    /// Explicit `$slug` invocations from `task`.
    pub explicit: Vec<String>,
    /// Project stack tags inferred from workspace files in `cwd`.
    pub stack: Vec<String>,
    /// Technical keywords extracted from `task`.
    pub terms: Vec<String>,
}

impl DetectedSignals {
    pub fn is_empty(&self) -> bool {
        self.explicit.is_empty() && self.stack.is_empty() && self.terms.is_empty()
    }

    /// Compact search query string combining explicit slugs, stack signals, and top prompt terms.
    pub fn query(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for s in self.explicit.iter().chain(&self.stack).chain(&self.terms) {
            if !parts.contains(&s.as_str()) {
                parts.push(s.as_str());
            }
            if parts.len() >= 4 {
                break;
            }
        }
        parts.join(" ")
    }
}

const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "your", "have",
    "make", "sure", "need", "want", "using", "used", "work", "code", "file", "files",
    "please", "help", "can", "how", "what", "when", "where", "also", "just", "like",
    "some", "fix", "add", "update", "create", "build", "run", "test", "tests", "check",
    "read", "write", "edit", "change", "remove", "delete", "implement", "refactor",
    "all", "new", "use", "get", "set", "let", "our", "out", "not", "are", "was",
];

/// Detect project stack signals from `cwd` and `$skill` / technical keywords from `task`.
pub fn detect_signals(cwd: &Path, task: Option<&str>) -> DetectedSignals {
    let mut out = DetectedSignals::default();
    let mut push_stack = |tag: &str| {
        if !out.stack.iter().any(|s| s == tag) {
            out.stack.push(tag.to_string());
        }
    };

    if cwd.join("tauri.conf.json").is_file() || cwd.join("src-tauri").join("tauri.conf.json").is_file() {
        push_stack("tauri");
        push_stack("rust");
    }
    if cwd.join("Cargo.toml").is_file() {
        push_stack("rust");
    }
    let pkg_json = cwd.join("package.json");
    if pkg_json.is_file() || cwd.join("tsconfig.json").is_file() {
        push_stack("typescript");
        if let Ok(f) = std::fs::File::open(&pkg_json) {
            use std::io::Read;
            let mut body = String::new();
            let _ = f.take(32 << 10).read_to_string(&mut body);
            let lower = body.to_ascii_lowercase();
            if lower.contains("\"next\"") {
                push_stack("nextjs");
            }
            if lower.contains("\"react\"") {
                push_stack("react");
            }
            if lower.contains("\"@tauri-apps/") {
                push_stack("tauri");
            }
            if lower.contains("\"tailwindcss\"") {
                push_stack("tailwind");
            }
            if lower.contains("\"prisma\"") || lower.contains("\"drizzle-orm\"") {
                push_stack("sql");
            }
            if lower.contains("\"viem\"") || lower.contains("\"ethers\"") || lower.contains("\"wagmi\"") {
                push_stack("web3");
            }
        }
    }
    if cwd.join("pyproject.toml").is_file()
        || cwd.join("requirements.txt").is_file()
        || cwd.join("uv.lock").is_file()
    {
        push_stack("python");
    }
    if cwd.join("go.mod").is_file() {
        push_stack("go");
    }
    if cwd.join("Dockerfile").is_file()
        || cwd.join("compose.yml").is_file()
        || cwd.join("docker-compose.yml").is_file()
    {
        push_stack("docker");
    }
    if cwd.join(".github").join("workflows").is_dir()
        || cwd.join(".hanzo").join("workflows").is_dir()
    {
        push_stack("github-actions");
    }
    if cwd.join("foundry.toml").is_file()
        || cwd.join("hardhat.config.ts").is_file()
        || cwd.join("hardhat.config.js").is_file()
        || cwd.join("remappings.txt").is_file()
        || cwd.join("solidity").is_dir()
    {
        push_stack("solidity");
        push_stack("evm");
    }
    if cwd.join("Chart.yaml").is_file() || cwd.join("kustomization.yaml").is_file() {
        push_stack("kubernetes");
    }
    if cwd.join("hanzo.yml").is_file() || cwd.join("hanzo.toml").is_file() {
        push_stack("hanzo");
    }

    if let Some(prompt) = task {
        let lower = prompt.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        let mut idx = 0;
        while idx < bytes.len() {
            if bytes[idx] == b'$' {
                let start = idx + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_lowercase()
                        || bytes[end].is_ascii_digit()
                        || matches!(bytes[end], b'_' | b'-'))
                {
                    end += 1;
                }
                if end > start {
                    let slug = &lower[start..end];
                    if writable(slug) && !out.explicit.iter().any(|s| s == slug) {
                        out.explicit.push(slug.to_string());
                    }
                }
                idx = end;
            } else {
                idx += 1;
            }
        }

        for tok in lower.split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')) {
            let tok = tok.trim_matches(|c| c == '-' || c == '_');
            if tok.len() < 2
                || tok.bytes().all(|b| b.is_ascii_digit())
                || STOP_WORDS.contains(&tok)
                || out.explicit.iter().any(|s| s == tok)
                || out.stack.iter().any(|s| s == tok)
                || out.terms.iter().any(|s| s == tok)
            {
                continue;
            }
            out.terms.push(tok.to_string());
            if out.terms.len() >= 8 {
                break;
            }
        }
    }

    out
}

/// One skill record from either `GET {api}/api/skills?search=...` or
/// `GET {api}/search.json` (`hanzoskills.com` Tier 1 search index).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct RemoteSkillEntry {
    #[serde(alias = "i", alias = "id")]
    slug: String,
    name: String,
    #[serde(alias = "d", alias = "summary")]
    description: String,
    #[serde(alias = "c")]
    category: String,
    #[serde(alias = "g")]
    tags: Vec<String>,
    #[serde(alias = "body", alias = "markdown")]
    content: Option<String>,
    url: Option<String>,
}

impl RemoteSkillEntry {
    fn clean_slug(&self) -> Option<String> {
        let raw = if !self.slug.trim().is_empty() {
            self.slug.trim()
        } else {
            self.name.trim()
        };
        let norm = raw.to_ascii_lowercase();
        writable(&norm).then_some(norm)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RemoteSkillsPayload {
    List(Vec<RemoteSkillEntry>),
    Wrapped {
        #[serde(default, alias = "s", alias = "items", alias = "data", alias = "results")]
        skills: Vec<RemoteSkillEntry>,
    },
}

impl RemoteSkillsPayload {
    fn into_entries(self) -> Vec<RemoteSkillEntry> {
        match self {
            RemoteSkillsPayload::List(v) => v,
            RemoteSkillsPayload::Wrapped { skills } => skills,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct CachedQuery {
    at: i64,
    entries: Vec<RemoteSkillEntry>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct CachedDoc {
    at: i64,
    content: String,
}

#[derive(Default, Serialize, Deserialize)]
struct HanzoskillsCache {
    api: String,
    #[serde(default)]
    index_at: i64,
    #[serde(default)]
    index: Vec<RemoteSkillEntry>,
    #[serde(default)]
    queries: BTreeMap<String, CachedQuery>,
    #[serde(default)]
    docs: BTreeMap<String, CachedDoc>,
}

fn fresh(now: i64, at: i64) -> bool {
    (0..HANZOSKILLS_TTL).contains(&(now - at))
}

fn read_hanzoskills_cache(path: &Path, api: &str) -> HanzoskillsCache {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<HanzoskillsCache>(&b).ok())
        .filter(|c| c.api == api)
        .unwrap_or_else(|| HanzoskillsCache {
            api: api.to_string(),
            ..HanzoskillsCache::default()
        })
}

fn write_hanzoskills_cache(path: &Path, cache: &HanzoskillsCache) {
    if let Ok(body) = serde_json::to_vec(cache) {
        if let Some(dir) = path.parent() {
            let _ = private_dir(dir);
        }
        let _ = crate::private::write(path, &body);
    }
}

/// Score and select up to `limit` skills from `entries` matching `signals`.
fn rank_hanzoskills(
    entries: &[RemoteSkillEntry],
    signals: &DetectedSignals,
    limit: usize,
) -> Vec<RemoteSkillEntry> {
    let mut weighted: Vec<(&str, i32)> = Vec::new();
    for s in &signals.stack {
        weighted.push((s.as_str(), 2));
        if s == "solidity" {
            weighted.push(("web3", 1));
        } else if s == "github-actions" {
            weighted.push(("ci", 1));
            weighted.push(("cicd", 1));
        }
    }
    for t in &signals.terms {
        weighted.push((t.as_str(), 2));
    }

    let mut scored: Vec<(i32, usize, String, RemoteSkillEntry)> = Vec::new();
    for entry in entries {
        let Some(slug) = entry.clean_slug() else { continue };
        let desc = entry.description.to_ascii_lowercase();
        let cat = entry.category.to_ascii_lowercase();
        let tags: Vec<String> = entry.tags.iter().map(|g| g.to_ascii_lowercase()).collect();
        let slug_parts: Vec<&str> = slug.split(['-', '_']).collect();

        let mut score = 0i32;
        if signals.explicit.iter().any(|e| e == &slug) {
            score += 200;
        }
        for (sig, weight) in &weighted {
            if slug == *sig {
                score += 45 * weight;
            } else if sig.contains('-') && slug.contains(sig) {
                score += 30 * weight;
            } else if slug_parts.contains(sig) {
                score += 22 * weight;
            } else if tags.iter().any(|t| t == sig) {
                score += 18 * weight;
            } else if slug.contains(sig) && sig.len() >= 4 {
                score += 10 * weight;
            } else if desc.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == *sig) {
                score += 5 * weight;
            } else if cat == *sig {
                score += 4 * weight;
            }
        }
        if score >= 12 {
            scored.push((score, slug.len(), slug, entry.clone()));
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));
    let mut out = Vec::new();
    for (_, _, slug, entry) in scored {
        if out.iter().any(|e: &RemoteSkillEntry| e.clean_slug().as_deref() == Some(slug.as_str())) {
            continue;
        }
        out.push(entry);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Ensure `SKILL.md` begins with valid YAML frontmatter (`name` + `description`),
/// synthesizing a complete document when only summary metadata is available.
fn format_skill_doc(base: &str, slug: &str, entry: &RemoteSkillEntry, raw_md: Option<&str>) -> String {
    let desc: String = entry
        .description
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(240)
        .collect::<String>()
        .trim()
        .replace('"', "'");
    let desc_line = if desc.is_empty() {
        format!("Hanzo skill {slug}")
    } else {
        desc
    };

    if let Some(md) = raw_md.map(str::trim).filter(|s| !s.is_empty()) {
        if md.starts_with("---\n") || md.starts_with("---\r\n") {
            return format!("{md}\n");
        }
        return format!("---\nname: {slug}\ndescription: \"{desc_line}\"\n---\n\n{md}\n");
    }

    let title = if entry.name.trim().is_empty() {
        slug
    } else {
        entry.name.trim()
    };
    let source = entry
        .url
        .as_deref()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .map(String::from)
        .unwrap_or_else(|| format!("{}/skills/{slug}/SKILL.md", base.trim_end_matches('/')));
    format!(
        "---\nname: {slug}\ndescription: \"{desc_line}\"\n---\n\n# {title}\n\n{desc_line}\n\nSource: {source}\n"
    )
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(char::from(b));
        } else if b == b' ' {
            out.push_str("%20");
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn is_static_hanzoskills_host(base: &str) -> bool {
    reqwest::Url::parse(base)
        .ok()
        .and_then(|u| u.host_str().map(|h| matches!(h, "hanzoskills.com" | "www.hanzoskills.com")))
        .unwrap_or(false)
}

/// Fetch and cache relevant `hanzoskills.com` skills within [`HANZOSKILLS_WAIT`].
pub async fn load_hanzoskills(auto: &AutoSkills<'_>) -> Vec<Skill> {
    let signals = detect_signals(auto.cwd, auto.task);
    if signals.is_empty() {
        return Vec::new();
    }
    tokio::time::timeout(HANZOSKILLS_WAIT, fetch_hanzoskills_cached(auto, &signals))
        .await
        .unwrap_or_default()
}

async fn fetch_hanzoskills_cached(auto: &AutoSkills<'_>, signals: &DetectedSignals) -> Vec<Skill> {
    let base = auto.api.trim_end_matches('/');
    if base.is_empty() {
        return Vec::new();
    }
    let now = chrono::Utc::now().timestamp();
    let mut store = read_hanzoskills_cache(auto.cache, base);
    let mut dirty = false;

    let Ok(client) = reqwest::Client::builder().timeout(HANZOSKILLS_WAIT).build() else {
        return Vec::new();
    };

    let query = signals.query();
    let mut selected: Option<Vec<RemoteSkillEntry>> = None;

    // 1. Custom / REST API mode (`GET {base}/api/skills?search={query}&limit=4`),
    //    skipped on the static `hanzoskills.com` S3 host where `/search.json` is
    //    the canonical Tier-1 index. Falls back to `/search.json` on 404.
    if !is_static_hanzoskills_host(base) && !query.is_empty() {
        if let Some(hit) = store.queries.get(&query).filter(|q| fresh(now, q.at)) {
            selected = Some(hit.entries.clone());
        } else {
            let url = format!(
                "{base}/api/skills?search={}&limit={HANZOSKILLS_LIMIT}",
                urlencode(&query)
            );
            if let Ok(resp) = client.get(&url).send().await {
                if resp.status().is_success() {
                    if let Ok(payload) = resp.json::<RemoteSkillsPayload>().await {
                        let mut entries: Vec<RemoteSkillEntry> = payload
                            .into_entries()
                            .into_iter()
                            .filter(|e| e.clean_slug().is_some())
                            .take(HANZOSKILLS_LIMIT)
                            .collect();
                        // Deduplicate preserving server ranking order.
                        let mut dedup = Vec::new();
                        for e in entries.drain(..) {
                            let slug = e.clean_slug().unwrap();
                            if !dedup.iter().any(|x: &RemoteSkillEntry| x.clean_slug().as_deref() == Some(slug.as_str())) {
                                dedup.push(e);
                            }
                        }
                        store.queries.insert(
                            query.clone(),
                            CachedQuery { at: now, entries: dedup.clone() },
                        );
                        dirty = true;
                        selected = Some(dedup);
                    }
                } else if resp.status().as_u16() != 404 {
                    return Vec::new();
                }
            } else {
                return Vec::new();
            }
        }
    }

    // 2. Progressive static index mode (`GET {base}/search.json`), used natively by
    //    `https://hanzoskills.com` and as fallback when `/api/skills` is absent.
    let entries = match selected {
        Some(list) => list,
        None => {
            if !fresh(now, store.index_at) || store.index.is_empty() {
                let url = format!("{base}/search.json");
                let Ok(resp) = client.get(&url).send().await else {
                    return Vec::new();
                };
                if !resp.status().is_success() {
                    return Vec::new();
                }
                let Ok(payload) = resp.json::<RemoteSkillsPayload>().await else {
                    return Vec::new();
                };
                let parsed = payload.into_entries();
                if parsed.is_empty() {
                    return Vec::new();
                }
                store.index = parsed;
                store.index_at = now;
                dirty = true;
            }
            rank_hanzoskills(&store.index, signals, HANZOSKILLS_LIMIT)
        }
    };

    if entries.is_empty() {
        if dirty {
            write_hanzoskills_cache(auto.cache, &store);
        }
        return Vec::new();
    }

    // 3. Resolve `SKILL.md` for each matched entry: inline `content`, fresh cache,
    //    or `GET {base}/skills/{slug}/SKILL.md` (with synthesized fallback).
    let mut to_fetch = Vec::new();
    for entry in &entries {
        let Some(slug) = entry.clean_slug() else { continue };
        let has_inline = entry.content.as_deref().is_some_and(|s| !s.trim().is_empty());
        let has_cached = store.docs.get(&slug).is_some_and(|d| fresh(now, d.at));
        if !has_inline && !has_cached {
            to_fetch.push(slug);
        }
    }

    if !to_fetch.is_empty() {
        let fetches = to_fetch.into_iter().map(|slug| {
            let client = client.clone();
            let url = format!("{base}/skills/{slug}/SKILL.md");
            async move {
                let text = match client.get(&url).send().await {
                    Ok(r) if r.status().is_success() => r.text().await.ok(),
                    _ => None,
                };
                (slug, text)
            }
        });
        for (slug, maybe_md) in futures_join_all(fetches).await {
            if let Some(entry) = entries.iter().find(|e| e.clean_slug().as_deref() == Some(slug.as_str())) {
                let doc = format_skill_doc(base, &slug, entry, maybe_md.as_deref());
                store.docs.insert(slug, CachedDoc { at: now, content: doc });
                dirty = true;
            }
        }
    }

    let mut out = Vec::new();
    for entry in &entries {
        let Some(slug) = entry.clean_slug() else { continue };
        let content = if let Some(inline) = entry.content.as_deref().filter(|s| !s.trim().is_empty()) {
            let doc = format_skill_doc(base, &slug, entry, Some(inline));
            if !store.docs.get(&slug).is_some_and(|d| fresh(now, d.at)) {
                store.docs.insert(slug.clone(), CachedDoc { at: now, content: doc.clone() });
                dirty = true;
            }
            doc
        } else if let Some(cached) = store.docs.get(&slug).filter(|d| fresh(now, d.at)) {
            cached.content.clone()
        } else {
            format_skill_doc(base, &slug, entry, None)
        };
        out.push(Skill { name: slug, content });
    }

    if dirty {
        write_hanzoskills_cache(auto.cache, &store);
    }
    out
}

async fn futures_join_all<F, T>(futs: impl IntoIterator<Item = F>) -> Vec<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handles: Vec<_> = futs.into_iter().map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(v) = h.await {
            out.push(v);
        }
    }
    out
}

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
mod tests {
    use super::*;
    use crate::testmock::MockCloud;
    use serde_json::json;

    fn wire(v: serde_json::Value) -> Kit {
        Kit::from(serde_json::from_value::<Wire>(v).unwrap())
    }

    fn skill(name: &str, body: &str) -> Skill {
        Skill { name: name.into(), content: body.into() }
    }

    const KIT: &str = r#"{
        "skills": [
            {"name":"review","content":"---\nname: review\ndescription: review a diff\n---\nread it"}
        ],
        "servers": [{"name":"github","tools":["search_code"]}],
        "muted": ["skill_deploy","linear"]
    }"#;

    fn caller<'a>(api: &'a str, who: &'a str) -> Caller<'a> {
        // The mock is plain http on loopback, so it stands in for the local network.
        Caller { api, who, token: "IAM-BEARER", local: true }
    }

    /// The kit is carried as the cloud answers it: the cloud already left out what
    /// the person muted, so nothing here re-decides it.
    #[test]
    fn the_kit_is_carried_as_the_cloud_answers_it() {
        let kit = wire(serde_json::from_str(KIT).unwrap());
        assert_eq!(kit.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["review"]);
        assert_eq!(kit.servers, ["github"]);
        assert!(kit.omitted.is_empty(), "a muted skill is not 'left out': {:?}", kit.omitted);

        // A name twice is carried once.
        let kit = wire(json!({"skills":[{"name":"a","content":"x"},{"name":"a","content":"y"}]}));
        assert_eq!(kit.skills, [skill("a", "x")]);
    }

    /// A name no directory can safely take, or a document with nothing in it, is
    /// left out and SAID — never written, and never printed raw.
    #[test]
    fn an_unwritable_skill_is_left_out_and_named_printably() {
        let kit = wire(json!({"skills":[
            {"name":"../../.bashrc","content":"x"},
            {"name":"Upper","content":"x"},
            {"name":"empty","content":"  \n"},
            {"name":"\u{1b}[2Jwipe","content":"x"}
        ],"servers":[{"name":"\u{1b}]52;c;evil\u{7}gh"}]}));
        assert!(kit.skills.is_empty());
        assert_eq!(kit.omitted, [".....bashrc", "Upper", "empty", "2Jwipe"]);
        for s in kit.omitted.iter().chain(&kit.servers) {
            assert!(!s.contains('\u{1b}') && !s.contains('\u{7}') && !s.contains('/'), "{s:?}");
        }
    }

    /// The tool app's bounds hold here too: 16 skills and 512 KiB, the rest named.
    #[test]
    fn a_run_carries_at_most_sixteen_skills_and_512_kib() {
        let rows: Vec<_> = (0..18).map(|i| json!({"name":format!("s{i}"),"content":"x"})).collect();
        let kit = wire(json!({ "skills": rows }));
        assert_eq!(kit.skills.len(), 16);
        assert_eq!(kit.omitted, ["s16", "s17"]);

        let big = "x".repeat(300 << 10);
        let kit = wire(json!({"skills":[
            {"name":"a","content":big},
            {"name":"b","content":big},
            {"name":"c","content":"small"}
        ]}));
        assert_eq!(kit.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(kit.omitted, ["b"]);
    }

    #[tokio::test]
    async fn the_kit_is_read_as_the_caller_with_the_iam_bearer() {
        let mock = MockCloud::start_kit(KIT).await;
        let dir = tempfile::tempdir().unwrap();
        let api = mock.base_url();
        let kit = load(&dir.path().join("kit.json"), Some(&caller(&api, "hanzo/z"))).await.unwrap();
        assert_eq!(kit.skills[0].name, "review");

        let r = &mock.requests()[0];
        assert_eq!((r.method.as_str(), r.path.as_str()), ("GET", "/v1/tool/kit"));
        assert_eq!(r.header("authorization").as_deref(), Some("Bearer IAM-BEARER"));
        assert!(r.header("x-org-id").is_none(), "the org is the bearer's, never sent");
    }

    /// Back-to-back runs cost one request; another identity, another network, or a
    /// stale entry is a fresh read. The cache is owner-only.
    #[tokio::test]
    async fn a_fresh_cache_answers_for_the_same_caller_only() {
        let mock = MockCloud::start_kit(KIT).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kit.json");
        let api = mock.base_url();

        load(&path, Some(&caller(&api, "hanzo/z"))).await.unwrap();
        load(&path, Some(&caller(&api, "hanzo/z"))).await.unwrap();
        assert_eq!(mock.requests().len(), 1, "a fresh cache answers the second run");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        load(&path, Some(&caller(&api, "acme/z"))).await.unwrap();
        assert_eq!(mock.requests().len(), 2, "another org's run never reads this org's kit");

        let other = format!("{api}/");
        load(&path, Some(&caller(&other, "acme/z"))).await.unwrap();
        assert_eq!(mock.requests().len(), 3, "another network is another cache key");

        let mut c: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        c["at"] = json!(c["at"].as_i64().unwrap() - TTL);
        std::fs::write(&path, c.to_string()).unwrap();
        load(&path, Some(&caller(&other, "acme/z"))).await.unwrap();
        assert_eq!(mock.requests().len(), 4, "a stale entry is read again");
    }

    #[tokio::test]
    async fn no_kit_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kit.json");
        assert_eq!(load(&path, None).await, Err(Miss::SignedOut));

        let mock = MockCloud::start_status(401).await;
        let api = mock.base_url();
        assert_eq!(load(&path, Some(&caller(&api, "hanzo/z"))).await, Err(Miss::Refused(401)));

        // Before the server serves a kit at all.
        let mock = MockCloud::start().await;
        let api = mock.base_url();
        assert_eq!(load(&path, Some(&caller(&api, "hanzo/z"))).await, Err(Miss::Refused(404)));

        // Nothing listening.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let api = format!("http://127.0.0.1:{port}");
        assert_eq!(load(&path, Some(&caller(&api, "hanzo/z"))).await, Err(Miss::Unreachable));
        assert!(!path.exists(), "a miss caches nothing");
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    /// Each carried skill is `<root>/<name>/SKILL.md`, recorded in a ledger outside
    /// the root; a skill the kit stops carrying is withdrawn; the person's own are
    /// never touched, and nothing but SKILL.md is written into the root.
    #[test]
    fn sync_writes_the_kit_and_withdraws_only_what_it_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let (root, ledger) = (dir.path().join("skills"), dir.path().join(LEDGER));
        std::fs::create_dir_all(root.join("mine")).unwrap();
        std::fs::write(root.join("mine").join(DOC), "my own").unwrap();

        let s = sync(&root, &ledger, &[skill("review", "r1"), skill("lint", "l1")]).unwrap();
        assert_eq!(s.written, ["review", "lint"]);
        assert_eq!(read(&root.join("review").join(DOC)), "r1");
        let entries: Vec<_> = std::fs::read_dir(root.join("review")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(entries, [DOC], "only SKILL.md is written into the root");
        assert!(read(&ledger).contains(&digest(b"r1")), "the ledger records what was written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&root.join("review")), 0o700);
            assert_eq!(mode(&root.join("review").join(DOC)), 0o600);
            assert_eq!(mode(&ledger), 0o600);
        }

        // The kit changed: `lint` went away, `review` was rewritten.
        let s = sync(&root, &ledger, &[skill("review", "r2")]).unwrap();
        assert_eq!(s.written, ["review"]);
        assert_eq!(read(&root.join("review").join(DOC)), "r2");
        assert!(!root.join("lint").exists(), "a skill the kit dropped is withdrawn");
        assert_eq!(read(&root.join("mine").join(DOC)), "my own", "the person's own is untouched");

        // No kit: every org skill goes, the person's stays.
        sync(&root, &ledger, &[]).unwrap();
        assert!(!root.join("review").exists());
        assert_eq!(read(&root.join("mine").join(DOC)), "my own");
    }

    /// A SKILL.md the person edited is theirs: it is neither rewritten nor removed,
    /// on this run or any later one, and whatever they put beside an org skill
    /// outlives its withdrawal.
    #[test]
    fn a_customized_copy_survives() {
        let dir = tempfile::tempdir().unwrap();
        let (root, ledger) = (dir.path().join("skills"), dir.path().join(LEDGER));
        sync(&root, &ledger, &[skill("review", "org"), skill("lint", "org")]).unwrap();

        std::fs::write(root.join("review").join(DOC), "my review").unwrap();
        std::fs::write(root.join("lint").join("notes.md"), "mine").unwrap();

        let s = sync(&root, &ledger, &[skill("review", "org v2")]).unwrap();
        assert_eq!(s.yours, ["review"]);
        assert_eq!(read(&root.join("review").join(DOC)), "my review", "an edit is never overwritten");
        assert!(!root.join("lint").join(DOC).exists(), "an unchanged org file is withdrawn");
        assert_eq!(read(&root.join("lint").join("notes.md")), "mine", "the person's file beside it stays");

        sync(&root, &ledger, &[]).unwrap();
        assert_eq!(read(&root.join("review").join(DOC)), "my review", "an edit is never withdrawn");
        assert!(!read(&ledger).contains("review"), "an edited file leaves the ledger");
    }

    /// An org skill named like one of the person's yields to theirs, and is SAID.
    /// A link in the root is never written through or removed.
    #[test]
    fn sync_never_writes_over_the_persons_skill_or_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let (root, ledger) = (dir.path().join("skills"), dir.path().join(LEDGER));
        std::fs::create_dir_all(root.join("review")).unwrap();
        std::fs::write(root.join("review").join(DOC), "my review").unwrap();

        let s = sync(&root, &ledger, &[skill("review", "org review")]).unwrap();
        assert!(s.written.is_empty());
        assert_eq!(s.yours, ["review"]);
        assert_eq!(read(&root.join("review").join(DOC)), "my review");

        #[cfg(unix)]
        {
            let elsewhere = dir.path().join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::fs::write(elsewhere.join(DOC), "precious").unwrap();
            std::os::unix::fs::symlink(&elsewhere, root.join("linked")).unwrap();

            let s = sync(&root, &ledger, &[skill("linked", "planted")]).unwrap();
            assert_eq!(s.yours, ["linked"], "a link is not ours to write through");
            sync(&root, &ledger, &[]).unwrap();
            assert_eq!(read(&elsewhere.join(DOC)), "precious", "nor ours to remove");
            assert!(root.join("linked").exists());

            // An org skill the person replaced with a link is theirs, and what the
            // link points at is untouched when the kit drops it.
            sync(&root, &ledger, &[skill("lint", "l1")]).unwrap();
            std::fs::remove_dir_all(root.join("lint")).unwrap();
            let target = dir.path().join("target");
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(target.join(DOC), "l1").unwrap();
            std::os::unix::fs::symlink(&target, root.join("lint")).unwrap();
            sync(&root, &ledger, &[]).unwrap();
            assert_eq!(read(&target.join(DOC)), "l1", "a link's target is never removed");

            // A SKILL.md planted as a link inside an org skill is the person's.
            sync(&root, &ledger, &[skill("fmt", "f1")]).unwrap();
            let bashrc = dir.path().join("bashrc");
            std::fs::write(&bashrc, "precious").unwrap();
            std::fs::remove_file(root.join("fmt").join(DOC)).unwrap();
            std::os::unix::fs::symlink(&bashrc, root.join("fmt").join(DOC)).unwrap();
            let s = sync(&root, &ledger, &[skill("fmt", "f2")]).unwrap();
            assert_eq!(s.yours, ["fmt"]);
            assert_eq!(read(&bashrc), "precious");
        }

        // A name that would climb out of the root never reaches the filesystem, even
        // from a cache or a ledger someone edited.
        for name in ["../escape", "..", ".", "a/b", "/etc", "a\\b", ""] {
            let s = sync(&root, &ledger, &[skill(name, "x")]).unwrap();
            assert!(s.written.is_empty(), "{name:?} was written");
        }
        assert!(!dir.path().join("escape").exists());
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join(DOC), "x").unwrap();
        let forged = serde_json::json!({ root.to_string_lossy(): { "../outside": digest(b"x") } });
        std::fs::write(&ledger, forged.to_string()).unwrap();
        sync(&root, &ledger, &[]).unwrap();
        assert_eq!(read(&outside.join(DOC)), "x", "a forged ledger entry cannot remove a file outside the root");
    }

    /// The bearer goes to https, or to plain http on the local network only; a
    /// refused origin is never asked.
    #[tokio::test]
    async fn the_bearer_goes_only_to_an_https_origin() {
        assert!(trusted("https://api.hanzo.ai", false));
        assert!(trusted("http://localhost:3690", true));
        for (api, local) in [
            ("http://api.hanzo.ai", false),
            ("ftp://api.hanzo.ai", true),
            ("https://user:pw@api.hanzo.ai", false),
            ("not a url", true),
            ("https://", false),
        ] {
            assert!(!trusted(api, local), "{api} local={local}");
        }

        let mock = MockCloud::start_kit(KIT).await;
        let dir = tempfile::tempdir().unwrap();
        let api = mock.base_url();
        let remote = Caller { api: &api, who: "hanzo/z", token: "IAM-BEARER", local: false };
        assert_eq!(load(&dir.path().join("kit.json"), Some(&remote)).await, Err(Miss::Insecure));
        assert!(mock.requests().is_empty(), "the bearer was sent over plain http");
        let e = equip(&dir.path().join("kit.json"), &dir.path().join("skills"), Some(&remote), true).await;
        assert!(e.remote.is_none());
    }

    /// The whole step against a mock kit and MCP server: the skills land where the
    /// backend reads them, and the server a backend is handed answers an MCP
    /// `initialize` for the caller — the bearer its configuration names, sent.
    #[tokio::test]
    async fn equip_writes_the_skills_and_hands_over_an_authenticated_mcp_server() {
        let mock = MockCloud::start_kit(KIT).await;
        let dir = tempfile::tempdir().unwrap();
        let (cache, root) = (dir.path().join("kit.json"), dir.path().join("skills"));
        let api = mock.base_url();

        let e = equip(&cache, &root, Some(&caller(&api, "hanzo/z")), true).await;
        assert!(read(&root.join("review").join(DOC)).contains("description: review a diff"));
        let remote = e.remote.expect("the kit answered and MCP is on");
        assert_eq!(remote.url, format!("{api}/v1/mcp"));

        // Each backend is handed this server; ask it what the harness would.
        let reply = reqwest::Client::new()
            .post(&remote.url)
            .bearer_auth(&remote.token)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);
        let body: serde_json::Value = reply.json().await.unwrap();
        assert_eq!(body["result"]["serverInfo"]["name"], "mock");
        let r = mock.requests().into_iter().find(|r| r.path == "/v1/mcp").unwrap();
        assert_eq!(r.header("authorization").as_deref(), Some("Bearer IAM-BEARER"));

        // --no-mcp: the skills still come, the server does not.
        let e = equip(&cache, &root, Some(&caller(&api, "hanzo/z")), false).await;
        assert!(e.remote.is_none() && root.join("review").exists());

        // Signed out: no server, and the org's skills are withdrawn so the run
        // truly starts without the kit the banner says it lacks.
        let e = equip(&cache, &root, None, true).await;
        assert!(e.remote.is_none());
        assert!(!root.join("review").exists());
    }

    /// The MCP server refuses a call without the bearer — so the test above proves
    /// the harness's configuration is what authenticates it.
    #[tokio::test]
    async fn the_mock_mcp_server_refuses_an_anonymous_call() {
        let mock = MockCloud::start_kit(KIT).await;
        let reply = reqwest::Client::new()
            .post(format!("{}/v1/mcp", mock.base_url()))
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 401);
    }

    #[test]
    fn only_the_backends_that_read_skills_carry_a_kit() {
        let via = Route::Inherit;
        let at = Root::at("/h");
        assert!(root(&at, BackendKind::parse("agy").unwrap(), &via).is_none());
        assert!(root(&at, BackendKind::parse("cursor").unwrap(), &via).is_none());
        assert_eq!(root(&at, BackendKind::Dev, &via), Some(PathBuf::from("/h/.agents/skills")));
        assert_eq!(root(&at, BackendKind::Codex, &via), Some(PathBuf::from("/h/.agents/skills")));
        assert_eq!(root(&at, BackendKind::Claude, &via), Some(PathBuf::from("/h/.claude/skills")));
    }

    #[test]
    fn detect_signals_extracts_workspace_stack_and_prompt_terms() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert!(detect_signals(p, None).is_empty(), "empty dir with no task yields no signals");

        std::fs::write(p.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::write(p.join("tauri.conf.json"), "{}").unwrap();
        std::fs::write(
            p.join("package.json"),
            r#"{"dependencies":{"next":"15.0.0","react":"19.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(p.join("pyproject.toml"), "[project]\nname = \"py\"\n").unwrap();
        std::fs::write(p.join("go.mod"), "module example.com/m\n").unwrap();
        std::fs::write(p.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::create_dir_all(p.join(".github").join("workflows")).unwrap();
        std::fs::write(p.join("foundry.toml"), "[profile.default]\n").unwrap();

        let sig = detect_signals(p, Some("Use $hanzo-mcp to deploy kubernetes cluster"));
        assert_eq!(sig.explicit, ["hanzo-mcp"]);
        for expected in [
            "tauri",
            "rust",
            "typescript",
            "nextjs",
            "react",
            "python",
            "go",
            "docker",
            "github-actions",
            "solidity",
            "evm",
        ] {
            assert!(sig.stack.iter().any(|s| s == expected), "missing stack signal {expected}: {:?}", sig.stack);
        }
        assert!(sig.terms.iter().any(|t| t == "deploy"));
        assert!(sig.terms.iter().any(|t| t == "kubernetes"));
        assert!(!sig.query().is_empty());
    }

    #[tokio::test]
    async fn hanzoskills_api_endpoint_fetches_caches_and_synthesizes_skill_md() {
        let api_json = r##"{
            "skills": [
                {
                    "slug": "rust-sdk",
                    "name": "Hanzo Rust SDK",
                    "description": "Full Rust infrastructure SDK for agents, PQC, and MCP."
                },
                {
                    "slug": "wasm-rust-toolchain",
                    "name": "WASM Rust Toolchain",
                    "description": "Compile Rust crates to WebAssembly.",
                    "content": "# WASM Toolchain\n\nBuild with wasm-bindgen."
                }
            ]
        }"##;
        let mock = MockCloud::start_hanzoskills_api(
            api_json,
            &[("rust-sdk", "# Hanzo Rust SDK\n\n14 active crates.")],
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();

        let hs_cache = dir.path().join("hanzoskills.json");
        let base = mock.base_url();
        let auto = AutoSkills {
            cache: &hs_cache,
            api: &base,
            cwd: &ws,
            task: Some("optimize tokio async"),
        };

        let skills = load_hanzoskills(&auto).await;
        assert_eq!(
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["rust-sdk", "wasm-rust-toolchain"]
        );
        assert!(skills[0].content.starts_with("---\nname: rust-sdk\n"));
        assert!(skills[0].content.contains("14 active crates."));
        assert!(skills[1].content.starts_with("---\nname: wasm-rust-toolchain\n"));
        assert!(skills[1].content.contains("Build with wasm-bindgen."));

        let initial_reqs = mock.requests().len();
        assert!(
            mock.requests().iter().any(|r| r.path.starts_with("/api/skills?search=rust")),
            "expected /api/skills?search=... call, got: {:?}",
            mock.requests()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&hs_cache).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // Second run within the 10-minute TTL hits 0 network requests.
        let cached_skills = load_hanzoskills(&auto).await;
        assert_eq!(cached_skills, skills);
        assert_eq!(mock.requests().len(), initial_reqs, "10-minute TTL cache must prevent repeat network fetches");
    }

    #[tokio::test]
    async fn hanzoskills_live_search_index_merges_with_kit_and_works_when_signed_out() {
        let search_json = r#"{
            "v": 1,
            "t": 3,
            "s": [
                {"i": "rust-sdk", "c": "hanzo", "d": "Hanzo Rust infrastructure SDK.", "g": ["hanzo", "rust", "sdk"]},
                {"i": "review", "c": "hanzo", "d": "Public review fallback.", "g": ["review", "rust"]},
                {"i": "python-sdk", "c": "hanzo", "d": "Hanzo Python SDK.", "g": ["hanzo", "python"]}
            ]
        }"#;
        let mock = MockCloud::start_hanzoskills_site(
            Some(KIT),
            search_json,
            &[
                ("rust-sdk", "# Hanzo Rust SDK\n\nNative crates."),
                ("review", "# Public Review\n\nPublic body."),
            ],
        )
        .await;

        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("repo");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[package]\nname = \"gen\"\n").unwrap();

        let (kit_cache, hs_cache, skills_root) = (
            dir.path().join("kit.json"),
            dir.path().join("hanzoskills.json"),
            dir.path().join("skills"),
        );
        let api = mock.base_url();
        let auto = AutoSkills {
            cache: &hs_cache,
            api: &api,
            cwd: &ws,
            task: None,
        };

        // 1. Signed out (`caller = None`): org kit is absent, yet public hanzoskills.com
        //    skills STILL install via kit::sync + kit-written.json!
        let e = equip_with(&kit_cache, &skills_root, None, true, Some(&auto)).await;
        assert!(e.remote.is_none());
        assert!(skills_root.join("rust-sdk").join(DOC).is_file());
        assert!(read(&skills_root.join("rust-sdk").join(DOC)).contains("Native crates."));
        assert!(read(&skills_root.join("review").join(DOC)).contains("Public body."));
        assert!(!skills_root.join("python-sdk").exists(), "unrelated language skills are not installed");

        // 2. Signed in (`caller = Some(...)`): org skill `review` overrides public `review`,
        //    while public `rust-sdk` stays alongside it!
        let e = equip_with(&kit_cache, &skills_root, Some(&caller(&api, "hanzo/z")), true, Some(&auto)).await;
        assert!(e.remote.is_some());
        assert!(
            read(&skills_root.join("review").join(DOC)).contains("description: review a diff"),
            "org kit skill must win over public skill of the same name"
        );
        assert!(skills_root.join("rust-sdk").join(DOC).is_file());
    }
}
