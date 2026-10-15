//! Continue's CLI (`cn`). It keeps its own Continue login, takes no MCP server
//! for one run, and prints plain text: its `--format json` is one closing
//! document, not a stream of events.

use anyhow::Result;

use super::{wire, Agent, Run, Toolset, Upgrade};

pub const AGENT: Agent = Agent {
    name: "continue",
    aliases: &["cn"],
    title: "Continue CLI",
    programs: &["cn"],
    upgrade: Upgrade::Npm("@continuedev/cli"),
    routable: false,
    toolset: Toolset::Never,
    build,
    read: wire::plain,
};

fn build(r: &mut Run) -> Result<()> {
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
    fn continue_takes_its_task_by_p() {
        assert_eq!(argv(&build("continue", &spec(Mode::Headless, Route::Inherit))), ["-p", "fix the test"]);
    }
}
