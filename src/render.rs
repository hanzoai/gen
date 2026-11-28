//! Terminal banner, status lines, and live event rendering.

use colored::*;
use serde_json::Value;
use std::path::Path;

use crate::backend::{BackendKind, Routing};
use crate::event::Kind;
use crate::{summary, Options};

pub(crate) fn banner(
    opts: &Options,
    backend: &str,
    cwd: &Path,
    api: &str,
    routing: Option<&Routing>,
    signed_in: bool,
    session: Option<&str>,
) {
    let _ = api; // the route line carries its own host
    println!(
        "{} {} · {} · {}",
        "hanzo dev".bold(),
        backend.cyan(),
        cwd.display().to_string().dimmed(),
        opts.resume.as_deref().map(|_| "resume").unwrap_or("start").dimmed(),
    );
    let (route_line, stream_line) = status_lines(opts, routing, signed_in, session);
    let route_line = if routing.is_some() { route_line.green() } else { route_line.dimmed() };
    let stream_line = if session.is_some() { stream_line.green() } else { stream_line.dimmed() };
    println!("  {route_line}");
    println!("  {stream_line}");
}

/// The two status lines — model-routing and session-stream — as PLAIN text.
/// Kept separate from `banner` (which colors + prints) so the wording is
/// unit-testable and stays honest.
///
/// The two are INDEPENDENT: routing decides where prompts + code + tool output
/// go for inference (the gateway, or a provider's own API), streaming decides
/// whether the session is mirrored to mission-control. "off" on one says nothing
/// about the other — so an unlinked run must never imply "local only" while
/// routing still ships code somewhere.
fn status_lines(
    opts: &Options,
    routing: Option<&Routing>,
    signed_in: bool,
    session: Option<&str>,
) -> (String, String) {
    let strip = |u: &str| u.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string();
    let effort_suffix = opts
        .effort
        .filter(|e| *e != crate::reasoning::ReasoningEffort::Auto)
        .map(|e| format!(" · effort {}", e.as_str()))
        .unwrap_or_default();
    let route = match routing {
        Some(Routing::Gateway { api, model, .. }) => {
            let display = crate::enso::format_model_id(model);
            format!(
                "model routing: Hanzo cloud → {} · {display}{effort_suffix} (prompts + code go here; usage metered to your org)",
                strip(api)
            )
        }
        Some(Routing::Anthropic { .. }) => {
            format!("model routing: on → api.anthropic.com{effort_suffix} (your Anthropic key; usage billed by Anthropic)")
        }
        Some(Routing::OpenAI { .. }) => {
            format!("model routing: on → api.openai.com{effort_suffix} (your OpenAI key; usage billed by OpenAI)")
        }
        None if opts.backend == BackendKind::Dev && !signed_in => {
            "model routing: off (sign in with `hanzo auth login` to route + meter model calls)".to_string()
        }
        None => format!("model routing: your account, direct{effort_suffix} (code stays with your provider)"),
    };
    let stream = match session {
        Some(id) => format!("session stream: on → {}", summary::url(id)),
        None => "session stream: off (this session is not mirrored to cloud; pass --link to stream it)".to_string(),
    };
    (route, stream)
}

pub(crate) fn report_link(id: &str) {
    // A COPY-PASTEABLE command and nothing else: `hanzo --resume <id>` now parses at
    // the top level (a bare-`hanzo` code flag — see `main::Cli`), so the whole line is
    // a valid invocation with no label prefix to accidentally copy. Bare id (no `sess_`).
    let short = id.strip_prefix("sess_").unwrap_or(id);
    println!("{}", format!("hanzo --resume {short}").magenta());
}

pub(crate) fn render_event(kind: Kind, payload: &Value) {
    match kind {
        Kind::Message => {
            if let Some(t) = payload.get("text").and_then(Value::as_str) {
                if payload.get("role").and_then(Value::as_str) == Some("assistant") {
                    println!("{t}");
                }
            }
        }
        Kind::ToolCall => {
            let name = payload.get("name").and_then(Value::as_str).unwrap_or("tool");
            let brief = payload
                .get("input")
                .map(one_line)
                .unwrap_or_default();
            println!("{}", format!("→ {name} {brief}").dimmed());
        }
        Kind::Spawn => {
            let a = payload.get("agent").and_then(Value::as_str).unwrap_or("agent");
            println!("{}", format!("⇒ spawn {a}").dimmed());
        }
        Kind::Log => {
            match payload.get("type").and_then(Value::as_str) {
                Some("tool-result") => {
                    let n = payload.get("output").and_then(Value::as_str).map(|s| s.len()).unwrap_or(0);
                    println!("{}", format!("← result ({n} bytes)").dimmed());
                }
                Some("reasoning") => {
                    if let Some(t) = payload.get("text").and_then(Value::as_str) {
                        println!("{}", format!("· {}", first_line(t)).dimmed());
                    }
                }
                _ => {
                    if let Some(t) = payload.get("text").and_then(Value::as_str) {
                        println!("{}", t.dimmed());
                    }
                }
            }
        }
        Kind::Status | Kind::Control => {}
    }
}

