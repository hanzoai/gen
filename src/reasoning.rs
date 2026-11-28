//! Canonical reasoning effort levels across `gen`, `hanzo dev`, cloud coding
//! sessions, `hanzod`, and every wrapped coding backend.
//!
//! Matches the full web & desktop reasoning effort ladder:
//! `auto`, `off` (`none`), `minimal`, `low`, `medium`, `high`, `xhigh`, `max`.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The full reasoning effort ladder supported across Hanzo web, desktop, CLI,
/// and tracked frontier models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Leave reasoning depth to Enso's router / the model's own default.
    #[default]
    Auto,
    /// Disable extended reasoning (`off` / `none`).
    #[serde(alias = "none", alias = "instant")]
    Off,
    /// Lightest reasoning pass (`minimal`).
    Minimal,
    /// Quick pass with low reasoning (`low`).
    Low,
    /// Balanced thinking depth (`medium`).
    Medium,
    /// Deep reasoning for multi-step tasks (`high`).
    High,
    /// Extra-high reasoning depth (`xhigh`, e.g. Opus 4.7+ / o3 / GPT-5+ / Pi).
    #[serde(alias = "extra-high", alias = "extra_high")]
    XHigh,
    /// Maximum reasoning compute and exhaustive verification (`max`).
    Max,
}

/// Every reasoning level in canonical UI/CLI order.
pub const EFFORT_LEVELS: &[ReasoningEffort] = &[
    ReasoningEffort::Auto,
    ReasoningEffort::Off,
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

/// Every explicit (non-`auto`) reasoning level, in ascending intensity order.
pub const EXPLICIT_LEVELS: &[ReasoningEffort] = &[
    ReasoningEffort::Off,
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

/// Canonical CLI spelling list (including `none` as an explicit alias for `off`).
pub const CLI_EFFORT_VALUES: &[&str] = &[
    "auto", "off", "none", "minimal", "low", "medium", "high", "xhigh", "max",
];

impl ReasoningEffort {
    /// Parse a reasoning effort level case-insensitively, accepting common
    /// aliases (`none`/`instant` → `Off`, `extra-high` → `XHigh`, etc.).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "default" => Some(Self::Auto),
            "off" | "none" | "instant" | "disabled" | "0" => Some(Self::Off),
            "minimal" | "min" => Some(Self::Minimal),
            "low" | "fast" => Some(Self::Low),
            "medium" | "med" | "moderate" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "extra-high" | "extra_high" | "extrahigh" | "very-high" => Some(Self::XHigh),
            "max" | "maximum" | "ultracode" => Some(Self::Max),
            _ => None,
        }
    }

    /// Canonical lowercase identifier (`auto`, `off`, `minimal`, `low`,
    /// `medium`, `high`, `xhigh`, `max`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Human-readable display label matching web & desktop.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Off => "Off",
            Self::Minimal => "Minimal",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::XHigh => "Extra High",
            Self::Max => "Max",
        }
    }

    /// Short description of what this level does.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Auto => "Adaptive reasoning based on task complexity",
            Self::Off => "Disable extended thinking for instant answers",
            Self::Minimal => "Minimal reasoning tokens before answering",
            Self::Low => "Fast pass with light reasoning",
            Self::Medium => "Balanced reasoning for everyday tasks",
            Self::High => "Deep reasoning for complex multi-step tasks",
            Self::XHigh => "Extra-high reasoning depth for hard problems",
            Self::Max => "Maximum reasoning compute and exhaustive verification",
        }
    }

    /// Wire value for `dev` / `codex` / `interpreter` (`model_reasoning_effort`).
    /// `Auto` returns `None` so Enso's router / model default governs.
    pub const fn codex_wire(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Off => Some("none"),
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
            Self::Max => Some("max"),
        }
    }

    /// Wire value for Claude Code (`--effort` / `CLAUDE_CODE_EFFORT_LEVEL`).
    /// `Auto` leaves depth to Enso's per-tier router (`None`); `Off` disables
    /// thinking via `MAX_THINKING_TOKENS=0` instead of an effort flag.
    pub const fn claude_wire(self) -> Option<&'static str> {
        match self {
            Self::Auto | Self::Off => None,
            Self::Minimal | Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
            Self::Max => Some("max"),
        }
    }

    /// Whether Claude Code should explicitly disable extended thinking
    /// (`MAX_THINKING_TOKENS=0`).
    pub const fn claude_disables_thinking(self) -> bool {
        matches!(self, Self::Off)
    }

    /// Wire value for Pi / Oh My Pi (`--thinking <level>`).
    pub const fn pi_wire(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Off => Some("off"),
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
            Self::Max => Some("max"),
        }
    }

    /// Wire value sent to Hanzo Cloud (`/v1/agent/coding`) and `hanzod`.
    pub const fn cloud_wire(self) -> &'static str {
        self.as_str()
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| {
            format!(
                "invalid reasoning effort `{s}` (expected one of: {})",
                CLI_EFFORT_VALUES.join(", ")
            )
        })
    }
}

