//! Canonical Enso model family glyphs, display names, and Free-tier policy.
//!
//! ONE source of truth shared by `gen`, `hanzo dev`, Claude Code `/model`
//! picker labels, Pi / Oh My Pi model catalogs, and the `hanzo` CLI / TUI so
//! terminal surfaces match web and desktop Enso visuals:
//!
//! - **`enso-auto`** (default) is the **only** closed circle: `○` (U+25CB).
//! - Every explicit Enso tier opens progressively from Ultra down to Flash:
//!   `◕` (Ultra) → `◑` (Max) → `◔` (Pro) → `◌` (Enso) → `◦` (Flash).
//! - **`enso-free` / `Enso Free`** is retired and must never be shown in any
//!   model list or picker. Free-tier accounts use `enso-auto` and `enso-flash`
//!   only; legacy `enso-free` references normalize cleanly to `enso-auto`.

/// Closed circle (U+25CB) — exclusively `enso-auto`, the default adaptive router.
pub const GLYPH_AUTO: &str = "○";
/// 3/4 circle (U+25D5) — slightly open (`enso-ultra`).
pub const GLYPH_ULTRA: &str = "◕";
/// 1/2 circle (U+25D1) — more open (`enso-max`).
pub const GLYPH_MAX: &str = "◑";
/// 1/4 circle (U+25D4) — more open (`enso-pro`).
pub const GLYPH_PRO: &str = "◔";
/// Dotted open ring (U+25CC) — more open (`enso` standard / fallback).
pub const GLYPH_ENSO: &str = "◌";
/// Lightest open ring (U+25E6) — most open (`enso-flash`).
pub const GLYPH_FLASH: &str = "◦";

/// Models permitted on the Free tier: Enso Auto and Enso Flash only.
/// `enso-free` is retired and never exposed.
pub const FREE_TIER_MODELS: &[&str] = &["enso-auto", "enso-flash"];

/// One visible Enso model in canonical display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnsoModel {
    pub id: &'static str,
    pub name: &'static str,
    pub glyph: &'static str,
    pub free_tier: bool,
    pub desc: &'static str,
}

/// The visible Enso model lineup, ordered from default router (`enso-auto`)
/// then progressively from highest capability (`enso-ultra`) to fastest
/// (`enso-flash`). Never contains `enso-free`.
pub const ENSO_MODELS: &[EnsoModel] = &[
    EnsoModel {
        id: "enso-auto",
        name: "Enso Auto",
        glyph: GLYPH_AUTO,
        free_tier: true,
        desc: "Adaptive router (default) — matches model and depth to each prompt",
    },
    EnsoModel {
        id: "enso-ultra",
        name: "Enso Ultra",
        glyph: GLYPH_ULTRA,
        free_tier: false,
        desc: "Hardest turns, deep multi-model routing (1M context)",
    },
    EnsoModel {
        id: "enso-max",
        name: "Enso Max",
        glyph: GLYPH_MAX,
        free_tier: false,
        desc: "Hard turns, frontier reasoning (1M context)",
    },
    EnsoModel {
        id: "enso-pro",
        name: "Enso Pro",
        glyph: GLYPH_PRO,
        free_tier: false,
        desc: "Frontier coders, routed (1M context)",
    },
    EnsoModel {
        id: "enso-flash",
        name: "Enso Flash",
        glyph: GLYPH_FLASH,
        free_tier: true,
        desc: "Fast routine turns: reads, searches, small edits",
    },
];

/// Strip extended-context suffix (`[1m]`), provider prefix (`hanzo/`), trim,
/// and lowercase into a normalized slug.
fn normalized_slug(model_id: &str) -> String {
    let trimmed = model_id.trim();
    let without_ctx = match trimmed.find('[') {
        Some(idx) => &trimmed[..idx],
        None => trimmed,
    };
    let tail = without_ctx.rsplit('/').next().unwrap_or(without_ctx).trim();
    tail.to_ascii_lowercase()
}

/// Returns true if `model_id` names a retired `enso-free` alias that must never
/// be rendered in any CLI/TUI model list or picker.
pub fn is_hidden_enso_model(model_id: &str) -> bool {
    matches!(
        normalized_slug(model_id).as_str(),
        "enso-free" | "enso_free" | "enso free"
    )
}

