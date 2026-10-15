//! The block `hanzo dev` prints when a session ends: how long it ran, what it
//! used, what that comes to, and where to see it in the cloud.
//!
//! Every figure says where it came from. **Reported** is what the backend's own
//! stream disclosed (headless runs only: an interactive run is the backend's own
//! screen, and nothing of its model traffic passes through here). **Listed price**
//! is those tokens at the rates `GET /v1/models` lists — an estimate, not a charge.
//! **Metered** is the cloud's own count of what the org sent through api.hanzo.ai
//! in the session's time window, for a routed run whose backend reported nothing;
//! it includes any other session of the org running in that window. A run on the
//! backend's own account (`--no-route`, Cursor, Antigravity) is not metered by
//! Hanzo, and the block says so.

use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use tokio::task::JoinHandle;

use super::backend::{Route, Routing};
use super::event::Usage;
use crate::commands::savings::{self, int, pct, span, usd, Book, Llm, Tally};

/// Where the cloud shows a session.
pub fn url(id: &str) -> String {
    format!("https://hanzo.bot/sessions/{id}")
}

/// Everything the block is written from.
pub struct Ended<'a> {
    pub backend: &'a str,
    pub took: Duration,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub usage: &'a Usage,
    /// The backend counts cache tokens apart from input (Claude); the others count
    /// them inside it.
    pub apart: bool,
    /// The gateway model the run named, when its calls went through api.hanzo.ai.
    pub routed: Option<&'a str>,
    pub session: Option<&'a str>,
    /// What the Hanzo toolset did ([`super::ledger`]), or why it did nothing.
    pub tools: Result<&'a [super::ledger::Entry], &'a str>,
}

impl Ended<'_> {
    /// The tokens the backend reported, or none when it reported no token count.
    fn tally(&self) -> Option<Tally> {
        let u = self.usage;
        if u.input_tokens.is_none() && u.output_tokens.is_none() {
            return None;
        }
        let read = u.cache_read_tokens.unwrap_or(0);
        let written = u.cache_write_tokens.unwrap_or(0);
        let base = u.input_tokens.unwrap_or(0);
        Some(Tally {
            input: if self.apart { base + read + written } else { base },
            output: u.output_tokens.unwrap_or(0),
            cached: read,
            written,
        })
    }
}

/// The cloud's count for the session window, asked only when the backend reported
/// nothing and the run was routed.
pub fn window(e: &Ended) -> Vec<String> {
    let at = |t: DateTime<Utc>| t.to_rfc3339_opts(SecondsFormat::Secs, true);
    vec!["range=custom".into(), format!("start={}", at(e.from)), format!("end={}", at(e.to))]
}

/// Whether the cloud should be asked at all: a routed run that reported no tokens.
pub fn asks(e: &Ended) -> bool {
    e.routed.is_some() && e.tally().is_none()
}

/// The block, one line per fact, labelled. `book` prices the reported tokens;
/// `meter` is the cloud's answer for the window, when it was asked.
pub fn lines(e: &Ended, book: Option<&Book>, meter: Option<&Result<serde_json::Value, String>>) -> Vec<String> {
    let mut head = format!("{} · {}", e.backend, span(e.took));
    if let Some(turns) = e.usage.num_turns {
        head.push_str(&format!(" · {turns} turns"));
    }
    let mut out = vec![row("session", &head)];

    match e.tally() {
        Some(t) => {
            let mut tokens = format!("in {} · out {}", int(t.input), int(t.output));
            if let Some(s) = t.share() {
                tokens.push_str(&format!(" · {} of input cached", pct(s)));
            }
            out.push(row("tokens", &format!("{tokens}   (reported by {})", e.backend)));
            out.extend(cost(e, &t, book));
        }
        None => out.extend(metered(e, meter)),
    }

    match e.tools {
        Ok(entries) => out.extend(super::ledger::rows(entries).iter().map(|(k, v)| row(k, v))),
        Err(why) => out.push(row("hanzo tools", why)),
    }
    if let Some(id) = e.session {
        out.push(row("dashboard", &url(id)));
    }
    out.push(row("more", "hanzo console (6: Savings) · hanzo usage summary"));
    out
}