/// Clap `value_parser` accepting every canonical effort level and alias
/// (`auto`, `off`, `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`).
pub fn parse_cli_effort(raw: &str) -> Result<ReasoningEffort, String> {
    raw.parse()
}

/// Supported reasoning levels array for Codex `model_catalog_json` entries.
pub fn codex_supported_levels() -> Vec<serde_json::Value> {
    EXPLICIT_LEVELS
        .iter()
        .filter_map(|lvl| {
            lvl.codex_wire().map(|effort| {
                serde_json::json!({
                    "effort": effort,
                    "description": lvl.description(),
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_expanded_effort_level_and_aliases() {
        for (raw, want) in [
            ("auto", ReasoningEffort::Auto),
            ("default", ReasoningEffort::Auto),
            ("off", ReasoningEffort::Off),
            ("none", ReasoningEffort::Off),
            ("instant", ReasoningEffort::Off),
            ("minimal", ReasoningEffort::Minimal),
            ("low", ReasoningEffort::Low),
            ("medium", ReasoningEffort::Medium),
            ("high", ReasoningEffort::High),
            ("xhigh", ReasoningEffort::XHigh),
            ("extra-high", ReasoningEffort::XHigh),
            ("max", ReasoningEffort::Max),
            ("HIGH", ReasoningEffort::High),
            ("  xhigh ", ReasoningEffort::XHigh),
        ] {
            assert_eq!(ReasoningEffort::parse(raw), Some(want), "{raw}");
            assert_eq!(parse_cli_effort(raw).unwrap(), want, "{raw}");
        }
        let err = parse_cli_effort("extreme").unwrap_err();
        assert!(err.contains("xhigh") && err.contains("minimal") && err.contains("none"), "{err}");
    }

    #[test]
    fn backend_wire_mappings_cover_full_ladder() {
        assert_eq!(ReasoningEffort::Auto.codex_wire(), None);
        assert_eq!(ReasoningEffort::Off.codex_wire(), Some("none"));
        assert_eq!(ReasoningEffort::Minimal.codex_wire(), Some("minimal"));
        assert_eq!(ReasoningEffort::Low.codex_wire(), Some("low"));
        assert_eq!(ReasoningEffort::Medium.codex_wire(), Some("medium"));
        assert_eq!(ReasoningEffort::High.codex_wire(), Some("high"));
        assert_eq!(ReasoningEffort::XHigh.codex_wire(), Some("xhigh"));
        assert_eq!(ReasoningEffort::Max.codex_wire(), Some("max"));

        assert_eq!(ReasoningEffort::Auto.claude_wire(), None);
        assert!(ReasoningEffort::Off.claude_disables_thinking());
        assert_eq!(ReasoningEffort::Off.claude_wire(), None);
        assert_eq!(ReasoningEffort::Minimal.claude_wire(), Some("low"));
        assert_eq!(ReasoningEffort::Low.claude_wire(), Some("low"));
        assert_eq!(ReasoningEffort::Medium.claude_wire(), Some("medium"));
        assert_eq!(ReasoningEffort::High.claude_wire(), Some("high"));
        assert_eq!(ReasoningEffort::XHigh.claude_wire(), Some("xhigh"));
        assert_eq!(ReasoningEffort::Max.claude_wire(), Some("max"));

        assert_eq!(ReasoningEffort::Auto.pi_wire(), None);
        assert_eq!(ReasoningEffort::Off.pi_wire(), Some("off"));
        assert_eq!(ReasoningEffort::Minimal.pi_wire(), Some("minimal"));
        assert_eq!(ReasoningEffort::XHigh.pi_wire(), Some("xhigh"));
        assert_eq!(ReasoningEffort::Max.pi_wire(), Some("max"));
    }

    #[test]
    fn codex_supported_levels_advertises_all_explicit_levels() {
        let levels = codex_supported_levels();
        let ids: Vec<&str> = levels.iter().filter_map(|v| v["effort"].as_str()).collect();
        assert_eq!(ids, vec!["none", "minimal", "low", "medium", "high", "xhigh", "max"]);
    }
}
