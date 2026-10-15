//! Kimi Code. It keeps its own Moonshot login, takes no MCP server for one run
//! and prints plain text. Its `--yolo` belongs to its screen: it refuses it beside
//! a task.

use anyhow::Result;

use super::{wire, Agent, Run, Toolset, Upgrade};

pub const AGENT: Agent = Agent {
    name: "kimi",
    aliases: &["kimi-code"],
    title: "Kimi Code",
    programs: &["kimi"],
    upgrade: Upgrade::Npm("@moonshot-ai/kimi-code"),
    routable: false,
    toolset: Toolset::Never,
    build,
    read: wire::plain,
};

fn build(r: &mut Run) -> Result<()> {
    if r.headless() {
        let task = r.task();
        r.arg("-p").arg(task);
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
    fn kimi_acts_without_asking_on_its_screen_only() {
        assert_eq!(argv(&build("kimi", &spec(Mode::Headless, Route::Inherit))), ["-p", "fix the test"]);
        assert_eq!(argv(&build("kimi", &spec(Mode::Interactive, Route::Inherit))), ["--yolo"]);
    }
}
