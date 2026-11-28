//! Model routing, approval precedence, and gateway model resolution.

use anyhow::Result;

use crate::backend::{Approval, BackendKind, Route, Routing};
use crate::host::{Host, Provider};
use crate::settings::Settings;

/// A credential source to try for a routed run, in preference order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cred {
    /// The active identity's hanzo.id bearer — already in hand, no Vault read.
    Bearer,
    /// A stored `sk-` Hanzo gateway key.
    HanzoKey,
    /// A stored `sk-ant-` Anthropic key (direct to api.anthropic.com).
    AnthropicKey,
    /// A stored `sk-` OpenAI key (direct to api.openai.com).
    OpenAIKey,
}

/// The gateway's default coding + small/fast models. Claude Code's built-in
/// default (`claude-fable-5`) is NOT in the gateway catalog, so a routed run that
/// names no model 400s ("model … is not available"); `dev`/codex's built-in
/// default is likewise absent. We name Hanzo's own models, which the catalog
/// carries as bare ids — so a default `hanzo dev` works with no per-machine env.
/// `enso-auto` is the default; `enso-ultra` is the adaptive fan-out a user can pick
/// with `--model enso-ultra`. Overridable in `~/.hanzo/settings.json` (see [`Settings`]).
///
/// It was `enso` until the family was renamed so every member states its role —
/// auto, flash, pro, max, ultra — leaving no unsuffixed id. The old name still serves
/// during the cutover, which matters more here than anywhere else: this constant is
/// COMPILED IN, so every already-installed binary keeps asking for `enso` and no
/// deploy of ours can reach them. The catalog's transitional entry may not be
/// removed until those have aged out.
/// The longest a linked run keeps trying to register its session before it fails.
pub(crate) const DEFAULT_MODEL: &str = "enso-auto";
pub(crate) const DEFAULT_SMALL_FAST_MODEL: &str = "enso-flash";

/// The context window `hanzo dev` requests on the gateway route by default: the
/// real 1M window Hanzo's frontier models serve. A coding backend pointed at a
/// custom gateway can't verify this and self-clamps to the standard 200K, so the
/// gateway route NAMES the window (Claude `[1m]` suffix, `dev` `model_catalog_json`).
/// Overridable in `~/.hanzo/settings.json` `contextWindow`; below the standard
/// window it's a no-op (opting out of the extended window).
pub(crate) const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;

/// Resolve the auto-approve decision by precedence: an explicit CLI flag wins,
/// then `~/.hanzo/settings.json` `autoApprove`, then the built-in default (ON).
/// PURE + testable; the impure settings read lives in [`run`].
///
/// `--no-sandbox` (escalate) and `--ask`/`--safe` (opt out) are mutually exclusive
/// at the clap layer, so at most one is set. The persisted `autoApprove` can turn
/// the default OFF but can NEVER reach `Bypass`: dropping the sandbox is always a
/// deliberate per-invocation `--no-sandbox`, never a stored default (fail-secure).
pub(crate) fn resolve_approval(ask: bool, no_sandbox: bool, settings: Option<bool>) -> Approval {
    if no_sandbox {
        Approval::Bypass
    } else if ask {
        Approval::Ask
    } else if settings.unwrap_or(true) {
        Approval::Auto
    } else {
        Approval::Ask
    }
}

/// Resolve one gateway model id by precedence — the first NON-EMPTY of: an explicit
/// `--model` flag, the user's own exported env, the `~/.hanzo/settings.json` value,
/// else the built-in default. PURE + testable; the impure reads live in [`run`].
///
/// No allowlist: the gateway is the sole authority on validity, so a bad id simply
/// 400s with the gateway's own message rather than being rejected client-side.
pub(crate) fn resolve_model(flag: Option<&str>, user_env: Option<&str>, settings: Option<&str>, default: &str) -> String {
    let picked = [flag, user_env, settings]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or(default);
    crate::enso::normalize_enso_model(picked).to_string()
}

/// Resolve the reasoning effort level by precedence: an explicit `--effort`
/// flag wins, then `HANZO_REASONING_EFFORT`, then `~/.hanzo/settings.json`
/// `effort` / `reasoningEffort`. Unknown or empty values are ignored rather
/// than crashing startup.
pub(crate) fn resolve_effort(
    flag: Option<crate::reasoning::ReasoningEffort>,
    user_env: Option<&str>,
    settings: Option<&str>,
) -> Option<crate::reasoning::ReasoningEffort> {
    if flag.is_some() {
        return flag;
    }
    [user_env, settings]
        .into_iter()
        .flatten()
        .find_map(crate::reasoning::ReasoningEffort::parse)
}

