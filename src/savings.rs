//! What a coding session cost and what the cache and Kai saved, read three ways
//! and labelled by where each number came from.
//!
//! - **Listed price** (`Book`, `Tally`): tokens a session used, priced at the
//!   rates `GET {api}/v1/models` lists (`pricing.prompt`, `pricing.completion`,
//!   `pricing.rates.input_cache_read`, `input_cache_write`). An estimate: it is
//!   what those tokens cost at the list, not what an account was charged.
//! - **The cloud meter** (`Llm`): `GET /v1/usage/summary`, the org's Hanzo-routed
//!   inference totals. Only the fields the cloud answers are read; one it does not
//!   answer is absent, never zero.
//! - **Formatting**: one spelling of a count, a dollar amount and a share, shared by
//!   the end-of-session block and the console.

use std::collections::HashMap;
use std::time::Duration;

use hanzo_client::Method;
use serde_json::Value;

use crate::commands::product::{envelope_error, Seam};
use crate::config::Config;

// ---- listed prices ------------------------------------------------------------

/// What one token costs, in US dollars. A cache rate is absent where the model has none.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    /// The list marks the model free, so a price of zero is a price and not a gap.
    pub free: bool,
}

/// Hanzo's model list read at one moment: every id with the price it was listed at.
#[derive(Debug, Clone, Default)]
pub struct Book {
    prices: HashMap<String, Price>,
}

