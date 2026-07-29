//! The zen ⇆ Claude-carrier table — ONE source of truth for how a Hanzo model id
//! reaches Claude Code, and how Claude Code's own tiering reaches Hanzo models.
//!
//! **Why a carrier at all.** Claude Code sizes a model's context window and
//! features from ids it RECOGNIZES. A custom gateway id (`zen5-pro`) is unknown to
//! it, so it gets the fallback budget and is rejected client-side above it — the
//! "max context 131072" the gateway never actually imposes. Nothing lets a client
//! declare a custom model's window. So a tier names a CARRIER: a `claude-*` id
//! Claude Code knows, whose budget it grants (`[1m]` where the extended window is
//! wanted). The carrier is a CLIENT-SIDE concern only — the `modelOverrides` map in
//! the seeded settings ([`super::home`]) rewrites it back to the zen id before the
//! request leaves the process, so the gateway is served the zen id and the carrier
//! never reaches the wire. Requires Claude Code v2.1.200+.
//!
//! **What is a value here and what is a place.** The table is keyed by the SERVED
//! zen id — the only id the gateway is authoritative about. Everything else
//! (carrier, tiering slot, picker label) is a projection of it. A model absent from
//! the table passes through unchanged with Claude's own budgeting, which is what
//! makes the table additive rather than an allowlist: the gateway stays the sole
//! authority on which ids are valid.

use std::collections::BTreeMap;

/// `ANTHROPIC_MODEL` — the session's main model.
pub const MAIN: &str = "ANTHROPIC_MODEL";
/// The small/fast slot Claude resolves its background work through.
pub const HAIKU: &str = "ANTHROPIC_DEFAULT_HAIKU_MODEL";

/// Claude's standard context window; a request above it is "extended" (1M),
/// signalled by the `[1m]` model-id suffix.
pub const STANDARD_WINDOW: u64 = 200_000;

/// One capability tier: the zen id the gateway serves, the Claude id that unlocks
/// its budget, the tiering slot it fills, and how the `/model` picker names it.
pub struct Tier {
    /// The served zen alias — the wire model api.hanzo.ai receives.
    pub zen: &'static str,
    /// The recognized id Claude Code budgets from. `None` pins the zen id directly
    /// (a short-context tier, which never needs a lifted window).
    pub carrier: Option<&'static str>,
    /// The `ANTHROPIC_DEFAULT_*_MODEL` slot this tier fills, if any. `None` means
    /// selectable but not part of Claude's own tiering.
    pub slot: Option<&'static str>,
    /// The `/model` picker label and its one-line description.
    pub name: &'static str,
    pub desc: &'static str,
}

/// Every zen id here must be one api.hanzo.ai serves. `zen5-pro` fills TWO slots
/// (opus and the top-effort fable tier) because the gateway has no distinct
/// deeper model to put behind fable yet — so [`carrier`] takes the FIRST match,
/// which is the opus carrier.
pub const TIERS: &[Tier] = &[
    Tier { zen: "zen5-flash", carrier: None, slot: Some(HAIKU), name: "Zen5 Flash", desc: "Hanzo Zen5 Flash — fast, cheap tier" },
    Tier { zen: "zen5", carrier: Some("claude-sonnet-4-6[1m]"), slot: Some("ANTHROPIC_DEFAULT_SONNET_MODEL"), name: "Zen5", desc: "Hanzo Zen5 — frontier tier (1M context)" },
    Tier { zen: "zen5-pro", carrier: Some("claude-opus-4-8[1m]"), slot: Some("ANTHROPIC_DEFAULT_OPUS_MODEL"), name: "Zen5 Pro", desc: "Hanzo Zen5 Pro — deep reasoning (1M context)" },
    Tier { zen: "zen5-pro", carrier: Some("claude-fable-5[1m]"), slot: Some("ANTHROPIC_DEFAULT_FABLE_MODEL"), name: "Zen5 Pro (max effort)", desc: "Hanzo Zen5 Pro, top tier (1M context)" },
    Tier { zen: "zen5-coder", carrier: Some("claude-sonnet-5"), slot: None, name: "Zen5 Coder", desc: "Hanzo Zen5 Coder — code-specialized" },
];

/// The tier a served id belongs to, if any. The id is normalized through
/// [`bare`] first: `ANTHROPIC_MODEL` is one of the precedence sources the run
/// resolves a model from, and Claude's own convention lets it carry the `[1m]`
/// suffix — so an exported `ANTHROPIC_MODEL=zen5[1m]` must find the zen5 tier
/// rather than miss the table and grow a second suffix.
fn of(model: &str) -> Option<&'static Tier> {
    let model = bare(model);
    TIERS.iter().find(|t| t.zen == model)
}

/// Drop the `[1m]` extended-context suffix, so a carrier matches its override key
/// and a standard-window run pins the plain id.
fn bare(id: &str) -> &str {
    match id.find('[') {
        Some(i) => &id[..i],
        None => id,
    }
}

