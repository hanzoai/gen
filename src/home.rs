//! `hanzo code`'s OWN Claude config home: `~/.hanzo/claude`.
//!
//! Claude Code and `hanzo code` are independent products that happen to share a
//! binary. Sharing one mutable config braids them: the user's saved `/model`
//! selection leaks in as this session's identity — and it OUTRANKS `ANTHROPIC_MODEL`,
//! so a stale `fable` beats the resolved routing — while Hanzo's injected tiers leak
//! back out into their own sessions. `CLAUDE_CONFIG_DIR` decomplects them: the same
//! binary, two homes, no shared state.
//!
//! **The home follows the ROUTE, because the route is what says who is driving.**
//! Relocating is right exactly when Hanzo supplies the model and the credential
//! ([`Route::Via`]) — that is when a saved `/model` could outrank us and when our
//! injected tiers could leak out. Under `--no-route` ([`Route::Inherit`]) the
//! backend is promised its OWN account, and on this platform that account IS a file
//! in the config home (`~/.claude/.credentials.json`) — so relocating there would
//! hand the user an empty home and a login prompt instead of the pass-through the
//! flag promises. `FailClosed` clears model auth entirely, so it has no session to
//! own; it stays hands-off for the same reason. One rule: we take the home only
//! when we are driving.
//!
//! The home is a SUBDIRECTORY of `~/.hanzo`, not `~/.hanzo` itself, because the
//! directory name is what says whose schema is inside. `~/.hanzo/settings.json` is
//! Hanzo's own [`super::settings`] document; `~/.hanzo/claude/settings.json` is
//! Claude's. One path, one schema, one owner — the collision the old Go launcher
//! shipped (both schemas at `~/.hanzo/settings.json`, so Hanzo read Claude's saved
//! `/model` as its gateway model) is structurally impossible here.
//!
//! Everything seeded here is WRITE-ONCE, because everything here is a first-run
//! default the user's later edits own:
//!   - the non-interactive blockers (`skipAutoPermissionPrompt`,
//!     `skipDangerousModePermissionPrompt`, `hasCompletedOnboarding`) — a headless
//!     run cannot answer a TTY dialog, so these remove prompts, not guardrails;
//!   - first-run preferences (`effortLevel`, `includeCoAuthoredBy`, `enableWorkflows`).
//!
//! Nothing per-RUN is persisted here, and that is the load-bearing rule. The
//! carrier⇒zen `modelOverrides` map is a property of the GATEWAY ROUTE, so it rides
//! the run as a `--settings` overlay ([`super::claude`]) instead: persisted in this
//! home it would also rewrite a `--no-route` or direct-Anthropic session's model to
//! a zen id and 404 against api.anthropic.com. A route-dependent policy cannot live
//! in a route-independent file.
//!
//! Two more keys are deliberately absent. Permission policy is resolved ONCE, in
//! [`super::resolve_approval`], and reaches the child on argv — a persisted
//! `permissions.defaultMode: auto` would silently defeat `--ask` from a file the
//! user never opened. The theme has exactly one owner too ([`super::theme`], which
//! sets it per session and restores it after). Two writers for one key is the
//! disease this module exists to cure.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::backend::Route;

/// The `CLAUDE_CONFIG_DIR` to SET for `route` — `None` means DO NOT relocate, so
/// the child keeps Claude's own home and the account that lives in it. Pure, so
/// every reader (the launch env, the transcript pointer, the theme) agrees from
/// the route alone without coordinating.
pub fn relocate(route: &Route) -> Option<PathBuf> {
    match route {
        Route::Via(_) => Some(dirs::home_dir()?.join(".hanzo").join("claude")),
        Route::Inherit | Route::FailClosed => None,
    }
}

/// The home the child will ACTUALLY read for `route`: ours when we are driving,
/// else Claude's own. This is the one every consumer of the resulting FILES wants
/// (transcripts, the theme), as opposed to [`relocate`], which answers the
/// narrower "must we set the env var".
pub fn of(route: &Route) -> Option<PathBuf> {
    match relocate(route) {
        Some(d) => Some(d),
        None => Some(dirs::home_dir()?.join(".claude")),
    }
}

/// Create the home and seed its first-run defaults, returning the directory.
/// A no-op when this route keeps Claude's own home: seeding there would write
/// Hanzo's defaults into the user's install, which is the braiding this module
/// exists to prevent. Best-effort otherwise: a write failure degrades to an
/// unseeded home rather than blocking the session, exactly as a missing
/// `~/.hanzo/settings.json` does.
pub fn prepare(route: &Route) -> Option<PathBuf> {
    let dir = relocate(route)?;
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(doc) = serde_json::to_string_pretty(&defaults()) {
        once(&dir.join("settings.json"), format!("{doc}\n").as_bytes());
    }
    // Onboarding is a TTY dialog a headless run cannot answer; write-once so the
    // rest of Claude's own state file is never touched.
    once(&dir.join(".claude.json"), b"{\"hasCompletedOnboarding\":true}\n");
    Some(dir)
}

