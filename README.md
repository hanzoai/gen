# Hanzo Gen

The agentic harness. `gen` launches a coding agent in your terminal with the
Hanzo MCP toolset attached, routes its model calls through api.hanzo.ai, and
streams the session live to mission control. Hanzo Dev is the default agent;
Claude Code, Codex, Cursor, Antigravity, Gemini, Qwen, OpenCode and the rest are
one flag away.

`hanzo dev` is Gen too: the Hanzo CLI links this crate, so `gen` and `hanzo dev`
take the same flags and run the same session.

## Install

```sh
curl -fsSL hanzo.sh | sh
```

Gen signs in with your Hanzo account, the same one Hanzo Dev and the Hanzo CLI
use. Sign in once:

```sh
hanzo auth login
```

## Use

```sh
gen                          # an interactive Hanzo Dev session
gen "fix the failing test"   # a task, run headless
gen --claude                 # Claude Code
gen --codex "add a test"     # Codex, headless
gen --cursor                 # Cursor's agent
gen --gemini                 # any agent is a flag
gen --claude -- -c           # args after -- go to the agent
gen --resume <id>            # pick a linked session back up
gen --help                   # every flag
```

## Routing

Every agent's model calls go to api.hanzo.ai, which serves Enso, Zen and Kai,
so one account and one bill cover every agent. Hanzo Dev always routes there.
A third-party agent keeps its own login unless you pass `--cloud`; with it, its
calls route through api.hanzo.ai like Dev's. `--model enso-ultra` picks the model
on that route.

When you are signed in, a session is registered with Hanzo cloud and its events
stream to mission control; `--no-link` keeps it local.

## Build

```sh
cargo build --release
cargo test
```

## License

MIT
