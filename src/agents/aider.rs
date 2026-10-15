//! Aider. Under `--cloud` its model calls go to api.hanzo.ai as an OpenAI base
//! (`OPENAI_API_BASE`, `OPENAI_API_KEY`) and an `openai/` model. It takes no MCP
//! server and prints plain text.

use anyhow::Result;

use super::{wire, Agent, Run, Toolset};

pub const AGENT: Agent = Agent {
    name: "aider",
    aliases: &["aider-chat"],
    title: "Aider",
    programs: &["aider"],
    package: None,
    routable: true,
    toolset: Toolset::Never,
    build,
    read: wire::plain,
};

fn build(r: &mut Run) -> Result<()> {
    if let Some(g) = r.gateway() {
        r.env("OPENAI_API_BASE", g.v1()).env("OPENAI_API_KEY", g.token);
        // A gateway model is not in aider's own table; without this it warns, and
        // `--yes-always` answers its offer to open the docs in a browser.
        r.arg("--model").arg(format!("openai/{}", g.model)).arg("--no-show-model-warnings");
    }
    if r.auto() {
        r.arg("--yes-always");
    }
    if r.headless() {
        let task = r.task();
        r.arg("--message").arg(task);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test::*;
    use crate::commands::code::backend::Mode;

    #[test]
    fn aider_takes_the_gateway_as_an_openai_base_and_an_openai_model() {
        let l = build("aider", &spec(Mode::Headless, gateway()));
        assert_eq!(argv(&l), ["--model", "openai/enso", "--no-show-model-warnings", "--yes-always", "--message", "fix the test"]);
        assert_eq!(env(&l, "OPENAI_API_BASE").as_deref(), Some("https://api.hanzo.ai/v1"));
    }
}