/// Rewrite retired `enso-free` aliases to `enso-auto` while leaving every other
/// model id untouched.
pub fn normalize_enso_model(model_id: &str) -> &str {
    if is_hidden_enso_model(model_id) {
        "enso-auto"
    } else {
        model_id
    }
}

/// Returns true if `model_id` is a visible member of the Enso model family.
/// Retired `enso-free` returns false so it is excluded from Enso UI surfaces.
pub fn is_enso_model(model_id: &str) -> bool {
    let slug = normalized_slug(model_id);
    if matches!(slug.as_str(), "enso-free" | "enso_free" | "enso free") {
        return false;
    }
    slug == "enso" || slug.starts_with("enso-") || slug.starts_with("enso_")
}

/// Returns true if `model_id` is allowed on the Free tier (`enso-auto` and
/// `enso-flash` only). Never grants `enso-free`, `enso-pro`, `enso-max`, or
/// `enso-ultra`.
pub fn is_free_tier_model(model_id: &str) -> bool {
    matches!(
        normalized_slug(model_id).as_str(),
        "enso-auto" | "enso-flash"
    )
}

/// Return the canonical monochrome Unicode Enso glyph for `model_id`, or `""`
/// for non-Enso models and retired `enso-free`.
///
/// ONLY `enso-auto` receives the closed circle `○` (U+25CB); every other Enso
/// tier progressively opens from Ultra (`◕`) down to Flash (`◦`).
pub fn enso_glyph(model_id: &str) -> &'static str {
    let slug = normalized_slug(model_id);
    match slug.as_str() {
        "enso-auto" | "enso_auto" => GLYPH_AUTO,
        "enso-ultra" | "enso_ultra" => GLYPH_ULTRA,
        "enso-max" | "enso_max" => GLYPH_MAX,
        "enso-pro" | "enso_pro" => GLYPH_PRO,
        "enso" => GLYPH_ENSO,
        "enso-flash" | "enso_flash" => GLYPH_FLASH,
        "enso-free" | "enso_free" | "enso free" => "",
        s if s.starts_with("enso-") || s.starts_with("enso_") => GLYPH_ENSO,
        _ => "",
    }
}

/// Canonical display name for a visible Enso model id, or `None` for non-Enso
/// or retired `enso-free`.
pub fn enso_display_name(model_id: &str) -> Option<&'static str> {
    match normalized_slug(model_id).as_str() {
        "enso-auto" | "enso_auto" => Some("Enso Auto"),
        "enso-ultra" | "enso_ultra" => Some("Enso Ultra"),
        "enso-max" | "enso_max" => Some("Enso Max"),
        "enso-pro" | "enso_pro" => Some("Enso Pro"),
        "enso" => Some("Enso"),
        "enso-flash" | "enso_flash" => Some("Enso Flash"),
        _ => None,
    }
}

/// Format a model id for human-facing CLI/TUI output by prefixing its Enso
/// glyph when `model_id` is a visible Enso model (`"○ enso-auto"`). Non-Enso
/// models are returned unchanged.
pub fn format_model_id(model_id: &str) -> String {
    let glyph = enso_glyph(model_id);
    if glyph.is_empty() || model_id.starts_with(glyph) {
        model_id.to_string()
    } else {
        format!("{glyph} {model_id}")
    }
}

