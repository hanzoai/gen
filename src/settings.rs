//! The native `hanzo code` settings home: `~/.hanzo/settings.json`.
//!
//! ONE file configures the coding agent's defaults on a fresh machine — the model
//! it names, whether it auto-approves the agent's actions, whether it attaches the
//! Hanzo MCP toolset, and the context window it requests — so a new box needs no
//! per-shell `ANTHROPIC_MODEL` export or hand-edited `~/.claude/settings.json`.
//! Every key is optional; a missing file or key falls through to the built-in
//! default, and an explicit CLI flag or process env always wins over the file
//! (precedence lives in [`super::run`] / [`super::resolve_model`], not here).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The parsed `~/.hanzo/settings.json`. Every field is `Option`, so "unset" is
/// distinct from a value — that is what lets the file sit BELOW a CLI flag and the
/// process env in precedence. Unknown keys are ignored (forward-compatible), and a
/// missing key defaults to `None` via the container `#[serde(default)]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// The gateway model `hanzo code` names (`ANTHROPIC_MODEL` for Claude, codex's
    /// `model` for `dev`). Unset ⇒ the built-in [`super::DEFAULT_MODEL`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The gateway small/fast model (Claude's `ANTHROPIC_SMALL_FAST_MODEL`). Unset
    /// ⇒ the built-in [`super::DEFAULT_SMALL_FAST_MODEL`]. `dev` has no small/fast model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub small_fast_model: Option<String>,
    /// Auto-approve the coding agent's actions without a per-action prompt. Unset
    /// ⇒ ON (the built-in default — the confirmed always-on default). `--ask` /
    /// `--safe` force it OFF regardless; `--no-sandbox` escalates past it. This is a
    /// PERSISTED default only: dropping the sandbox entirely is never persisted, it
    /// is always a per-invocation `--no-sandbox` (fail-secure — see [`super::Approval`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_approve: Option<bool>,
    /// Attach the Hanzo MCP toolset. Unset ⇒ ON (the built-in default). `--no-mcp`
    /// still forces it off regardless.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<bool>,
    /// The context window (in tokens) `hanzo code` requests on the GATEWAY route.
    /// Unset ⇒ [`super::DEFAULT_CONTEXT_WINDOW`] (1M). Hanzo's frontier models are
    /// natively 1M, but a coding backend pointed at a custom gateway can't verify
    /// that and clamps itself to the standard 200K, so this NAMES the real window:
    /// Claude via the `[1m]` model suffix, `dev` via a `model_catalog_json`. Set it
    /// below 1M (e.g. `200000`) to opt out of the extended window. Applies only to
    /// the metered gateway route; a direct provider uses that provider's own window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

impl Settings {
    /// The settings home: `~/.hanzo/settings.json` (`None` only when `$HOME` is
    /// unresolvable — a headless/odd environment, which then uses built-in defaults).
    pub fn path() -> Option<PathBuf> {
        Some(dirs::home_dir()?.join(".hanzo").join("settings.json"))
    }

    /// Load the settings, best-effort — the coding agent must start even if `$HOME`
    /// is odd. A missing file is CREATED with the built-in defaults (so a fresh box
    /// gets a discoverable, editable config) and read back as all-default; an
    /// unreadable or malformed file degrades to defaults WITHOUT clobbering it —
    /// a parse slip must never destroy a user's hand-edit.
    pub fn load() -> Settings {
        let Some(path) = Self::path() else { return Settings::default() };
        match std::fs::read_to_string(&path) {
            Ok(body) => serde_json::from_str(&body).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write_defaults(&path);
                Settings::default()
            }
            Err(_) => Settings::default(),
        }
    }
}

/// The fully-populated default document written on first run — every key at its
/// built-in value, so the file DOCUMENTS the defaults instead of being empty.
fn defaults_document() -> Settings {
    Settings {
        model: Some(super::DEFAULT_MODEL.to_string()),
        small_fast_model: Some(super::DEFAULT_SMALL_FAST_MODEL.to_string()),
        auto_approve: Some(true),
        mcp: Some(true),
        context_window: Some(super::DEFAULT_CONTEXT_WINDOW),
    }
}

/// Write the built-in defaults to a fresh settings file (best-effort; a write
/// failure is silent — the in-memory defaults still apply). Atomic + owner-only via
/// the ONE file-write seam, the same guarantee the config + credential store use.
fn write_defaults(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(body) = serde_json::to_string_pretty(&defaults_document()) {
        let _ = crate::private::write(path, format!("{body}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty document parses to all-unset — every value then resolves to its
    /// built-in default at run time.
    #[test]
    fn empty_document_is_all_unset() {
        let s: Settings = serde_json::from_str("{}").expect("empty object parses");
        assert!(s.model.is_none() && s.small_fast_model.is_none() && s.mcp.is_none());
        assert!(s.auto_approve.is_none() && s.context_window.is_none());
    }

    /// Values round-trip; the JSON keys are camelCase (mirroring Claude's own
    /// settings conventions), and unknown keys are ignored (forward-compatible).
    #[test]
    fn parses_camel_case_and_ignores_unknown_keys() {
        let s: Settings = serde_json::from_str(
            r#"{ "model": "enso-ultra", "smallFastModel": "enso-flash", "autoApprove": false,
                 "mcp": false, "contextWindow": 200000, "future": 1 }"#,
        )
        .expect("parses");
        assert_eq!(s.model.as_deref(), Some("enso-ultra"));
        assert_eq!(s.small_fast_model.as_deref(), Some("enso-flash"));
        assert_eq!(s.auto_approve, Some(false));
        assert_eq!(s.mcp, Some(false));
        assert_eq!(s.context_window, Some(200_000));
    }

    /// The first-run document names every default explicitly — a self-documenting
    /// file, not an empty `{}` — and its keys are camelCase.
    #[test]
    fn defaults_document_names_the_built_in_defaults() {
        let doc = serde_json::to_string(&defaults_document()).unwrap();
        assert!(doc.contains("\"model\":\"enso\""), "got {doc}");
        assert!(doc.contains("\"smallFastModel\":\"enso-flash\""), "got {doc}");
        assert!(doc.contains("\"autoApprove\":true"), "got {doc}");
        assert!(doc.contains("\"mcp\":true"), "got {doc}");
        assert!(doc.contains("\"contextWindow\":1000000"), "got {doc}");
    }

    /// The path is the native home, distinct from `~/.config/hanzo/config.toml` and
    /// from Claude's `~/.claude/settings.json`.
    #[test]
    fn path_is_dot_hanzo_settings_json() {
        if let Some(p) = Settings::path() {
            assert!(p.ends_with(".hanzo/settings.json"), "got {}", p.display());
        }
    }
}