/// The resolved gateway model selection for a run — carried into `Routing::Gateway`
/// so the model mapping lives in exactly ONE place and can never reach a direct route.
pub(crate) struct GatewayModels {
    pub model: String,
    pub small_fast_model: String,
    pub context_window: u64,
}

/// Resolve both gateway models once, reading the user's exported env and the
/// native `~/.hanzo/settings.json`. The `ANTHROPIC_*` vars are honored ONLY for
/// the Claude backend, whose model auth reads them; `dev`/codex reads neither, so
/// its model is `--model` > settings > built-in default (and it has no small/fast
/// model at all).
pub(crate) fn gateway_models(backend: BackendKind, model_flag: Option<&str>, settings: &Settings) -> GatewayModels {
    let claude = backend == BackendKind::Claude;
    let env = |k: &str| claude.then(|| std::env::var(k).ok()).flatten();
    GatewayModels {
        model: resolve_model(
            model_flag,
            env("ANTHROPIC_MODEL").as_deref(),
            settings.model.as_deref(),
            DEFAULT_MODEL,
        ),
        small_fast_model: resolve_model(
            None,
            env("ANTHROPIC_SMALL_FAST_MODEL").as_deref(),
            settings.small_fast_model.as_deref(),
            DEFAULT_SMALL_FAST_MODEL,
        ),
        context_window: settings.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
    }
}

/// The ordered credential preference for a routed run, from the active provider,
/// the backend, and whether a bearer is held. PURE + testable — the impure
/// resolver walks this and reads the Vault lazily, so the precedence lives in
/// exactly one place.
///
/// A DIRECT provider is tried ONLY when it matches the backend it can drive
/// (Anthropic↔Claude, OpenAI↔dev); the Hanzo gateway (bearer, then `sk-` key) is
/// always the fallback, so a signed-in user keeps routing even if a direct key
/// is absent or paired with the wrong backend.
fn route_plan(backend: BackendKind, provider: Option<&str>, has_bearer: bool) -> Vec<Cred> {
    let mut plan = Vec::new();
    match (provider, backend) {
        (Some("anthropic"), BackendKind::Claude) => plan.push(Cred::AnthropicKey),
        (Some("openai"), BackendKind::Dev) => plan.push(Cred::OpenAIKey),
        _ => {}
    }
    if has_bearer {
        plan.push(Cred::Bearer);
    }
    plan.push(Cred::HanzoKey);
    plan
}

/// Resolve the routing for this run by walking [`route_plan`] and taking the
/// first credential actually held. Provider keys are read from the Vault LAZILY
/// (only as the plan reaches them), so the common gateway path (bearer in hand)
/// does zero extra keychain reads, and a direct run does none at all.
///
/// Returns a [`Route`] — the three-way decision the backend needs to treat the
/// child's model-auth env correctly. A direct run ⇒ `Inherit`; a resolved
/// credential ⇒ `Via`; nothing resolved ⇒ [`unresolved_route`] (fail closed iff a
/// provider was selected, else inherit an unconfigured backend's own account).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_routing(
    host: &dyn Host,
    provider: Option<&str>,
    settings: &Settings,
    cloud: bool,
    backend: BackendKind,
    api: &str,
    bearer: Option<&str>,
    org: Option<&str>,
    model_flag: Option<&str>,
) -> Result<Route> {
    // The default: a third-party tool talks straight to the account it is signed
    // in to, and we touch no env. `dev` is ours, and its account IS Hanzo's.
    if !cloud && backend != BackendKind::Dev {
        return Ok(Route::Inherit);
    }
    // An agent with no gateway wire (Cursor, Antigravity, Gemini, …) always keeps
    // its own login, `--cloud` or not.
    if backend.own_account_only() {
        return Ok(Route::Inherit);
    }
    // The model rides ONLY the gateway route (resolved once here). Both gateway
    // credentials build the SAME variant, so one closure keeps the mapping DRY.
    let models = gateway_models(backend, model_flag, settings);
    let org_opt = org.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let gateway = |token: String| Routing::Gateway {
        api: api.to_string(),
        token,
        org: org_opt.clone(),
        model: models.model.clone(),
        small_fast_model: models.small_fast_model.clone(),
        context_window: models.context_window,
    };
    for cred in route_plan(backend, provider, bearer.is_some()) {
        match cred {
            Cred::Bearer => {
                if let Some(token) = bearer {
                    return Ok(Route::Via(gateway(token.to_string())));
                }
            }
            Cred::HanzoKey => {
                if let Some(token) = host.key(Provider::Hanzo)? {
                    return Ok(Route::Via(gateway(token)));
                }
            }
            Cred::AnthropicKey => {
                if let Some(key) = host.key(Provider::Anthropic)? {
                    return Ok(Route::Via(Routing::Anthropic { key }));
                }
            }
            Cred::OpenAIKey => {
                if let Some(key) = host.key(Provider::OpenAI)? {
                    return Ok(Route::Via(Routing::OpenAI { key }));
                }
            }
        }
    }
    Ok(unresolved_route(provider.is_some()))
}