/// What the reported tokens come to: at the model's listed price when the run was
/// routed and the list names it, else the backend's own figure if it gave one.
fn cost(e: &Ended, t: &Tally, book: Option<&Book>) -> Vec<String> {
    let mut out = Vec::new();
    let priced = e.routed.and_then(|m| book?.price(m));
    match (&priced, e.usage.total_cost_usd) {
        (Some((id, p)), _) => {
            let note = if p.free { "listed as free" } else { "estimate: tokens at the list" };
            out.push(row("cost", &format!("{} at the listed price of {id}   ({note})", usd(t.cost(p)))));
            if let Some(saved) = t.saved(p) {
                out.push(row("cache", &format!("{} cheaper than full-price input at that price   (estimate)", usd(saved))));
            }
        }
        (None, Some(reported)) => {
            out.push(row("cost", &format!("{} as {} reports it   (its own figure, not priced here)", usd(reported), e.backend)));
        }
        (None, None) => {
            let why = match e.routed {
                Some(m) => format!("{m} is not on the price list"),
                None => format!("{} ran on its own account; Hanzo lists no price for it", e.backend),
            };
            out.push(row("cost", &format!("not priced: {why}")));
        }
    }
    out
}

/// The cloud's own count for a routed run whose backend reported nothing; for any
/// other run, the plain statement that Hanzo did not see its tokens.
fn metered(e: &Ended, meter: Option<&Result<serde_json::Value, String>>) -> Vec<String> {
    if e.routed.is_none() {
        return vec![row("model", &format!("{} on its own account; Hanzo bills only what its tools handed off", e.backend))];
    }
    let window = format!(
        "{}–{} UTC",
        e.from.format("%H:%M"),
        e.to.format("%H:%M")
    );
    match meter {
        Some(Ok(doc)) => {
            let llm = Llm::of(doc);
            if llm.available == Some(false) || llm.rows().is_empty() {
                return vec![row("usage", &format!("api.hanzo.ai has metered nothing for your org yet ({window}); the meter can trail a session"))];
            }
            let mut out: Vec<String> = llm.rows().iter().map(|(k, v)| row(k, v)).collect();
            out.push(row(
                "basis",
                &format!("metered by api.hanzo.ai for your org, {window}; includes any other session in that window"),
            ));
            out
        }
        Some(Err(why)) => vec![row("usage", &format!("the api.hanzo.ai meter did not answer: {why}"))],
        None => vec![row("usage", "not asked: this run's backend reported no tokens, and the meter is read at a terminal")],
    }
}

fn row(label: &str, value: &str) -> String {
    format!("  {label:<12}{value}")
}

/// The block is printed after the agent's work is done, and the process does not end
/// until it is: it waits this long for the price list, and for the cloud's meter.
const PRICES_WAIT: Duration = Duration::from_secs(1);
const METER_WAIT: Duration = Duration::from_millis(1500);

/// A session's clock and, on a routed run, the price list read while it ran.
pub struct Run {
    from: DateTime<Utc>,
    at: Instant,
    routed: Option<String>,
    book: Option<JoinHandle<Option<Book>>>,
}

impl Run {
    /// Start the clock. A route through the Hanzo gateway names the model and reads
    /// the price list in the background, so the block does not wait on it.
    pub fn begin(route: &Route) -> Run {
        let (routed, book) = match route.via() {
            Some(Routing::Gateway { api, model, .. }) => {
                let api = api.clone();
                (Some(model.clone()), Some(tokio::spawn(async move { savings::book(&api).await })))
            }
            _ => (None, None),
        };
        Run { from: Utc::now(), at: Instant::now(), routed, book }
    }