fn one_line(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    first_line(&s)
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.len() > 100 {
        format!("{}…", &line[..100])
    } else {
        line.to_string()
    }
}

pub(crate) fn session_title(opts: &Options) -> String {
    match &opts.task {
        Some(t) => t.chars().take(120).collect(),
        None => "interactive coding session".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_separates_model_routing_from_session_stream() {
        let opts = Options {
            backend: BackendKind::Claude,
            link: false,
            no_link: true,
            cloud: true,
            mcp: true,
            project_mcp: false,
            ask: false,
            no_sandbox: false,
            resume: None,
            brand: "hanzo".into(),
            task: None,
            theme: None,
            model: None,
            effort: None,
            passthrough: vec![],
            root: crate::root::scratch(),
        };
        // Routing ON (gateway), stream OFF — the case that regressed:
        // --no-link but model calls still ship code to the gateway.
        let gw = Routing::Gateway { api: "https://api.hanzo.ai".into(), token: "T".into(), org: None, model: "enso-auto".into(), small_fast_model: "enso-flash".into(), context_window: 1_000_000 };
        let (route, stream) = status_lines(&opts, Some(&gw), true, None);
        assert!(route.contains("model routing: Hanzo cloud"), "got: {route}");
        assert!(route.contains("api.hanzo.ai"));
        assert!(route.contains("○ enso-auto"), "gateway route must display progressive Enso glyph: {route}");
        assert!(route.contains("prompts + code"));
        assert!(stream.contains("session stream: off"), "got: {stream}");
        // Must NOT claim the run is "local only" while routing is on.
        assert!(!stream.to_lowercase().contains("local only"));
        assert!(!route.to_lowercase().contains("local only"));

        // The default says whose account it is, and is distinct from "off because
        // not signed in" (which only `dev` can be).
        let mut o2 = opts;
        o2.cloud = false;
        let (route2, _) = status_lines(&o2, None, true, None);
        assert!(route2.contains("model routing: your account, direct"), "got: {route2}");
        o2.backend = BackendKind::Dev;
        let (signed_out, _) = status_lines(&o2, None, false, None);
        assert!(signed_out.contains("hanzo auth login"), "got: {signed_out}");
        o2.backend = BackendKind::Claude;

        // Stream ON names the session id and mission-control, not "link".
        let (_, stream_on) = status_lines(&o2, None, true, Some("sess_x"));
        assert!(stream_on.contains("session stream: on"));
        assert!(stream_on.contains("sess_x"));
        // The viewer is hanzo.ai/tabs/:id.
        assert!(stream_on.contains("https://hanzo.ai/tabs/sess_x"), "got: {stream_on}");
    }

    /// A direct provider route names the VENDOR endpoint + who bills — never the
    /// gateway, so the user is never misled about where their code + money go.
    #[test]
    fn status_line_names_the_direct_provider_endpoint() {
        let opts = Options {
            backend: BackendKind::Claude,
            link: false,
            no_link: true,
            cloud: true,
            mcp: true,
            project_mcp: false,
            ask: false,
            no_sandbox: false,
            resume: None,
            brand: "hanzo".into(),
            task: None,
            theme: None,
            model: None,
            effort: Some(crate::reasoning::ReasoningEffort::XHigh),
            passthrough: vec![],
            root: crate::root::scratch(),
        };
        let anthropic = Routing::Anthropic { key: "sk-ant-x".into() };
        let (route, _) = status_lines(&opts, Some(&anthropic), true, None);
        assert!(route.contains("model routing: on"), "got: {route}");
        assert!(route.contains("api.anthropic.com"), "got: {route}");
        assert!(route.contains("effort xhigh"), "got: {route}");
        assert!(route.contains("billed by Anthropic"), "got: {route}");
        assert!(!route.contains("api.hanzo.ai"), "a direct route must NOT claim the gateway");
        // The key never appears in the human-facing line.
        assert!(!route.contains("sk-ant-x"));

        let openai = Routing::OpenAI { key: "sk-x".into() };
        let (route, _) = status_lines(&opts, Some(&openai), true, None);
        assert!(route.contains("api.openai.com") && route.contains("billed by OpenAI"), "got: {route}");
        assert!(route.contains("effort xhigh"), "got: {route}");

        let ultra = Routing::Gateway {
            api: "https://api.hanzo.ai".into(),
            token: "T".into(),
            org: None,
            model: "enso-ultra".into(),
            small_fast_model: "enso-flash".into(),
            context_window: 1_000_000,
        };
        let (route, _) = status_lines(&opts, Some(&ultra), true, None);
        assert!(route.contains("◕ enso-ultra") && route.contains("effort xhigh"), "got: {route}");
    }

}