/// The routing outcome when the credential plan resolves NOTHING.
///
/// A SELECTED provider means the user EXPECTS that route, so silently inheriting a
/// shell-set `ANTHROPIC_BASE_URL`/key would ship prompts+code somewhere they never
/// chose — fail CLOSED (the backend then clears its vendor's model-auth env). With
/// NO provider selected there is no such expectation (an unconfigured or
/// signed-out run), so the backend keeps its OWN account, exactly as a bare
/// `claude`/`dev` would — inheriting the shell is the honest, unchanged behavior.
pub(crate) fn unresolved_route(provider_selected: bool) -> Route {
    if provider_selected {
        Route::FailClosed
    } else {
        Route::Inherit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The routing precedence: a direct provider is preferred ONLY when it can
    /// drive the backend, and the gateway (bearer, then sk-) is always the tail.
    #[test]
    fn route_plan_prefers_a_matching_direct_provider_else_the_gateway() {
        use BackendKind::{Claude, Dev};
        // Anthropic + Claude → try the Anthropic key first, then gateway.
        assert_eq!(route_plan(Claude, Some("anthropic"), true), vec![Cred::AnthropicKey, Cred::Bearer, Cred::HanzoKey]);
        // OpenAI + dev → the OpenAI key first.
        assert_eq!(route_plan(Dev, Some("openai"), false), vec![Cred::OpenAIKey, Cred::HanzoKey]);
        // Mismatched pairing (OpenAI selected, Claude backend) → NO direct key,
        // fall straight to the gateway.
        assert_eq!(route_plan(Claude, Some("openai"), true), vec![Cred::Bearer, Cred::HanzoKey]);
        assert_eq!(route_plan(Dev, Some("anthropic"), false), vec![Cred::HanzoKey]);
        // No provider selected → the gateway, bearer preferred over a stored key.
        assert_eq!(route_plan(Claude, None, true), vec![Cred::Bearer, Cred::HanzoKey]);
        assert_eq!(route_plan(Claude, None, false), vec![Cred::HanzoKey]);
        // Explicit "hanzo" behaves like the gateway default.
        assert_eq!(route_plan(Claude, Some("hanzo"), true), vec![Cred::Bearer, Cred::HanzoKey]);
    }

    /// The model precedence — `--model` flag, then exported env, then
    /// `~/.hanzo/settings.json`, then the built-in default — skipping any EMPTY
    /// tier. This is the whole policy; the impure env read in `gateway_models` just
    /// supplies the middle argument.
    #[test]
    fn resolve_model_precedence() {
        // Flag wins over everything, even a set env + settings.
        assert_eq!(resolve_model(Some("enso-ultra"), Some("env"), Some("file"), "enso"), "enso-ultra");
        // No flag → the user's exported env is PRESERVED over settings + default.
        assert_eq!(resolve_model(None, Some("env-model"), Some("file"), "enso"), "env-model");
        // No flag, no env → the persisted settings value.
        assert_eq!(resolve_model(None, None, Some("file-model"), "enso"), "file-model");
        // Nothing set → the built-in default.
        assert_eq!(resolve_model(None, None, None, "enso"), "enso");
        // Empty / whitespace-only tiers name no model and are skipped.
        assert_eq!(resolve_model(Some("  "), Some(""), Some("file"), "enso"), "file");
        assert_eq!(resolve_model(Some(""), Some("  "), Some("  "), "enso"), "enso");
        // Retired `enso-free` normalizes to `enso-auto`.
        assert_eq!(resolve_model(Some("enso-free"), None, None, "enso-auto"), "enso-auto");
        assert_eq!(resolve_model(None, None, Some("hanzo/enso-free"), "enso-auto"), "enso-auto");
    }

    #[test]
    fn resolve_effort_precedence_and_aliases() {
        use crate::reasoning::ReasoningEffort;
        assert_eq!(resolve_effort(Some(ReasoningEffort::XHigh), Some("low"), Some("high")), Some(ReasoningEffort::XHigh));
        assert_eq!(resolve_effort(None, Some("none"), Some("high")), Some(ReasoningEffort::Off));
        assert_eq!(resolve_effort(None, Some("  "), Some("minimal")), Some(ReasoningEffort::Minimal));
        assert_eq!(resolve_effort(None, Some("unknown"), Some("max")), Some(ReasoningEffort::Max));
        assert_eq!(resolve_effort(None, None, None), None);
    }

    /// The auto-approve precedence: an explicit CLI flag wins, then the persisted
    /// `autoApprove`, then the built-in default (ON → `Auto`). The escalation
    /// (`--no-sandbox` → `Bypass`) and the opt-out (`--ask`/`--safe` → `Ask`) both
    /// beat the persisted setting; the setting can turn the default OFF but can
    /// NEVER reach `Bypass` — dropping the sandbox is always a per-invocation act.
    #[test]
    fn resolve_approval_precedence() {
        // No flags: the persisted setting decides, defaulting ON (Auto).
        assert_eq!(resolve_approval(false, false, None), Approval::Auto);
        assert_eq!(resolve_approval(false, false, Some(true)), Approval::Auto);
        assert_eq!(resolve_approval(false, false, Some(false)), Approval::Ask);
        // `--ask`/`--safe` opt out, winning over a persisted `true`.
        assert_eq!(resolve_approval(true, false, Some(true)), Approval::Ask);
        assert_eq!(resolve_approval(true, false, None), Approval::Ask);
        // `--no-sandbox` escalates, winning over any setting — including `false`
        // (the flag is explicit; the setting can never itself reach Bypass).
        assert_eq!(resolve_approval(false, true, Some(false)), Approval::Bypass);
        assert_eq!(resolve_approval(false, true, Some(true)), Approval::Bypass);
        assert_eq!(resolve_approval(false, true, None), Approval::Bypass);
    }

    /// The context window rides the gateway route by precedence `settings > default`,
    /// resolved once in `gateway_models` and carried only inside `Routing::Gateway`.
    #[test]
    fn gateway_models_resolves_the_context_window() {
        use BackendKind::Dev;
        // Unset → the built-in 1M default.
        let none = Settings::default();
        assert_eq!(gateway_models(Dev, None, &none).context_window, DEFAULT_CONTEXT_WINDOW);
        // A settings value wins over the default (e.g. opting the window to standard).
        let file = Settings { context_window: Some(200_000), ..Default::default() };
        assert_eq!(gateway_models(Dev, None, &file).context_window, 200_000);
    }

    /// `gateway_models` reads the exported `ANTHROPIC_*` env for the CLAUDE backend
    /// (whose model auth honors them), but NOT for `dev` (codex reads neither), and
    /// a `--model` flag overrides the env either way. This is the only path that
    /// touches process env, so it owns `ANTHROPIC_MODEL` for the test's lifetime.
    #[test]
    fn gateway_models_reads_env_for_claude_only() {
        use BackendKind::{Claude, Dev};
        let none = Settings::default();

        std::env::set_var("ANTHROPIC_MODEL", "user-exported");
        std::env::set_var("ANTHROPIC_SMALL_FAST_MODEL", "user-fast");

        // Claude: the exported env is preserved (no flag), on BOTH models.
        let g = gateway_models(Claude, None, &none);
        assert_eq!(g.model, "user-exported");
        assert_eq!(g.small_fast_model, "user-fast");

        // A `--model` flag overrides the exported env; the small/fast still tracks env.
        let g = gateway_models(Claude, Some("enso-ultra"), &none);
        assert_eq!(g.model, "enso-ultra");
        assert_eq!(g.small_fast_model, "user-fast");

        // dev/codex reads NO `ANTHROPIC_*` env → the built-in default, ignoring the
        // exported Claude vars entirely.
        let g = gateway_models(Dev, None, &none);
        assert_eq!(g.model, DEFAULT_MODEL);
        assert_eq!(g.small_fast_model, DEFAULT_SMALL_FAST_MODEL);

        // Settings supply the value when neither flag nor env is present (dev path,
        // no env read): the file's model wins over the built-in default.
        let file = Settings { model: Some("file-model".into()), ..Default::default() };
        assert_eq!(gateway_models(Dev, None, &file).model, "file-model");

        std::env::remove_var("ANTHROPIC_MODEL");
        std::env::remove_var("ANTHROPIC_SMALL_FAST_MODEL");
    }

    /// When the credential plan resolves NOTHING, a SELECTED provider fails
    /// closed (the backend then clears its model-auth env, and `run` warns), while
    /// an unconfigured/signed-out run inherits the backend's own account —
    /// unchanged, so "continuing locally" still works with a user's own key.
    #[test]
    fn unresolved_route_fails_closed_only_when_a_provider_is_selected() {
        assert!(matches!(unresolved_route(true), Route::FailClosed));
        assert!(matches!(unresolved_route(false), Route::Inherit));
        // Both carry no credential — the banner reads them the same ("off").
        assert!(unresolved_route(true).via().is_none());
        assert!(unresolved_route(false).via().is_none());
    }

}
