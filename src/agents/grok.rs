//! Grok Build, xAI's CLI. It keeps its own xAI login, takes no MCP server for one
//! run and prints plain text.

use anyhow::Result;

use super::{wire, Agent, Run, Toolset, Upgrade};

pub const AGENT: Agent = Agent {
    name: "grok",
    aliases: &["grok-build", "xai"],
    title: "Grok Build",
    programs: &["grok"],
    upgrade: Upgrade::Own(&["update"]),
    routable: false,
    toolset: Toolset::Never,
    build,
    read: wire::plain,
};

fn build(r: &mut Run) -> Result<()> {
    if r.auto() {
        r.arg("--always-approve");
    }
    if r.headless() {
        let task = r.task();
        r.arg("-p").arg(task);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::{Mode, Route};

    #[test]
    fn grok_takes_its_task_by_p() {
        assert_eq!(argv(&build("grok", &spec(Mode::Headless, Route::Inherit))), ["--always-approve", "-p", "fix the test"]);
    }
}
