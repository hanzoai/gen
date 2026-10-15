//! Crush. Its configuration for a run is a `crush.json` in a data directory of
//! Hanzo's own (`--data-dir ~/.hanzo/crush`): the toolset, and under `--cloud`
//! a `hanzo` provider whose key it reads from `$HANZO_API_KEY`. It prints plain
//! text, and its `--yolo` belongs to its screen: `crush run` refuses it.

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
    toolset: Toolset::Always,
    build,
    read: wire::plain,
};

fn build(r: &mut Run) -> Result<()> {
    let dir = r.home("crush")?;
    let mut doc = json!({});
    if let Some(m) = r.mcp() {
        doc["mcp"] = json!({ "hanzo": { "type": "stdio", "command": m.program, "args": m.args } });
    }
    let gateway = r.gateway();
    if let Some(g) = &gateway {
        doc["providers"] = json!({ "hanzo": {
            "name": "Hanzo",
            "type": "openai-compat",
            "base_url": g.v1(),
            "api_key": format!("${KEY}"),
            "models": [{ "id": g.model, "name": g.model }],
        } });
        r.env(KEY, g.token);
    }
    r.write(&dir.join("crush.json"), &doc)?;
    r.arg("--data-dir").arg(&dir);
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
    use crate::commands::code::backend::{Mode, Route};

    #[test]
    fn crush_keeps_its_own_flags_ahead_of_run_and_yolo_on_its_screen() {
        let a = argv(&build("crush", &spec(Mode::Headless, gateway())));
        assert_eq!(a[0], "--data-dir");
        assert_eq!(a[2..], ["run", "-m", "hanzo/enso", "fix the test"]);
        assert_eq!(argv(&build("crush", &spec(Mode::Interactive, Route::Inherit)))[2..], ["--yolo"]);
    }

    #[test]
    fn its_plain_text_is_its_reply() {
        assert_eq!(said(&read("crush", "\u{1b}[0mmango\n\n")), ["mango"]);
    }
}