/// Write `body` only when `path` does not yet exist, so seeding never clobbers a
/// user's later edits. Owner-only + atomic via the ONE file-write seam.
fn once(path: &Path, body: &[u8]) {
    if !path.exists() {
        let _ = crate::private::write(path, body);
    }
}

/// The first-run settings document. PURE, so what a fresh home gets — and what it
/// pointedly does NOT get — is testable without a filesystem.
pub(crate) fn defaults() -> Value {
    json!({
        "includeCoAuthoredBy": false,
        "skipAutoPermissionPrompt": true,
        "skipDangerousModePermissionPrompt": true,
        "effortLevel": "max",
        "enableWorkflows": true,
    })
}

/// Claude's transcript for a session, under whichever home THIS route runs in:
/// `<home>/projects/<cwd-with-separators-as-dashes>/<session-id>.jsonl`. It must
/// read the same home the launch sets, or the cloud pointer and the interactive
/// tail both name a file that never exists.
pub fn transcript(route: &Route, cwd: &Path, session: &str) -> Option<PathBuf> {
    let slug: String = cwd
        .display()
        .to_string()
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect();
    Some(of(route)?.join("projects").join(slug).join(format!("{session}.jsonl")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code::backend::Routing;

    fn gateway() -> Route {
        Route::Via(Routing::Gateway {
            api: "https://api.hanzo.ai".into(),
            token: "T".into(),
            model: "zen5".into(),
            small_fast_model: "zen5-flash".into(),
            context_window: 1_000_000,
        })
    }

    /// When Hanzo drives, the home is Hanzo's — never the user's own `~/.claude`.
    #[test]
    fn a_routed_run_gets_hanzos_own_home() {
        let d = relocate(&gateway()).expect("$HOME resolves in tests");
        assert!(d.ends_with(".hanzo/claude"), "got {}", d.display());
        // Nor `~/.hanzo` itself, where it would collide with `settings::Settings`.
        assert_ne!(d.file_name().unwrap(), ".hanzo");
    }

    /// `--no-route` promises the backend its OWN account, and on this platform that
    /// account is a FILE in the config home (`~/.claude/.credentials.json`).
    /// Relocating would hand the user an empty home and a login prompt, so a
    /// hands-off route must set no `CLAUDE_CONFIG_DIR` at all — and must seed
    /// nothing, which would write Hanzo's defaults into the user's own install.
    #[test]
    fn a_hands_off_route_keeps_the_users_own_home_and_is_never_seeded() {
        for route in [Route::Inherit, Route::FailClosed] {
            assert!(relocate(&route).is_none(), "{route:?} must not relocate the config home");
            assert!(prepare(&route).is_none(), "{route:?} must seed nothing");
            // ...and the files it reads are Claude's own.
            assert!(of(&route).unwrap().ends_with(".claude"), "{route:?} reads Claude's own home");
        }
    }

    /// A fresh home gets the blockers a headless run needs — and none of the keys
    /// that have an owner elsewhere.
    #[test]
    fn the_defaults_are_blockers_and_preferences_only() {
        let d = defaults();
        assert_eq!(d["effortLevel"], "max");
        assert_eq!(d["skipDangerousModePermissionPrompt"], true);
        assert_eq!(d["skipAutoPermissionPrompt"], true);
        // Permission policy reaches the child on argv, never from this file.
        assert!(d.get("permissions").is_none(), "permission policy must not be persisted here");
        // The theme has exactly one owner, and it is not the seeder.
        assert!(d.get("theme").is_none(), "the theme is `theme::apply`'s alone");
        // The carrier map is a property of the gateway ROUTE; persisting it here
        // would rewrite a direct/`--no-route` session's model too.
        assert!(d.get("modelOverrides").is_none(), "route policy must not be persisted here");
    }

    /// Seeding never clobbers: an existing file is left exactly as the user left it.
    #[test]
    fn seeding_never_overwrites_a_users_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{\"effortLevel\":\"low\"}").unwrap();
        once(&path, b"{}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"effortLevel\":\"low\"}");
        // ...but an absent file IS created.
        let fresh = dir.path().join("new.json");
        once(&fresh, b"{\"a\":1}");
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "{\"a\":1}");
    }

    /// The transcript pointer follows whichever home the run actually uses, so a
    /// cloud pointer and the interactive tail name a file that exists on BOTH
    /// routes — the bug a fixed path would reintroduce on one of them.
    #[test]
    fn the_transcript_pointer_follows_the_home_of_its_route() {
        let at = |r: &Route| transcript(r, Path::new("/home/z/proj"), "sid-1").unwrap().display().to_string();
        assert!(at(&gateway()).ends_with("/.hanzo/claude/projects/-home-z-proj/sid-1.jsonl"), "got {}", at(&gateway()));
        assert!(at(&Route::Inherit).ends_with("/.claude/projects/-home-z-proj/sid-1.jsonl"), "got {}", at(&Route::Inherit));
    }
}