fn rate(v: Option<&Value>) -> Option<f64> {
    let n = match v? {
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    (n.is_finite() && n >= 0.0).then_some(n)
}

impl Book {
    /// Read the body of `GET /v1/models`. A row with no input or output price is
    /// left out, never priced at zero.
    pub fn parse(body: &Value) -> Option<Book> {
        let rows = body.get("data")?.as_array()?;
        let mut prices = HashMap::new();
        for row in rows {
            let Some(id) = row.get("id").and_then(Value::as_str) else { continue };
            let Some(pricing) = row.get("pricing") else { continue };
            let (Some(input), Some(output)) = (rate(pricing.get("prompt")), rate(pricing.get("completion"))) else {
                continue;
            };
            let rates = pricing.get("rates");
            prices.insert(
                id.to_lowercase(),
                Price {
                    input,
                    output,
                    cache_read: rate(rates.and_then(|r| r.get("input_cache_read"))),
                    cache_write: rate(rates.and_then(|r| r.get("input_cache_write"))),
                    free: row.get("free").and_then(Value::as_bool).unwrap_or(false),
                },
            );
        }
        (!prices.is_empty()).then_some(Book { prices })
    }

    /// The price a model is listed at, with the id it was found under. A model the
    /// list does not name has none.
    pub fn price(&self, model: &str) -> Option<(String, Price)> {
        candidates(model).into_iter().find_map(|id| self.prices.get(&id).map(|p| (id, *p)))
    }
}

const PROVIDERS: [&str; 3] = ["openai", "anthropic", "google"];

/// The ids a model name may be listed under: with or without its provider, its
/// date, or dotted versions (`claude-sonnet-4-5` is listed as `claude-sonnet-4.5`).
fn candidates(model: &str) -> Vec<String> {
    let named = model.trim().to_lowercase();
    let bare = ["openai/", "anthropic/", "google/", "hanzo/", "zenlm/"]
        .iter()
        .find_map(|p| named.strip_prefix(p))
        .unwrap_or(&named)
        .to_string();
    let undated = undate(&bare);
    let dotted = dot(&undated);
    let mut out: Vec<String> = Vec::new();
    for base in [named, bare, undated, dotted] {
        for id in std::iter::once(base.clone()).chain(PROVIDERS.iter().map(|p| format!("{p}/{base}"))) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// `name-20250929` and `name-2025-09-29` without the date.
fn undate(s: &str) -> String {
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    if let Some((head, tail)) = s.rsplit_once('-') {
        if tail.len() == 8 && digits(tail) {
            return head.to_string();
        }
        let parts: Vec<&str> = s.rsplitn(4, '-').collect();
        if parts.len() == 4 && parts[0].len() == 2 && parts[1].len() == 2 && parts[2].len() == 4 && parts[..3].iter().all(|p| digits(p)) {
            return parts[3].to_string();
        }
    }
    s.to_string()
}

/// `4-5` to `4.5`: a hyphen between two digits, the second not followed by a digit.
fn dot(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let pair = i + 2 < b.len() && b[i].is_ascii_digit() && b[i + 1] == '-' && b[i + 2].is_ascii_digit();
        if pair && !b.get(i + 3).is_some_and(char::is_ascii_digit) {
            out.push(b[i]);
            out.push('.');
            out.push(b[i + 2]);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Read the model list. It answers without a credential; a failure is `None`,
/// and the caller shows tokens without dollars.
pub async fn book(api: &str) -> Option<Book> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().ok()?;
    let url = format!("{}/v1/models", api.trim_end_matches('/'));
    let body: Value = client.get(url).send().await.ok()?.error_for_status().ok()?.json().await.ok()?;
    Book::parse(&body)
}

// ---- tokens at a price --------------------------------------------------------

/// A session's tokens. `input` counts everything read, cached tokens included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub written: u64,
}

impl Tally {
    /// Cached share of input; none with no input.
    pub fn share(&self) -> Option<f64> {
        (self.input > 0).then(|| self.cached as f64 / self.input as f64)
    }

    /// Dollars at one price: uncached input at the input rate, cache reads at the
    /// cache-read rate, cache writes at the cache-write rate, output at the output rate.
    pub fn cost(&self, p: &Price) -> f64 {
        let plain = self.input.saturating_sub(self.cached + self.written) as f64;
        plain * p.input
            + self.cached as f64 * p.cache_read.unwrap_or(p.input)
            + self.written as f64 * p.cache_write.unwrap_or(p.input)
            + self.output as f64 * p.output
    }

    /// What reading the cached tokens from the cache saved against the full input
    /// price. Absent when the model lists no cache-read rate.
    pub fn saved(&self, p: &Price) -> Option<f64> {
        let read = p.cache_read?;
        (self.cached > 0).then_some(self.cached as f64 * (p.input - read))
    }
}

// ---- the cloud meter ----------------------------------------------------------

/// The org's Hanzo-routed inference totals out of `GET /v1/usage/summary`. A field
/// is read from `llm` first and from the top of the document second, and one the
/// cloud does not answer stays `None`: the console says so, it never shows a zero.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Llm {
    /// `llm.available`: false when the warehouse behind the totals is not connected.
    pub available: Option<bool>,
    pub requests: Option<u64>,
    pub prompt: Option<u64>,
    pub completion: Option<u64>,
    pub cost_cents: Option<f64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    /// A fraction of input, 0 to 1.
    pub share: Option<f64>,
    pub saved_cents: Option<f64>,
    /// The scalar fields of the `kai` block, as the cloud names them.
    pub kai: Vec<(String, String)>,
}

fn pick<'a>(summary: &'a Value, name: &str) -> Option<&'a Value> {
    summary.get("llm").and_then(|l| l.get(name)).or_else(|| summary.get(name)).filter(|v| !v.is_null())
}

fn count(summary: &Value, name: &str) -> Option<u64> {
    pick(summary, name).and_then(|v| v.as_u64().or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)))
}

fn number(summary: &Value, name: &str) -> Option<f64> {
    pick(summary, name).and_then(Value::as_f64)
}

impl Llm {
    pub fn of(summary: &Value) -> Llm {
        let kai = pick(summary, "kai")
            .and_then(Value::as_object)
            .map(|block| {
                block
                    .iter()
                    .filter_map(|(k, v)| scalar(k, v).map(|s| (words(k), s)))
                    .collect()
            })
            .unwrap_or_default();
        Llm {
            available: pick(summary, "available").and_then(Value::as_bool),
            requests: count(summary, "requests"),
            prompt: count(summary, "promptTokens"),
            completion: count(summary, "completionTokens"),
            cost_cents: number(summary, "costCents"),
            cache_read: count(summary, "cacheReadTokens"),
            cache_write: count(summary, "cacheWriteTokens"),
            share: number(summary, "cachedShare"),
            saved_cents: number(summary, "cacheSavedCents"),
            kai,
        }
    }

