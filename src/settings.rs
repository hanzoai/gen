//! The native `hanzo dev` settings home: `~/.hanzo/settings.json`.
//!
//! ONE file configures the coding agent's defaults on a fresh machine — the model
//! it names, whether it auto-approves the agent's actions, whether it attaches the
//! Hanzo MCP toolset, and the context window it requests — so a new box needs no
//! per-shell `ANTHROPIC_MODEL` export or hand-edited `~/.claude/settings.json`.
//! Every key is optional; a missing file or key falls through to the built-in
//! default, and an explicit CLI flag or process env always wins over the file
//! (precedence lives in [`super::run`] / [`super::resolve_model`], not here).

use serde::{Deserialize, Serialize};
use crate::root::Root;
use std::path::{Path, PathBuf};

/// The parsed `~/.hanzo/settings.json`. Every field is `Option`, so "unset" is
/// distinct from a value — that is what lets the file sit BELOW a CLI flag and the
/// process env in precedence. Unknown keys are ignored (forward-compatible), and a
/// missing key defaults to `None` via the container `#[serde(default)]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// The gateway model `hanzo dev` names (`ANTHROPIC_MODEL` for Claude, codex's
    /// `model` for `dev`). Unset ⇒ the built-in [`crate::DEFAULT_MODEL`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The gateway small/fast model (Claude's `ANTHROPIC_SMALL_FAST_MODEL`). Unset
    /// ⇒ the built-in [`crate::DEFAULT_SMALL_FAST_MODEL`]. `dev` has no small/fast model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub small_fast_model: Option<String>,
    /// Auto-approve the coding agent's actions without a per-action prompt. Unset
    /// ⇒ OFF (the built-in default). `--auto` / `--yes` opt in for one run; 
    /// `autoApprove: true` in settings persists the opt-in. `--ask` /
    /// `--safe` force it OFF regardless; `--no-sandbox` escalates past it. This is a
    /// PERSISTED default only: dropping the sandbox entirely is never persisted, it
    /// is always a per-invocation `--no-sandbox` (fail-secure — see [`super::Approval`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_approve: Option<bool>,
    /// Attach the Hanzo MCP toolset. Unset ⇒ ON (the built-in default). `--no-mcp`
    /// still forces it off regardless.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<bool>,
    /// Which coding agent a bare `hanzo` runs: `dev`, `claude`, `codex`,
    /// `agy` or `cursor`. Unset ⇒ [`super::backend::DEFAULT`] (`dev`, ours).
    /// Naming one on the command line — `hanzo dev --claude`,
    /// `--backend claude` — wins over this, because an invocation is more
    /// specific than a default.
    ///
    /// The three are separate products, so an unreadable value is REFUSED rather
    /// than silently falling back to ours: a reader who set `codex` and got `dev`
    /// would be told nothing while a different agent read their repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Which connected account the harness uses when the command line does not
    /// name one: `claude`, `codex`, `agy`, `cursor`, or `hanzo` (the gateway,
    /// and every `hanzo:{owner}/{name}` identity). `HANZO_HARNESS_ACCOUNT`
    /// overrides this for one run. Unset ⇒ `agent` above.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// The context window (in tokens) `hanzo dev` requests on the GATEWAY route.
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
    /// The settings home: `~/.hanzo/settings.json` under `root`.
    pub fn path(root: &Root) -> PathBuf {
        root.hanzo().join("settings.json")
    }

    /// Load the settings, best-effort — the coding agent must start even if `$HOME`
    /// is odd. A missing file is CREATED with the built-in defaults (so a fresh box
    /// gets a discoverable, editable config) and read back as all-default; an
    /// unreadable or malformed file degrades to defaults WITHOUT clobbering it —
    /// a parse slip must never destroy a user's hand-edit.
    pub fn load(root: &Root) -> Settings {
        let path = Self::path(root);
        match std::fs::read_to_string(&path) {
            Ok(body) => {
                let settings: Settings = serde_json::from_str(&body).unwrap_or_default();
                // One-time migration: if the file matches the OLD fully-populated defaults
                // exactly (autoApprove: true + all other keys at old built-in defaults),
                // flip it to false and warn. We CANNOT distinguish a user choice from
                // the old first-run default, so we migrate only the exact match.
                maybe_migrate_old_auto_approve(&path, &settings);
                settings
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write_defaults(&path);
                Settings::default()
            }
            Err(_) => Settings::default(),
        }
    }

    /// Remember which coding agent a bare `hanzo` should launch. Sets `agent`
    /// on the existing document and leaves every other key alone, including
    /// ones this struct does not know.
    pub fn remember_agent(root: &Root, agent: &str) -> anyhow::Result<()> {
        use serde_json::Value;
        let path = Self::path(root);
        let mut doc = match std::fs::read_to_string(&path) {
            Ok(body) => serde_json::from_str::<Value>(&body).map_err(|e| {
                anyhow::anyhow!("~/.hanzo/settings.json could not be read ({e}) — the coding agent was not changed")
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                serde_json::to_value(defaults_document()).unwrap_or_else(|_| Value::Object(Default::default()))
            }
            Err(e) => anyhow::bail!("could not read ~/.hanzo/settings.json ({e})"),
        };
        if !doc.is_object() {
            anyhow::bail!(
                "~/.hanzo/settings.json is not an object — the coding agent was not changed"
            );
        }
        doc["agent"] = Value::String(agent.to_string());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = serde_json::to_string_pretty(&doc)?;
        crate::private::write(&path, format!("{body}\n").as_bytes())
            .map_err(|e| anyhow::anyhow!("could not write ~/.hanzo/settings.json ({e})"))
    }

    /// Pin which connected account the harness uses (`harness` in the same
    /// document). Other keys, including ones this struct does not know, stay.
    pub fn pin_account(root: &Root, account: &str) -> anyhow::Result<()> {
        use serde_json::Value;
        let path = Self::path(root);
        let mut doc = match std::fs::read_to_string(&path) {
            Ok(body) => serde_json::from_str::<Value>(&body).map_err(|e| {
                anyhow::anyhow!("~/.hanzo/settings.json could not be read ({e}) — the harness account was not changed")
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                serde_json::to_value(defaults_document()).unwrap_or_else(|_| Value::Object(Default::default()))
            }
            Err(e) => anyhow::bail!("could not read ~/.hanzo/settings.json ({e})"),
        };
        if !doc.is_object() {
            anyhow::bail!(
                "~/.hanzo/settings.json is not an object — the harness account was not changed"
            );
        }
        doc["harness"] = Value::String(account.to_string());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = serde_json::to_string_pretty(&doc)?;
        crate::private::write(&path, format!("{body}\n").as_bytes())
            .map_err(|e| anyhow::anyhow!("could not write ~/.hanzo/settings.json ({e})"))
    }
}

/// The fully-populated default document written on first run — every key at its
/// built-in value, so the file DOCUMENTS the defaults instead of being empty.
fn defaults_document() -> Settings {
    Settings {
        model: Some(crate::DEFAULT_MODEL.to_string()),
        small_fast_model: Some(crate::DEFAULT_SMALL_FAST_MODEL.to_string()),
        auto_approve: Some(false),
        mcp: Some(true),
        context_window: Some(super::DEFAULT_CONTEXT_WINDOW),
        agent: Some(super::backend::DEFAULT.as_str().to_string()),
        harness: None,
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

/// One-time migration: if the settings file matches the OLD fully-populated
/// defaults document exactly (autoApprove: true + all other keys at the old
/// built-in defaults), flip it to false and warn. We CANNOT distinguish a user
/// choice from the old first-run default, so we migrate only the exact match.
/// If the file has any other value (explicit user edit or partial defaults), we
/// leave it alone — the user's intent is either explicit or ambiguous.
fn maybe_migrate_old_auto_approve(path: &Path, parsed: &Settings) {
    // The OLD fully-populated defaults document (before the security fix).
    let old_defaults = Settings {
        model: Some(crate::DEFAULT_MODEL.to_string()),
        small_fast_model: Some(crate::DEFAULT_SMALL_FAST_MODEL.to_string()),
        auto_approve: Some(true),
        mcp: Some(true),
        context_window: Some(super::DEFAULT_CONTEXT_WINDOW),
        agent: Some(super::backend::DEFAULT.as_str().to_string()),
        harness: None,
    };

    // Check if the parsed settings match the old defaults exactly
    if parsed.model == old_defaults.model
        && parsed.small_fast_model == old_defaults.small_fast_model
        && parsed.auto_approve == old_defaults.auto_approve
        && parsed.mcp == old_defaults.mcp
        && parsed.context_window == old_defaults.context_window
        && parsed.agent == old_defaults.agent
        && parsed.harness == old_defaults.harness
    {
        // Read the raw file to check if it matches exactly (including unknown keys)
        if let Ok(body) = std::fs::read_to_string(path) {
            if let Ok(doc) = serde_json::from_str::<serde_json::Value>(&body) {
                let old_doc = serde_json::to_value(&old_defaults).unwrap();
                if doc == old_doc {
                    let mut new_doc = doc;
                    new_doc["autoApprove"] = serde_json::Value::Bool(false);
                    if let Ok(new_body) = serde_json::to_string_pretty(&new_doc) {
                        let _ = crate::private::write(path, format!("{new_body}\n").as_bytes());
                        eprintln!("warning: migrated ~/.hanzo/settings.json autoApprove from legacy default ON to OFF — pass --auto to opt in per-invocation or set autoApprove: true in settings to persist");
                    }
                }
            }
        }
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
        // Built from the constants, not repeated: the family was renamed once
        // (`enso` -> `enso-auto`) and a literal here is a second statement of the
        // default, which is exactly what goes stale.
        assert!(
            doc.contains(&format!(
                "\"model\":\"{}\"",
                crate::DEFAULT_MODEL
            )),
            "got {doc}"
        );
        assert!(
            doc.contains(&format!(
                "\"smallFastModel\":\"{}\"",
                crate::DEFAULT_SMALL_FAST_MODEL
            )),
            "got {doc}"
        );
        assert!(doc.contains("\"autoApprove\":false"), "got {doc}");
        assert!(doc.contains("\"mcp\":true"), "got {doc}");
        assert!(doc.contains("\"contextWindow\":1000000"), "got {doc}");
    }

    /// The path is the native home, distinct from `~/.config/hanzo/config.toml` and
    /// from Claude's `~/.claude/settings.json`.
    #[test]
    fn path_is_dot_hanzo_settings_json() {
        let p = Settings::path(&Root::at("/h"));
        assert_eq!(p, std::path::Path::new("/h/.hanzo/settings.json"));
    }
    
    /// The one-time migration flips the OLD exact default document from
    /// autoApprove: true to false and warns.
    #[test]
    fn migration_flips_exact_old_defaults() {
        use tempfile::NamedTempFile;
        use std::io::Write;
        
        // Create a temp file with the OLD exact defaults
        let mut file = NamedTempFile::new().unwrap();
        let old_doc = r#"{
  "model": "enso-auto",
  "smallFastModel": "enso-flash",
  "autoApprove": true,
  "mcp": true,
  "contextWindow": 1000000,
  "agent": "dev"
}"#;
        file.write_all(old_doc.as_bytes()).unwrap();
        file.flush().unwrap();
        
        // Parse it through load (which calls migration)
        let root = Root::at(file.path().parent().unwrap());
        let _ = Settings::load(&root);
        
        // Read back the file - it should now have autoApprove: false
        let content = std::fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"autoApprove\":false"));
        // The warning was printed to stderr - we can't easily capture it in this test
    }
    
    /// The migration does NOT flip a file that has been edited by the user
    /// (even if only one key differs from old defaults).
    #[test]
    fn migration_skips_user_edited_file() {
        use tempfile::NamedTempFile;
        use std::io::Write;
        
        // Create a temp file with user-edited model but old autoApprove
        let mut file = NamedTempFile::new().unwrap();
        let edited_doc = r#"{
  "model": "enso-ultra",
  "smallFastModel": "enso-flash",
  "autoApprove": true,
  "mcp": true,
  "contextWindow": 1000000,
  "agent": "dev"
}"#;
        file.write_all(edited_doc.as_bytes()).unwrap();
        file.flush().unwrap();
        
        // Parse it through load
        let root = Root::at(file.path().parent().unwrap());
        let _ = Settings::load(&root);
        
        // Read back - should still have autoApprove: true (user's choice)
        let content = std::fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"autoApprove\":true"));
    }
}
