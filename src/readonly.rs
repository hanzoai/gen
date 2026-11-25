//! Read-only / wait-for-confirmation prompt directive detection for `hanzo-gen`.
//!
//! Ensures headless and supervised turns (`hanzo "do a read-only repo scan first and
//! wait for confirmation before writing code"`) never inject `NO_ONE_TO_ASK`, never
//! auto-escalate `sandbox_mode` to `"workspace-write"`, and never nudge Turn 1's
//! confirmation handoff into an unapproved Turn 2 edit loop.

pub(crate) const READ_ONLY_INSTRUCTIONS: &str = "The user explicitly requested a read-only scan, audit, or plan and wants confirmation before any files are modified. \
Inspect and analyze the repository using read-only commands and tools only. \
Do NOT modify, create, or delete any files, do NOT call apply_patch, and do NOT run file-mutating shell commands. \
End your turn with a clear summary of findings and proposed changes so the user can review and confirm.";

fn strip_code_and_quotes(prompt: &str) -> String {
    let mut out = String::with_capacity(prompt.len());
    let mut in_fence = false;
    for line in prompt.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || trimmed.starts_with('>') {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn normalize_ws(text: &str) -> String {
    text.to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns `true` when `prompt` explicitly instructs the agent to perform a read-only
/// scan/audit/review or wait for confirmation before modifying files.
pub(crate) fn requests_readonly_or_confirmation(prompt: &str) -> bool {
    let cleaned = strip_code_and_quotes(prompt);
    let norm = normalize_ws(&cleaned);
    if norm.is_empty() {
        return false;
    }

    const PROHIBITIONS: &[&str] = &[
        "do not edit",
        "do not modify",
        "do not write",
        "do not change",
        "do not patch",
        "do not touch",
        "do not create or modify",
        "do not make any changes",
        "do not make changes",
        "don't edit",
        "don't modify",
        "don't write",
        "don't change",
        "don't patch",
        "don't touch",
        "don't make any changes",
        "don't make changes",
        "never edit",
        "never modify",
        "never write files",
        "without editing",
        "without modifying",
        "without writing",
        "without changing",
        "without patching",
        "without touching",
        "without making any changes",
        "without making changes",
        "no file edits",
        "no file modifications",
        "no file changes",
        "no file writes",
        "no code changes",
        "make no changes",
    ];
    if PROHIBITIONS.iter().any(|pat| norm.contains(pat)) {
        return true;
    }

    const CONFIRMATION_GATES: &[&str] = &[
        "wait for confirmation",
        "wait for my confirmation",
        "wait for user confirmation",
        "wait for approval",
        "wait for my approval",
        "wait for user approval",
        "wait for permission",
        "wait for my permission",
        "wait for go-ahead",
        "wait for my go-ahead",
        "wait for sign-off",
        "await confirmation",
        "await my confirmation",
        "await approval",
        "await my approval",
        "pause for confirmation",
        "pause for approval",
        "stop for confirmation",
        "stop for approval",
        "stop and wait",
        "ask for confirmation before",
        "ask for my confirmation before",
        "ask for approval before",
        "ask for my approval before",
        "ask before editing",
        "ask before modifying",
        "ask before writing",
        "ask before patching",
        "ask before making",
        "confirm before editing",
        "confirm before modifying",
        "confirm before writing",
        "confirm before patching",
        "confirm before making",
        "confirm before applying",
        "confirm with me before",
        "get confirmation before",
        "get my confirmation before",
        "get approval before",
        "get my approval before",
        "until i confirm",
        "until i approve",
    ];
    if CONFIRMATION_GATES.iter().any(|pat| norm.contains(pat)) {
        return true;
    }

    let ro_norm = norm
        .replace("read-only", "readonly")
        .replace("read only", "readonly");
    if ro_norm.contains("readonly") {
        let starts_readonly = ro_norm.starts_with("readonly ")
            || ro_norm.starts_with("readonly:")
            || ro_norm.starts_with("readonly,")
            || ro_norm.starts_with("readonly -")
            || ro_norm.contains("(readonly)");
        const RO_AGENT_PHRASES: &[&str] = &[
            "do a readonly",
            "run a readonly",
            "perform a readonly",
            "make a readonly",
            "start with a readonly",
            "stay in readonly",
            "keep it readonly",
            "keep this readonly",
            "operate in readonly",
            "work in readonly",
            "readonly scan",
            "readonly repo scan",
            "readonly repository scan",
            "readonly codebase scan",
            "readonly project scan",
            "readonly workspace scan",
            "readonly audit",
            "readonly repo audit",
            "readonly codebase audit",
            "readonly review",
            "readonly repo review",
            "readonly codebase review",
            "readonly pass",
            "readonly analysis",
            "readonly inspection",
            "readonly check",
            "readonly exploration",
            "readonly assessment",
            "readonly survey",
            "readonly overview",
            "readonly first",
        ];
        if starts_readonly || RO_AGENT_PHRASES.iter().any(|pat| ro_norm.contains(pat)) {
            return true;
        }
    }

    false
}

/// Returns `true` when a follow-up steer/resume message authorizes proceeding with edits.
pub(crate) fn confirms_proceeding(prompt: &str) -> bool {
    if requests_readonly_or_confirmation(prompt) {
        return false;
    }
    let cleaned = strip_code_and_quotes(prompt);
    let norm = normalize_ws(&cleaned);
    let trimmed = norm.trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace());
    if trimmed.is_empty() {
        return false;
    }

    const EXACT: &[&str] = &[
        "y", "yes", "yep", "yeah", "yup", "ok", "okay", "sure", "go", "go ahead", "proceed",
        "continue", "approved", "approve", "confirmed", "confirm", "lgtm", "looks good",
        "sounds good", "do it", "ship it", "apply", "apply it", "fix it", "patch it",
    ];
    if EXACT.contains(&trimmed) {
        return true;
    }

    const PHRASES: &[&str] = &[
        "go ahead",
        "please proceed",
        "proceed with",
        "proceed and",
        "now apply",
        "please apply",
        "apply the ",
        "apply those",
        "apply these",
        "now implement",
        "implement the ",
        "now fix",
        "fix the ",
        "make the changes",
        "write the code",
        "you can now ",
        "feel free to ",
    ];
    let has_phrase = PHRASES.iter().any(|p| norm.contains(p));
    let negated = ["no", "nope", "wait", "hold", "stop", "not yet", "cancel"]
        .iter()
        .any(|neg| {
            trimmed == *neg
                || trimmed
                    .strip_prefix(neg)
                    .is_some_and(|r| r.starts_with(' ') || r.starts_with(','))
        });
    if negated && !has_phrase {
        return false;
    }
    if has_phrase {
        return true;
    }
    for lead in [
        "yes", "yep", "yeah", "ok", "okay", "sure", "approved", "confirmed", "lgtm",
        "looks good", "sounds good",
    ] {
        if trimmed
            .strip_prefix(lead)
            .is_some_and(|rest| rest.starts_with([' ', ',', '.', '-', ':', '!', ';']))
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_readonly_without_false_positives() {
        assert!(requests_readonly_or_confirmation(
            "Do a read-only repo scan first and wait for confirmation before writing code"
        ));
        assert!(requests_readonly_or_confirmation(
            "Audit the repo without modifying files"
        ));
        assert!(!requests_readonly_or_confirmation("do it"));
        assert!(!requests_readonly_or_confirmation(
            "Fix the read-only input bug in form.tsx"
        ));
        assert!(!requests_readonly_or_confirmation(
            "Add a --read-only flag to the CLI and write tests"
        ));
    }

    #[test]
    fn detects_confirmation_cleanly() {
        assert!(confirms_proceeding("Yes, proceed."));
        assert!(confirms_proceeding("go ahead and apply the changes"));
        assert!(!confirms_proceeding("wait, check tests first"));
        assert!(!confirms_proceeding(
            "yes, do a read-only check on auth.rs first"
        ));
    }
}
