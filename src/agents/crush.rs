//! Crush. On its own account it runs as itself, in its own data directory, with
//! its history. Routed through Hanzo (`--cloud`) its configuration for the run is
//! a `crush.json` in a data directory of this run's own (`--data-dir`): a `hanzo`
//! provider whose key it reads from `$HANZO_API_KEY`, and the toolset. It prints
//! plain text, and its `--yolo` belongs to its screen: `crush run` refuses it.

use anyhow::Result;
use serde_json::json;

use super::{wire, Agent, Run, Toolset, Upgrade, KEY};

pub const AGENT: Agent = Agent {
    name: "crush",
    aliases: &[],
    title: "Crush",
    programs: &["crush"],
    upgrade: Upgrade::Npm("@charmland/crush"),
    routable: true,
    toolset: Toolset::Routed,
    build,
    read: wire::plain,
};

fn build(r: &mut Run) -> Result<()> {
    let gateway = r.gateway();
    if let Some(g) = &gateway {
        let dir = r.home("crush")?;
        let mut doc = json!({ "providers": { "hanzo": {
            "name": "Hanzo",
            "type": "openai-compat",
            "base_url": g.v1(),
            "api_key": format!("${KEY}"),
            "models": [{ "id": g.model, "name": g.model }],
        } } });
        if let Some(m) = r.mcp() {
            doc["mcp"] = json!({ "hanzo": { "type": "stdio", "command": m.program, "args": m.args } });
        }
        r.env(KEY, g.token);
        r.write(&dir.join("crush.json"), &doc)?;
        r.arg("--data-dir").arg(&dir);
    }
    if r.headless() {
        r.arg("run");
        if let Some(g) = &gateway {
            r.arg("-m").arg(format!("hanzo/{}", g.model));
        }
        let task = r.task();
        r.arg(task);
    } else if r.auto() {
        r.arg("--yolo");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::backend::{Mode, Route};

    #[test]
    fn crush_keeps_its_own_flags_ahead_of_run_and_yolo_on_its_screen() {
        let a = argv(&build("crush", &spec(Mode::Headless, gateway())));
        assert_eq!(a[0], "--data-dir");
        assert_eq!(a[2..], ["run", "-m", "hanzo/enso", "fix the test"]);
        // On its own account it keeps its own data directory, and so its history.
        assert_eq!(argv(&build("crush", &spec(Mode::Interactive, Route::Inherit))), ["--yolo"]);
    }

    #[test]
    fn its_plain_text_is_its_reply() {
        assert_eq!(said(&read("crush", "\u{1b}[0mmango\n\n")), ["mango"]);
    }
}