/// A short-context variant (`*-flash`, `*-mini`) — the background models, which
/// must never request the extended window.
fn short(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("flash") || m.contains("mini")
}

/// The id to put on the wire for `model`: its carrier when it has one, else the
/// bare id. Either way the `[1m]` extended-context suffix survives only while the
/// run actually asks for more than the standard window — so `contextWindow:
/// 200000` opts every model back out, tiered or not, and an incoming id that
/// already carries the suffix is normalized rather than doubled.
pub fn wire(model: &str, context_window: u64) -> String {
    let model = bare(model);
    match of(model) {
        Some(t) => wire_tier(t, context_window),
        None if context_window > STANDARD_WINDOW && !short(model) => format!("{model}[1m]"),
        None => model.to_string(),
    }
}

/// The wire id for one TIER — keyed on the tier itself, never on its zen id,
/// because two tiers may share an id (`zen5-pro` fills both the opus and the fable
/// slot) and an id lookup would hand them both the same carrier.
fn wire_tier(t: &Tier, context_window: u64) -> String {
    let extended = context_window > STANDARD_WINDOW;
    match t.carrier {
        Some(c) if extended => c.to_string(),
        Some(c) => bare(c).to_string(),
        None if extended && !short(t.zen) => format!("{}[1m]", t.zen),
        None => t.zen.to_string(),
    }
}

/// The identity line for a tiered model, or `None` for an id the table does not
/// name. Claude Code is handed a CARRIER (`claude-opus-4-8`) so it grants the real
/// context budget, and its base prompt then has it introduce itself as that model
/// — a misidentification the carrier itself creates. This corrects only that: an
/// APPEND, so the harness keeps its own tool-use, safety and coding conventions.
/// An untiered id rides no carrier, so nothing is claimed on its behalf.
pub fn identity(model: &str) -> Option<String> {
    let t = of(model)?;
    t.carrier?;
    Some(format!(
        "You are running through the Hanzo AI cloud as {} (the `{}` capability tier, served via the Hanzo gateway). \
         When asked what model or assistant you are, identify as a Hanzo Zen model. You are operating inside the \
         Claude Code harness; keep its tool-use, safety, and coding conventions — only your identity is Hanzo Zen.",
        t.name, t.zen
    ))
}

/// The `modelOverrides` document for the seeded settings: carrier ⇒ zen. Claude
/// Code budgets from the KEY and sends the VALUE, so this is what keeps the
/// carrier client-side. Policy, not preference — reapplied on every launch.
pub fn overrides() -> BTreeMap<String, String> {
    TIERS
        .iter()
        .filter_map(|t| t.carrier.map(|c| (bare(c).to_string(), t.zen.to_string())))
        .collect()
}

/// The `ANTHROPIC_*` model env for a gateway-routed Claude run — the main model,
/// every tiering slot, and the picker labels — resolved as ONE pure map so the
/// precedence is a value rather than an order of side effects.
///
/// The slots matter beyond branding: Claude resolves subagents, `/compact` and its
/// background work through them, and their built-in defaults are `claude-*` ids the
/// gateway does not serve — so an unset slot 400s every subagent. The two models
/// THIS run resolved (`model`, `small_fast`) override the table's defaults for
/// their own slots; a model outside the table then carries NO label, because the
/// picker must never name a model the slot does not hold.
pub fn env(model: &str, small_fast: &str, context_window: u64) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    // Every slot is filled from ITS OWN tier — by identity, never by zen id, so the
    // two tiers sharing `zen5-pro` keep their own carriers and their own labels.
    for t in TIERS.iter().filter(|t| t.slot.is_some()) {
        fill(&mut out, t.slot.unwrap(), wire_tier(t, context_window), Some(t));
    }
    // The two models THIS run resolved then win their own slots.
    out.insert(MAIN.to_string(), wire(model, context_window));
    fill(&mut out, HAIKU, wire(small_fast, context_window), of(small_fast));
    out
}

