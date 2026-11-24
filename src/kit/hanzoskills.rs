//! Public `hanzoskills.com` stack signal detection, ranking, and cached `SKILL.md` synthesis.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{private_dir, writable, Skill};
use crate::root::Root;

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