    /// The cache fields the cloud answered, if any did.
    pub fn caches(&self) -> bool {
        self.cache_read.is_some() || self.cache_write.is_some() || self.share.is_some() || self.saved_cents.is_some()
    }

    /// Label and value for each field the cloud answered, measured first. Shared by
    /// the console and the end-of-session block so they cannot read the cloud two ways.
    pub fn rows(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(n) = self.requests {
            out.push(("calls", int(n)));
        }
        let tokens: Vec<String> = [("in", self.prompt), ("out", self.completion)]
            .iter()
            .filter_map(|(k, v)| v.map(|n| format!("{k} {}", int(n))))
            .collect();
        if !tokens.is_empty() {
            out.push(("tokens", tokens.join(" · ")));
        }
        let cached: Vec<String> = [
            self.share.map(|s| format!("{} of input", pct(s))),
            self.cache_read.map(|n| format!("read {}", int(n))),
            self.cache_write.map(|n| format!("written {}", int(n))),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !cached.is_empty() {
            out.push(("cached", cached.join(" · ")));
        }
        if let Some(c) = self.cost_cents {
            out.push(("cost", format!("{} metered", usd(c / 100.0))));
        }
        if let Some(c) = self.saved_cents {
            out.push(("cache saved", usd(c / 100.0)));
        }
        if !self.kai.is_empty() {
            let kai: Vec<String> = self.kai.iter().map(|(k, v)| format!("{k} {v}")).collect();
            out.push(("kai", kai.join(" · ")));
        }
        out
    }
}

/// A scalar of the `kai` block as text: counts with separators, `...Cents` as
/// dollars, other numbers to two places; anything nested is left out.
fn scalar(key: &str, v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => {
            let f = n.as_f64()?;
            Some(if key.ends_with("Cents") {
                usd(f / 100.0)
            } else if let Some(u) = n.as_u64() {
                int(u)
            } else {
                format!("{f:.2}")
            })
        }
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `inputTokens` as `input tokens`; a trailing `Cents` names a price, not a unit.
fn words(key: &str) -> String {
    let key = key.strip_suffix("Cents").unwrap_or(key);
    let mut out = String::new();
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            out.push(' ');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// One plan or account row of the summary's `accounts.rows`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub source: String,
    pub provider: String,
    pub window: String,
    pub used: Option<f64>,
    pub requests: Option<u64>,
    pub tokens: Option<u64>,
    pub cost_cents: Option<f64>,
}

impl Plan {
    /// The linked provider accounts and the org's Hanzo-routed usage, side by side
    /// and never summed: a plan's percent is not money.
    pub fn all(summary: &Value) -> Vec<Plan> {
        let text = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        summary
            .pointer("/accounts/rows")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .map(|r| Plan {
                        source: text(r, "source"),
                        provider: text(r, "provider"),
                        window: text(r, "window"),
                        used: r.get("usedPct").and_then(Value::as_f64),
                        requests: r.get("requests").and_then(Value::as_u64),
                        tokens: r.get("tokens").and_then(Value::as_u64),
                        cost_cents: r.get("costCents").and_then(Value::as_f64),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// How long one cloud read may take.
const PATIENCE: Duration = Duration::from_secs(10);

/// `GET /v1/usage/summary` through the one authenticated seam, as its `data`. A
/// refusal, a transport fault and an error envelope are each a reason, never an
/// empty document.
pub async fn summary(cfg: &mut Config, query: &[String]) -> Result<Value, String> {
    let call = async {
        let seam = Seam::open(cfg).await.map_err(|e| format!("{e:#}"))?;
        let reply = seam
            .send(Method::GET, "/v1/usage/summary", query, None)
            .await
            .map_err(|e| format!("{e:#}"))?;
        let body = reply.body;
        if !(200..300).contains(&reply.status) {
            let said = envelope_error(&body)
                .or_else(|| body.get("detail").and_then(Value::as_str).map(String::from))
                .unwrap_or_else(|| match &body {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    v => v.to_string(),
                });
            let said: String = said.split_whitespace().collect::<Vec<_>>().join(" ");
            let said: String = said.chars().take(120).collect();
            return Err(format!("{} {said}", reply.status).trim_end().to_string());
        }
        if let Some(msg) = envelope_error(&body) {
            return Err(msg);
        }
        Ok(body.get("data").cloned().unwrap_or(body))
    };
    tokio::time::timeout(PATIENCE, call)
        .await
        .unwrap_or_else(|_| Err(format!("no answer in {}s", PATIENCE.as_secs())))
}

// ---- how a number is written ----------------------------------------------------

/// `1234567` as `1,234,567`.
pub fn int(n: u64) -> String {
    let digits = n.to_string();
    let groups: Vec<&str> = digits.as_bytes().rchunks(3).rev().filter_map(|g| std::str::from_utf8(g).ok()).collect();
    groups.join(",")
}

/// Dollars: two places, five under a cent so a small amount is not rounded to nothing.
pub fn usd(v: f64) -> String {
    if v != 0.0 && v.abs() < 0.01 {
        format!("${v:.5}")
    } else {
        format!("${v:.2}")
    }
}

/// A fraction as a percent.
pub fn pct(f: f64) -> String {
    format!("{:.1}%", f * 100.0)
}

/// `42s`, `14m 02s`, `1h 03m`.
pub fn span(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn list() -> Value {
        json!({"data": [
            {"id": "anthropic/claude-sonnet-4.5", "free": false, "pricing": {
                "prompt": "0.000003165", "completion": "0.000015825",
                "rates": {"input_cache_read": "0.0000003165", "input_cache_write": "0.00000395625"}}},
            {"id": "enso-auto", "free": true, "pricing": {"prompt": "0", "completion": "0"}},
            {"id": "zen-scribe", "pricing": null},
            {"id": "gpt-5", "pricing": {"prompt": "0.00000125", "completion": "0.00001"}}
        ]})
    }

    #[test]
    fn the_list_prices_a_model_by_any_spelling_a_client_uses() {
        let book = Book::parse(&list()).unwrap();
        for name in [
            "anthropic/claude-sonnet-4.5",
            "claude-sonnet-4-5",
            "claude-sonnet-4-5-20250929",
            "Claude-Sonnet-4-5-2025-09-29",
        ] {
            let (id, p) = book.price(name).unwrap_or_else(|| panic!("{name} unpriced"));
            assert_eq!(id, "anthropic/claude-sonnet-4.5", "{name}");
            assert_eq!(p.cache_read, Some(0.0000003165));
        }
        assert!(book.price("zen-scribe").is_none(), "a row with no price is not priced at zero");
        assert!(book.price("no-such-model").is_none());
        let (_, free) = book.price("enso-auto").unwrap();
        assert!(free.free && free.input == 0.0, "a free model is priced at zero on purpose");
        assert!(!book.price("gpt-5").unwrap().1.free);
        assert!(Book::parse(&json!({"data": []})).is_none());
        assert!(Book::parse(&json!({})).is_none());
    }

    #[test]
    fn tokens_are_priced_the_way_the_gateway_prices_them() {
        let p = Price { input: 3.0e-6, output: 15.0e-6, cache_read: Some(0.3e-6), cache_write: Some(3.75e-6), free: false };
        // 1,000 in: 600 read from the cache, 100 written, 300 plain; 200 out.
        let t = Tally { input: 1000, output: 200, cached: 600, written: 100 };
        let want = 300.0 * 3.0e-6 + 600.0 * 0.3e-6 + 100.0 * 3.75e-6 + 200.0 * 15.0e-6;
        assert!((t.cost(&p) - want).abs() < 1e-12);
        let saved = t.saved(&p).unwrap();
        assert!((saved - 600.0 * (3.0e-6 - 0.3e-6)).abs() < 1e-12);
        assert_eq!(t.share(), Some(0.6));
        let bare = Price { cache_read: None, cache_write: None, ..p };
        assert!(t.saved(&bare).is_none(), "no cache-read rate, no claimed saving");
        assert!((t.cost(&bare) - (1000.0 * 3.0e-6 + 200.0 * 15.0e-6)).abs() < 1e-12, "no cache rate prices at the input rate");
        assert_eq!(Tally::default().share(), None);
    }

    /// The numbers a running kai-gateway computed for two Claude Sonnet 4.5 calls
    /// (190,000 in of which 150,000 read from the cache and 5,000 written, 1,600
    /// out) at the list's rates: spend $0.20335125, cache saved $0.427275.
    #[test]
    fn the_same_tokens_come_to_what_the_gateway_computed() {
        let p = Price { input: 3.165e-6, output: 15.825e-6, cache_read: Some(3.165e-7), cache_write: Some(3.95625e-6), free: false };
        let t = Tally { input: 190_000, output: 1_600, cached: 150_000, written: 5_000 };
        assert!((t.cost(&p) - 0.20335125).abs() < 1e-9, "{}", t.cost(&p));
        assert!((t.saved(&p).unwrap() - 0.427275).abs() < 1e-9);
    }

    #[test]
    fn the_cloud_meter_reads_what_it_answers_and_nothing_else() {
        let today = json!({"llm": {"available": true, "requests": 41, "promptTokens": 412310,
            "completionTokens": 18204, "costCents": 191.0, "tokens": 430514}});
        let l = Llm::of(&today);
        assert_eq!(l.requests, Some(41));
        assert!(!l.caches(), "fields the cloud has not added are absent, not zero");
        let rows = l.rows();
        assert_eq!(rows[0], ("calls", "41".into()));
        assert_eq!(rows[1], ("tokens", "in 412,310 · out 18,204".into()));
        assert_eq!(rows[2], ("cost", "$1.91 metered".into()));
        assert_eq!(rows.len(), 3);

        let later = json!({"llm": {"requests": 1, "cacheReadTokens": 354000, "cacheWriteTokens": 12000,
            "cachedShare": 0.86, "cacheSavedCents": 40.0,
            "kai": {"decisions": 12, "inputTokens": 5200, "costCents": 3.5, "p50": 38.25, "nested": {"x": 1}}}});
        let l = Llm::of(&later);
        assert!(l.caches());
        let rows = l.rows();
        assert!(rows.contains(&("cached", "86.0% of input · read 354,000 · written 12,000".into())), "{rows:?}");
        assert!(rows.contains(&("cache saved", "$0.40".into())));
        assert!(rows.contains(&("kai", "cost $0.04 · decisions 12 · input tokens 5,200 · p50 38.25".into())), "{rows:?}");

        // A field at the top of the document is read when `llm` does not carry it.
        let top = json!({"llm": {"requests": 2}, "cachedShare": 0.5, "kai": {"decisions": 3}});
        let l = Llm::of(&top);
        assert_eq!(l.share, Some(0.5));
        assert_eq!(l.kai, vec![("decisions".to_string(), "3".to_string())]);
    }

    #[test]
    fn linked_plans_are_listed_beside_hanzo_usage() {
        let doc = json!({"accounts": {"rows": [
            {"source": "account", "scope": "user", "provider": "anthropic", "window": "5h", "usedPct": 62.5, "confidence": "high"},
            {"source": "hanzo", "scope": "org", "provider": "hanzo", "window": "24h", "requests": 41, "tokens": 430514, "costCents": 191}
        ]}});
        let rows = Plan::all(&doc);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].provider.as_str(), rows[0].used), ("anthropic", Some(62.5)));
        assert_eq!((rows[1].requests, rows[1].used), (Some(41), None));
        assert!(Plan::all(&json!({})).is_empty());
    }

    #[test]
    fn numbers_are_written_one_way() {
        assert_eq!(int(0), "0");
        assert_eq!(int(999), "999");
        assert_eq!(int(1000), "1,000");
        assert_eq!(int(412310), "412,310");
        assert_eq!(int(1234567890), "1,234,567,890");
        assert_eq!(usd(1.839), "$1.84");
        assert_eq!(usd(0.0), "$0.00");
        assert_eq!(usd(0.00123), "$0.00123");
        assert_eq!(pct(0.8612), "86.1%");
        assert_eq!(span(Duration::from_secs(42)), "42s");
        assert_eq!(span(Duration::from_secs(842)), "14m 02s");
        assert_eq!(span(Duration::from_secs(3780)), "1h 03m");
        assert_eq!(words("inputTokens"), "input tokens");
        assert_eq!(words("costCents"), "cost");
    }
}