/// Set one slot to `id` and carry `tier`'s label — or drop the label when the model
/// filling the slot has no tier of its own.
fn fill(out: &mut BTreeMap<String, String>, slot: &str, id: String, tier: Option<&Tier>) {
    out.insert(slot.to_string(), id);
    let (name, desc) = (format!("{slot}_NAME"), format!("{slot}_DESCRIPTION"));
    match tier {
        Some(t) => {
            out.insert(name, t.name.to_string());
            out.insert(desc, t.desc.to_string());
        }
        None => {
            out.remove(&name);
            out.remove(&desc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every carrier maps back to its own zen id, so a request budgeted from the
    /// carrier is SERVED as zen. A carrier that overrode to the wrong tier would
    /// silently bill and answer from another model.
    #[test]
    fn every_carrier_overrides_back_to_its_own_tier() {
        let o = overrides();
        for t in TIERS {
            let Some(c) = t.carrier else {
                // A direct tier needs no override — and must not have one.
                assert!(!o.contains_key(t.zen), "{} is direct, it must not be an override key", t.zen);
                continue;
            };
            assert_eq!(o.get(bare(c)).map(String::as_str), Some(t.zen), "carrier {c} must resolve to {}", t.zen);
            // The override key is the BARE id: Claude matches the model it
            // budgeted from, not the suffixed request.
            assert!(
                bare(c) == c || !o.contains_key(c),
                "the override key must be the bare id, not {c}"
            );
        }
    }

    /// `zen5-pro` fills TWO slots, and each must keep its OWN carrier and label. A
    /// lookup by zen id is ambiguous between them, so filling a slot from the id
    /// silently gave the fable slot the opus carrier — a model the picker names
    /// "max effort" that is not the max-effort model.
    #[test]
    fn two_slots_sharing_one_zen_id_keep_their_own_carriers() {
        let e = env("zen5", "zen5-flash", 1_000_000);
        assert_eq!(e["ANTHROPIC_DEFAULT_OPUS_MODEL"], "claude-opus-4-8[1m]");
        assert_eq!(e["ANTHROPIC_DEFAULT_FABLE_MODEL"], "claude-fable-5[1m]");
        assert_eq!(e["ANTHROPIC_DEFAULT_OPUS_MODEL_NAME"], "Zen5 Pro");
        assert_eq!(e["ANTHROPIC_DEFAULT_FABLE_MODEL_NAME"], "Zen5 Pro (max effort)");
        // Both still SERVE the one zen id — the carrier is client-side only.
        let o = overrides();
        assert_eq!(o["claude-opus-4-8"], "zen5-pro");
        assert_eq!(o["claude-fable-5"], "zen5-pro");
    }

    /// An id that already carries the `[1m]` suffix is normalized, never doubled:
    /// `ANTHROPIC_MODEL` is one of the sources a run resolves its model from, and
    /// Claude's own convention lets a user export it suffixed.
    #[test]
    fn an_already_suffixed_id_is_normalized_not_doubled() {
        assert_eq!(wire("enso[1m]", 1_000_000), "enso[1m]");
        assert_eq!(wire("zen5[1m]", 1_000_000), "claude-sonnet-4-6[1m]");
        assert_eq!(wire("enso[1m]", 200_000), "enso");
    }

    /// An id outside the table passes through untouched — the table is a lookup,
    /// never an allowlist. The gateway stays the sole authority on validity.
    #[test]
    fn an_untiered_id_passes_through() {
        assert_eq!(wire("enso", 1_000_000), "enso[1m]");
        assert_eq!(wire("enso-flash", 1_000_000), "enso-flash");
        assert_eq!(wire("some-raw-upstream", 200_000), "some-raw-upstream");
    }

    /// The extended-context suffix rides only a large-context model at a window
    /// beyond the standard one — for carriers and bare ids alike.
    #[test]
    fn the_extended_suffix_tracks_the_requested_window() {
        assert_eq!(wire("zen5", 1_000_000), "claude-sonnet-4-6[1m]");
        assert_eq!(wire("zen5", 200_000), "claude-sonnet-4-6");
        assert_eq!(wire("zen5-flash", 1_000_000), "zen5-flash");
        // A carrier the table declares WITHOUT the suffix never grows one.
        assert_eq!(wire("zen5-coder", 1_000_000), "claude-sonnet-5");
    }

    /// `zen5-pro` fills two slots; the carrier lookup is deterministic (first match).
    #[test]
    fn a_tier_in_two_slots_resolves_to_one_carrier() {
        assert_eq!(wire("zen5-pro", 1_000_000), "claude-opus-4-8[1m]");
    }

    /// Every slot is filled and labelled, and the run's own models win their slots.
    #[test]
    fn the_run_models_override_their_slots() {
        let e = env("zen5-coder", "enso-flash", 1_000_000);
        assert_eq!(e[MAIN], "claude-sonnet-5");
        // The untiered small/fast model takes the slot and drops the tier's label.
        assert_eq!(e[HAIKU], "enso-flash");
        assert!(!e.contains_key(&format!("{HAIKU}_NAME")));
        // The tiers Claude picks from are still fully named.
        assert_eq!(e["ANTHROPIC_DEFAULT_SONNET_MODEL"], "claude-sonnet-4-6[1m]");
        assert_eq!(e["ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"], "Zen5");
        assert_eq!(e["ANTHROPIC_DEFAULT_FABLE_MODEL_NAME"], "Zen5 Pro (max effort)");
    }

    /// The env carries no secret and no endpoint — only model ids and labels — so
    /// nothing here can leak a credential into a picker or a log.
    #[test]
    fn the_model_env_is_free_of_credentials() {
        for (k, v) in env("zen5", "zen5-flash", 1_000_000) {
            assert!(k.starts_with("ANTHROPIC_"), "unexpected key {k}");
            assert!(!k.contains("TOKEN") && !k.contains("KEY") && !k.contains("BASE_URL"), "{k} is not a model var");
            assert!(!v.contains("://"), "{k} must not carry an endpoint: {v}");
        }
    }
}