/// Format a model picker label (`"○ Enso Auto"`, `"◕ Enso Ultra"`, …) without
/// duplicating the glyph if already present.
pub fn format_model_label(model_id: &str, name: &str) -> String {
    let glyph = enso_glyph(model_id);
    if glyph.is_empty() || name.starts_with(glyph) {
        name.to_string()
    } else {
        format!("{glyph} {name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn only_enso_auto_is_the_closed_circle() {
        assert_eq!(enso_glyph("enso-auto"), "○");
        assert_eq!(enso_glyph("hanzo/enso-auto"), "○");
        assert_eq!(enso_glyph("enso-auto[1m]"), "○");
        assert_eq!(enso_glyph("ENSO-AUTO"), "○");

        for other in [
            "enso-ultra",
            "enso-max",
            "enso-pro",
            "enso",
            "enso-flash",
            "enso-free",
            "enso-custom",
            "zen6",
            "anthropic/claude-opus-4-8",
        ] {
            assert_ne!(
                enso_glyph(other),
                "○",
                "only enso-auto may use the closed circle ○, got it for {other}"
            );
        }
    }

    #[test]
    fn enso_glyphs_progressively_open_with_six_distinct_monochrome_symbols() {
        let ordered = [
            ("enso-auto", "○"),
            ("enso-ultra", "◕"),
            ("enso-max", "◑"),
            ("enso-pro", "◔"),
            ("enso", "◌"),
            ("enso-flash", "◦"),
        ];
        let mut seen = HashSet::new();
        for (id, want) in ordered {
            let got = enso_glyph(id);
            assert_eq!(got, want, "{id}");
            assert_eq!(got.chars().count(), 1, "{id} glyph must be one Unicode scalar");
            assert!(seen.insert(got), "duplicate glyph {got} for {id}");
        }
        assert_eq!(enso_glyph("hanzo/enso-ultra[1m]"), "◕");
        assert_eq!(enso_glyph("hanzo/enso-max"), "◑");
        assert_eq!(enso_glyph("hanzo/enso-pro"), "◔");
        assert_eq!(enso_glyph("hanzo/enso"), "◌");
        assert_eq!(enso_glyph("hanzo/enso-flash"), "◦");
        assert_eq!(enso_glyph("zen6"), "");
    }

    #[test]
    fn enso_free_is_hidden_never_listed_and_normalizes_to_enso_auto() {
        for hidden in ["enso-free", "ENSO-FREE", "hanzo/enso-free", "enso_free", "enso-free[1m]"] {
            assert!(is_hidden_enso_model(hidden), "{hidden} must be hidden");
            assert!(!is_enso_model(hidden), "{hidden} must not be a visible Enso model");
            assert_eq!(enso_glyph(hidden), "", "{hidden} must carry no glyph");
            assert_eq!(enso_display_name(hidden), None, "{hidden} must have no display name");
            assert_eq!(normalize_enso_model(hidden), "enso-auto");
        }
        for m in ENSO_MODELS {
            assert!(!is_hidden_enso_model(m.id), "{} must not be hidden", m.id);
            assert!(!m.name.eq_ignore_ascii_case("Enso Free"));
        }
        assert_eq!(normalize_enso_model("enso-pro"), "enso-pro");
    }

    #[test]
    fn free_tier_allows_enso_auto_and_enso_flash_only() {
        assert_eq!(FREE_TIER_MODELS, &["enso-auto", "enso-flash"]);
        assert!(is_free_tier_model("enso-auto"));
        assert!(is_free_tier_model("hanzo/enso-auto"));
        assert!(is_free_tier_model("enso-flash"));
        assert!(is_free_tier_model("hanzo/enso-flash"));

        for denied in ["enso-free", "enso-pro", "enso-max", "enso-ultra", "enso", "zen6"] {
            assert!(!is_free_tier_model(denied), "free tier must deny {denied}");
        }
        let free_ids: Vec<&str> = ENSO_MODELS.iter().filter(|m| m.free_tier).map(|m| m.id).collect();
        assert_eq!(free_ids, vec!["enso-auto", "enso-flash"]);
    }

    #[test]
    fn format_helpers_prefix_glyph_once() {
        assert_eq!(format_model_id("enso-auto"), "○ enso-auto");
        assert_eq!(format_model_id("enso-flash"), "◦ enso-flash");
        assert_eq!(format_model_id("○ enso-auto"), "○ enso-auto");
        assert_eq!(format_model_id("zen6"), "zen6");

        assert_eq!(format_model_label("enso-ultra", "Enso Ultra"), "◕ Enso Ultra");
        assert_eq!(format_model_label("enso-ultra", "◕ Enso Ultra"), "◕ Enso Ultra");
        assert_eq!(format_model_label("zen6", "Zen 6"), "Zen 6");
    }
}