    /// Print the block. The cloud meter is asked only when the block needs it
    /// ([`asks`]); the price list is waited for a moment. Either failing leaves its
    /// figure out, never the block.
    pub async fn end(
        self,
        cfg: &mut crate::config::Config,
        backend: &str,
        apart: bool,
        usage: &Usage,
        session: Option<&str>,
        tools: Result<&std::path::Path, &str>,
    ) {
        use colored::Colorize;
        let entries = tools.map(super::ledger::read);
        let e = Ended {
            backend,
            took: self.at.elapsed(),
            from: self.from,
            to: Utc::now(),
            usage,
            apart,
            routed: self.routed.as_deref(),
            session,
            tools: entries.as_ref().map(Vec::as_slice).map_err(|w| *w),
        };
        // The cloud is asked only for a person at a terminal. A script reads what the agent printed, and the
        // process should end when the agent does: its block carries what the run reported and nothing it
        // has to wait for.
        let person = std::io::IsTerminal::is_terminal(&std::io::stdout());
        let book = match self.book {
            Some(handle) if person || e.tally().is_some() => tokio::time::timeout(PRICES_WAIT, handle).await.ok().and_then(Result::ok).flatten(),
            _ => None,
        };
        let meter = if person && asks(&e) {
            Some(match tokio::time::timeout(METER_WAIT, savings::summary(cfg, &window(&e))).await {
                Ok(m) => m,
                Err(_) => Err(format!("the cloud did not answer in {}s", METER_WAIT.as_secs_f32())),
            })
        } else {
            None
        };
        println!();
        println!("{}", "hanzo dev · session summary".bold());
        for line in lines(&e, book.as_ref(), meter.as_ref()) {
            println!("{}", line.dimmed());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-10-11T{h:02}:{m:02}:00Z")).unwrap().with_timezone(&Utc)
    }

    fn book() -> Book {
        Book::parse(&json!({"data": [
            {"id": "anthropic/claude-sonnet-4.5", "free": false, "pricing": {
                "prompt": "0.000003", "completion": "0.000015",
                "rates": {"input_cache_read": "0.0000003", "input_cache_write": "0.00000375"}}},
            {"id": "enso-auto", "free": true, "pricing": {"prompt": "0", "completion": "0"}}
        ]}))
        .unwrap()
    }

    fn ended<'a>(usage: &'a Usage, routed: Option<&'a str>) -> Ended<'a> {
        Ended {
            backend: "claude",
            took: Duration::from_secs(842),
            from: at(9, 12),
            to: at(9, 26),
            usage,
            apart: true,
            routed,
            session: Some("sess_abc"),
            tools: Err("not attached (`--no-mcp`, or hanzo-mcp is not installed)"),
        }
    }

    fn reported() -> Usage {
        Usage {
            input_tokens: Some(100),
            output_tokens: Some(2000),
            cache_read_tokens: Some(6000),
            cache_write_tokens: Some(1000),
            num_turns: Some(23),
            ..Default::default()
        }
    }

    fn text(lines: &[String]) -> String {
        lines.join("\n")
    }

    #[test]
    fn reported_tokens_are_priced_at_the_list_and_labelled_an_estimate() {
        let u = reported();
        let out = text(&lines(&ended(&u, Some("claude-sonnet-4-5")), Some(&book()), None));
        // Claude counts cache apart: 100 + 6,000 + 1,000 read in all, 6,000 of them cached.
        assert!(out.contains("claude · 14m 02s · 23 turns"), "{out}");
        assert!(out.contains("in 7,100 · out 2,000 · 84.5% of input cached   (reported by claude)"), "{out}");
        // 100 plain + 6,000 cached + 1,000 written + 2,000 out at the list.
        let want = 100.0 * 3.0e-6 + 6000.0 * 0.3e-6 + 1000.0 * 3.75e-6 + 2000.0 * 15.0e-6;
        assert!(out.contains(&format!("{} at the listed price of anthropic/claude-sonnet-4.5   (estimate: tokens at the list)", usd(want))), "{out}");
        assert!(out.contains(&format!("{} cheaper than full-price input at that price   (estimate)", usd(6000.0 * 2.7e-6))), "{out}");
        assert!(out.contains("https://hanzo.bot/sessions/sess_abc"), "{out}");
    }

    #[test]
    fn a_codex_style_report_counts_cache_inside_input() {
        let u = Usage { input_tokens: Some(8000), output_tokens: Some(500), cache_read_tokens: Some(6000), ..Default::default() };
        let mut e = ended(&u, None);
        e.backend = "dev";
        e.apart = false;
        let out = text(&lines(&e, None, None));
        assert!(out.contains("in 8,000 · out 500 · 75.0% of input cached   (reported by dev)"), "{out}");
    }

    #[test]
    fn a_free_listed_model_is_zero_and_says_why() {
        let u = reported();
        let out = text(&lines(&ended(&u, Some("enso-auto")), Some(&book()), None));
        assert!(out.contains("$0.00 at the listed price of enso-auto   (listed as free)"), "{out}");
    }

    #[test]
    fn what_cannot_be_priced_is_not_priced() {
        let u = reported();
        let out = text(&lines(&ended(&u, Some("mystery-1")), Some(&book()), None));
        assert!(out.contains("not priced: mystery-1 is not on the price list"), "{out}");
        let out = text(&lines(&ended(&u, Some("claude-sonnet-4-5")), None, None));
        assert!(out.contains("not priced: claude-sonnet-4-5 is not on the price list"), "no list, no dollars: {out}");
        // The backend's own figure is shown as its own.
        let own = Usage { total_cost_usd: Some(0.42), ..reported() };
        let out = text(&lines(&ended(&own, None), None, None));
        assert!(out.contains("$0.42 as claude reports it   (its own figure, not priced here)"), "{out}");
        assert!(!asks(&ended(&own, None)), "a run with a report asks the cloud for nothing");
    }

    #[test]
    fn a_routed_run_that_reported_nothing_reads_the_cloud_meter_for_its_window() {
        let none = Usage::default();
        let e = ended(&none, Some("enso-auto"));
        assert!(asks(&e));
        assert_eq!(
            window(&e),
            ["range=custom", "start=2026-10-11T09:12:00Z", "end=2026-10-11T09:26:00Z"]
        );
        let doc = Ok(json!({"llm": {"requests": 41, "promptTokens": 412310, "completionTokens": 18204, "costCents": 191.0}}));
        let out = text(&lines(&e, None, Some(&doc)));
        assert!(out.contains(&row("calls", "41")), "{out}");
        assert!(out.contains(&row("tokens", "in 412,310 · out 18,204")), "{out}");
        assert!(out.contains(&row("cost", "$1.91 metered")), "{out}");
        assert!(out.contains("metered by api.hanzo.ai for your org, 09:12–09:26 UTC; includes any other session in that window"), "{out}");
        assert!(!out.contains("cached"), "cache fields the cloud has not sent are not invented: {out}");

        let later = Ok(json!({"llm": {"requests": 1, "cachedShare": 0.86, "cacheSavedCents": 40.0}}));
        let out = text(&lines(&e, None, Some(&later)));
        assert!(out.contains(&row("cached", "86.0% of input")), "{out}");
        assert!(out.contains(&row("cache saved", "$0.40")), "{out}");
    }

    #[test]
    fn a_meter_that_cannot_answer_is_said_not_zeroed() {
        let none = Usage::default();
        let e = ended(&none, Some("enso-auto"));
        let out = text(&lines(&e, None, Some(&Err("403 not allowed".into()))));
        assert!(out.contains("the api.hanzo.ai meter did not answer: 403 not allowed"), "{out}");
        let dry = Ok(json!({"llm": {"available": false, "requests": 0}}));
        let out = text(&lines(&e, None, Some(&dry)));
        assert!(out.contains("has metered nothing for your org yet"), "{out}");
    }

    #[test]
    fn a_run_on_its_own_account_is_not_metered_and_says_so() {
        let none = Usage::default();
        let mut e = ended(&none, None);
        e.backend = "cursor";
        e.session = None;
        let out = text(&lines(&e, None, None));
        assert!(out.contains("cursor on its own account; Hanzo bills only what its tools handed off"), "{out}");
        assert!(out.contains("hanzo tools not attached"), "{out}");
        assert!(!out.contains("dashboard"), "an unlinked run has no session to link: {out}");
        assert!(!asks(&e));
    }

    #[test]
    fn the_block_says_what_the_toolset_handed_off() {
        let none = Usage::default();
        let calls = [
            super::super::ledger::Entry { tool: "llm".into(), ms: 900, ok: true, model: Some("enso-auto".into()), input: Some(110), output: Some(29) },
            super::super::ledger::Entry { tool: "kai_decide".into(), ms: 53, ok: true, model: Some("kai".into()), input: Some(11), output: Some(0) },
        ];
        let mut e = ended(&none, None);
        e.tools = Ok(&calls);
        let out = text(&lines(&e, None, None));
        assert!(out.contains("hanzo tools 2 calls"), "{out}");
        assert!(out.contains("1 call handed to enso-auto · in 110 · out 29"), "{out}");
        assert!(out.contains("1 call handed to kai"), "{out}");
    }

    #[test]
    fn the_block_claims_no_speedup() {
        let u = reported();
        let out = text(&lines(&ended(&u, Some("claude-sonnet-4-5")), Some(&book()), None)).to_lowercase();
        for word in ["faster", "speedup", "accelerat", "saved you", "x faster"] {
            assert!(!out.contains(word), "{word}: {out}");
        }
    }
}
