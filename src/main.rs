//! `gen` — start a coding session from the terminal, as `hanzo dev` does.

use std::sync::Arc;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser};

/// Hanzo Gen, the agentic harness: launch and route any coding agent with the
/// Hanzo MCP toolset attached, its model calls through api.hanzo.ai, and the
/// session streamed live to mission control
///
/// Runs our own `dev` agent by default. Name another tool with a flag:
///
///   gen              our own agent (the default)
///   gen --claude     gen --codex     gen --cursor     gen --agy
///   gen --runc       (bare metal host with GPU pass-through)
///   gen --claude -- -c    (args after `--` go to the tool)
///
/// A trailing task runs headless (`gen --claude "fix the failing
/// test"`); omit it for an interactive session. Sign in with `hanzo auth login`.
#[derive(Parser)]
#[command(name = "gen", version, verbatim_doc_comment)]
struct Cli {
    #[command(flatten)]
    args: hanzo_gen::Args,
}

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    // The lazy MCP proxy a coding agent is handed in place of hanzo-mcp: it runs
    // before the command is built, since the agent waits on it.
    if argv.get(1).map(String::as_str) == Some("__mcp") {
        return hanzo_gen::mcp::main(hanzo_gen::Root::user(), &argv[2..]).await;
    }
    let argv = hanzo_gen::args::expand(&Cli::command(), argv);
    let cli = Cli::from_arg_matches(&Cli::command().get_matches_from(argv))?;
    let opts = cli.args.dev("gen")?.into_options(hanzo_gen::Root::need()?)?;
    hanzo_gen::run(Arc::new(hanzo_gen::host::Command::found().await), opts).await
}
