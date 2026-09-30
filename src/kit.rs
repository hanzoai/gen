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

/// Seconds a fetched kit stands in for a fresh one.
const TTL: i64 = 60;
/// How long a run waits for the kit before it starts without one.
const WAIT: Duration = Duration::from_secs(5);
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
pub fn cache() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".hanzo").join("kit.json"))
}

/// Where `kind` reads its user's skills under `route`, or `None` for a backend
/// that carries no kit (Antigravity and Cursor read neither).
pub fn root(kind: BackendKind, route: &Route) -> Option<PathBuf> {
    match kind {
        BackendKind::Dev | BackendKind::Codex => Some(dirs::home_dir()?.join(".agents").join("skills")),
        BackendKind::Claude => Some(home::of(route)?.join("skills")),
        BackendKind::Agy | BackendKind::Cursor => None,
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
fn private_dir(dir: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// What a run takes from the kit.
pub struct Equipped {
    /// The cloud MCP server to attach: the kit answered and MCP is on.
    pub remote: Option<Remote>,
    /// Whether the run carries a kit.
    pub on: bool,
    /// The banner's one line about it.
    pub line: String,
}

/// Load the kit, reconcile `skills` to it, and decide the MCP server — the whole
/// of a run's kit in one step, so the banner cannot disagree with the disk.
pub async fn equip(cache: &Path, skills: &Path, caller: Option<&Caller<'_>>, mcp: bool) -> Equipped {
    let kit = load(cache, caller).await;
    let carried = kit.as_ref().map(|k| k.skills.as_slice()).unwrap_or_default();
    let synced = sync(skills, &cache.with_file_name(LEDGER), carried);
    let remote = match (&kit, caller) {
        (Ok(_), Some(c)) if mcp => Some(Remote {
            url: format!("{}/v1/mcp", c.api.trim_end_matches('/')),
            token: c.token.to_string(),
        }),
        _ => None,
    };
    Equipped { line: line(&kit, &synced, remote.as_ref()), on: kit.is_ok(), remote }
}

/// The banner line, as plain text.
fn line(kit: &Result<Kit, Miss>, synced: &io::Result<Synced>, remote: Option<&Remote>) -> String {
    let kit = match kit {
        Ok(k) => k,
        Err(m) => return format!("kit: off ({m}) — running without the org's skills and tools"),
    };
    let mut parts = Vec::new();
    match synced {
        Ok(s) if s.written.is_empty() => parts.push("no skills".to_string()),
        Ok(s) => parts.push(format!("skills {}", s.written.join(", "))),
        Err(e) => parts.push(format!("skills not written ({e})")),
    }
    if let Some(r) = remote {
        let host = r.url.trim_start_matches("https://").trim_start_matches("http://");
        match kit.servers.as_slice() {
            [] => parts.push(format!("tools → {host}")),
            names => parts.push(format!("tools → {host} ({})", names.join(", "))),
        }
    }
    if !kit.omitted.is_empty() {
        parts.push(format!("left out: {}", kit.omitted.join(", ")));
    }
    if let Ok(s) = synced {
        if !s.yours.is_empty() {
            parts.push(format!("yours kept: {}", s.yours.join(", ")));
        }
    }
    format!("kit: {}", parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code::testmock::MockCloud;
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
        assert!(!e.on && e.remote.is_none());
    }

    #[test]
    fn the_banner_line_says_what_the_run_carries_or_why_not() {
        let kit = wire(serde_json::from_str(KIT).unwrap());
        let synced = Ok(Synced { written: vec!["review".into()], yours: vec!["lint".into()] });
        let remote = Remote { url: "https://api.hanzo.ai/v1/mcp".into(), token: "T".into() };
        assert_eq!(
            line(&Ok(kit.clone()), &synced, Some(&remote)),
            "kit: skills review · tools → api.hanzo.ai/v1/mcp (github) · yours kept: lint"
        );
        // --no-mcp: skills only, and no claim about tools.
        assert_eq!(line(&Ok(kit), &synced, None), "kit: skills review · yours kept: lint");

        assert_eq!(
            line(&Err(Miss::Refused(401)), &Ok(Synced::default()), None),
            "kit: off (refused (401) — `hanzo auth login`) — running without the org's skills and tools"
        );
        assert_eq!(
            line(&Err(Miss::Unreachable), &Ok(Synced::default()), None),
            "kit: off (cloud unreachable) — running without the org's skills and tools"
        );
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
        assert!(e.on);
        assert!(read(&root.join("review").join(DOC)).contains("description: review a diff"));
        let remote = e.remote.expect("the kit answered and MCP is on");
        assert_eq!(remote.url, format!("{api}/v1/mcp"));
        assert!(e.line.starts_with("kit: skills review · tools → 127.0.0.1:"), "{}", e.line);

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
        assert!(e.on && e.remote.is_none());

        // Signed out: no server, and the org's skills are withdrawn so the run
        // truly starts without the kit the banner says it lacks.
        let e = equip(&cache, &root, None, true).await;
        assert!(!e.on && e.remote.is_none());
        assert!(!root.join("review").exists());
        assert!(e.line.starts_with("kit: off (not signed in"), "{}", e.line);
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
        assert!(root(BackendKind::Agy, &via).is_none());
        assert!(root(BackendKind::Cursor, &via).is_none());
        if let Some(home) = dirs::home_dir() {
            assert_eq!(root(BackendKind::Dev, &via), Some(home.join(".agents/skills")));
            assert_eq!(root(BackendKind::Codex, &via), Some(home.join(".agents/skills")));
            assert_eq!(root(BackendKind::Claude, &via), Some(home.join(".claude/skills")));
        }
    }
}
