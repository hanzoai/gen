//! Claude theme policy for `hanzo code claude` — NATIVE, no binary patching.
//!
//! Claude Code loads custom themes from `<config home>/themes/*.json` (its own
//! `loadCustomThemes`) and selects one via `theme: "custom:<name>"` in
//! `<config home>/settings.json` (built-ins are the bare name). Hanzo owns the
//! theme DATA (bundled JSON in Claude's schema) and drops it + selects it. Nothing
//! patches the Claude binary — a wrong/absent theme is ignored, never a crash.
//!
//! **The home is the ROUTE's** ([`super::home`]), never a fixed `~/.claude`. A
//! routed session reads `~/.hanzo/claude`, so writing the theme to the user's own
//! home would be doubly wrong: the session it is meant for never sees it, and the
//! install it was never meant for is mutated.
//!
//! Wrapper hygiene: we SAVE the current theme, apply ours for the wrapped session,
//! and RESTORE it when the session ends (RAII, any exit path). That matters most on
//! the hands-off route, where the home IS the user's, so plain `claude` keeps their
//! own theme.
//!
//! Auto light/dark: with the default `code.theme = "auto"`, we honor the user's
//! light/dark preference — Alucard (light Dracula) when their theme is a light
//! one, Dracula (dark) otherwise. Presets live in hanzoai/themes.

use std::path::{Path, PathBuf};

use super::backend::Route;
use super::home;

const DRACULA: &str = include_str!("../../../assets/themes/claude/dracula.json");
const ALUCARD: &str = include_str!("../../../assets/themes/claude/alucard.json");

/// Restores the previously-selected Claude theme when dropped (wrapper hygiene).
/// Holding `_guard` across the backend launch restores on ANY exit — normal,
/// error, or panic. It carries the home it read the theme FROM, so the restore
/// cannot land anywhere else than the write did.
pub struct Restore(Option<(PathBuf, String)>);

impl Drop for Restore {
    fn drop(&mut self) {
        if let Some((home, theme)) = self.0.take() {
            set_theme_raw(&home, &theme);
        }
    }
}

/// The requested theme name: an explicit `--theme` wins, else the persisted
/// `code.theme`. Trimmed + lowercased; empty/"none"/"off" disables theming.
pub fn effective(flag: Option<&str>, persisted: &str) -> Option<String> {
    let name = flag.unwrap_or(persisted).trim().to_lowercase();
    if name.is_empty() || name == "none" || name == "off" {
        return None;
    }
    Some(name)
}

/// Apply the effective theme for a Claude session and return a guard that restores
/// the prior theme on drop. "auto" picks Alucard for a light prior theme, Dracula
/// otherwise. `route` names the config home this session actually reads, so the
/// theme lands where the session will look for it. Best-effort — never blocks or
/// fails the session.
pub fn apply(route: &Route, flag: Option<&str>, persisted: &str) -> Restore {
    let (Some(home), Some(mut name)) = (home::of(route), effective(flag, persisted)) else {
        return Restore(None); // theming off (or no $HOME): nothing applied, nothing to restore
    };
    let prev = current_theme(&home);
    if name == "auto" {
        name = if prev.as_deref().map(is_light_ref).unwrap_or(false) {
            "alucard".into()
        } else {
            "dracula".into()
        };
    }
    // Drop the bundled preset's file so Claude can load it (user-supplied names
    // are assumed already present in the home's themes/).
    match name.as_str() {
        "dracula" => write_theme(&home, "dracula", DRACULA),
        "alucard" => write_theme(&home, "alucard", ALUCARD),
        _ => {}
    }
    set_theme_raw(&home, &theme_ref(&name));
    Restore(prev.map(|t| (home, t)))
}

/// Claude's built-in themes are selected by bare name; a custom theme (a file in
/// ~/.claude/themes) by the `custom:<name>` ref.
const BUILTIN: &[&str] = &[
    "dark",
    "light",
    "dark-daltonized",
    "light-daltonized",
    "dark-ansi",
    "light-ansi",
];

fn theme_ref(name: &str) -> String {
    if BUILTIN.contains(&name) {
        name.to_string()
    } else {
        format!("custom:{name}")
    }
}

/// Whether a settings `theme` value denotes a light theme (drives auto selection).
fn is_light_ref(theme: &str) -> bool {
    let t = theme.strip_prefix("custom:").unwrap_or(theme);
    t.starts_with("light") || t == "alucard"
}

fn write_theme(home: &Path, name: &str, body: &str) {
    let dir = home.join("themes");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(format!("{name}.json")), body);
}

/// Read the currently-selected theme (raw settings value) from `home`, if any.
fn current_theme(home: &Path) -> Option<String> {
    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(settings(home)).ok()?).ok()?;
    cfg.get("theme")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Write the exact `theme` value into `home`'s settings.json, preserving the rest.
fn set_theme_raw(home: &Path, value: &str) {
    let path = settings(home);
    let mut cfg: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = cfg.as_object_mut() {
        obj.insert("theme".into(), serde_json::Value::String(value.to_string()));
        if let Ok(s) = serde_json::to_string_pretty(&cfg) {
            let _ = std::fs::write(&path, s);
        }
    }
}

fn settings(home: &Path) -> PathBuf {
    home.join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_resolves_flag_over_persisted_and_disables_on_none() {
        assert_eq!(effective(Some("dracula"), "auto").as_deref(), Some("dracula"));
        assert_eq!(effective(None, "auto").as_deref(), Some("auto"));
        assert_eq!(effective(Some("  Alucard "), "x").as_deref(), Some("alucard"));
        assert_eq!(effective(Some("none"), "auto"), None);
        assert_eq!(effective(None, ""), None);
    }

    #[test]
    fn light_ref_detection_drives_auto() {
        assert!(is_light_ref("light"));
        assert!(is_light_ref("light-daltonized"));
        assert!(is_light_ref("custom:alucard"));
        assert!(!is_light_ref("dark"));
        assert!(!is_light_ref("custom:dracula"));
    }

    #[test]
    fn theme_ref_bare_for_builtin_custom_otherwise() {
        assert_eq!(theme_ref("dark"), "dark");
        assert_eq!(theme_ref("dracula"), "custom:dracula");
        assert_eq!(theme_ref("alucard"), "custom:alucard");
    }

    #[test]
    fn bundled_presets_are_valid() {
        for body in [DRACULA, ALUCARD] {
            let t: serde_json::Value = serde_json::from_str(body).unwrap();
            assert!(t.get("name").and_then(|v| v.as_str()).is_some());
            assert!(t.get("base").and_then(|v| v.as_str()).is_some());
        }
    }
}
