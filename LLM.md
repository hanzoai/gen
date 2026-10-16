# Hanzo Gen

The agentic harness: `gen` launches a coding agent, attaches the Hanzo MCP
toolset, routes the agent's model calls through api.hanzo.ai, and streams the
session to mission control. Hanzo Dev (`hanzoai/dev`) is the default agent; Gen
is what launches and routes it and every other one. Crate `hanzo-gen` (lib
`hanzo_gen`, binary `gen`), MIT, public.

`hanzo dev` is this crate. hanzoai/cli depends on `hanzo-gen` by pinned git rev
and flattens [`Args`] onto `hanzo`, `hanzo dev`, `hanzo desktop` and `hanzo
sandbox run`, so every launcher takes the same flags and runs `hanzo_gen::run`.

## Shape

| file | what |
|---|---|
| `src/lib.rs` | `run(host, opts)`: resolve route, link the cloud session, equip MCP + kit, launch, stream, finalize, print the summary. `Options`, `warn`. |
| `src/args.rs` | `Args` (the clap flags), `Args::dev(command)` (tool is a flag), `Args::into_options`, `expand` (`--<agent>` → `--backend <agent>`). |
| `src/host.rs` | `Host` trait, `Account`, `Profile`, `Provider`, `BRAND`, and `host::Command` (the standalone host). |
| `src/main.rs` | the `gen` binary: `__mcp` proxy, `expand`, parse, `dev("gen")`, `run(host::Command)`. |
| `src/backend.rs` | `BackendKind`, `select` (THE one place a backend is resolved), `Spec`, `Route`/`Routing`, `resolve_mcp`. |
| `src/agents/` | one adapter per third-party agent + `wire.rs`; `testdata/*.jsonl` are real captured streams. |
| `src/claude.rs`, `src/dev.rs` | Claude Code; `dev`/codex/interpreter (the codex grammar). |
| `src/mcp.rs` | the lazy MCP proxy (`<exe> __mcp -- hanzo-mcp …`). Both `gen` and `hanzo` route `__mcp` here before parsing. |
| `src/kit.rs` | the org's kit: skills written where the backend reads them, cloud tools as one MCP server. |
| `src/session.rs`, `event.rs`, `control.rs` | `/v1/agents/sessions` client, event vocabulary, remote steer/stop. |
| `src/target.rs`, `context.rs`, `sample.rs` | run-target registry + heartbeat (`beat`, `present`), machine snapshot, live metrics. `hanzo up` and `hanzo link` use them too. |
| `src/harness.rs`, `settings.rs`, `native.rs` | connected accounts + usage line, `~/.hanzo/settings.json`, native agent sign-in table (`hanzo auth login` reads it). |
| `src/summary.rs`, `savings.rs`, `ledger.rs` | end-of-session block; prices/tallies/meter rows/number formatting (the console uses them too); MCP tool-call ledger. |
| `src/private.rs` | THE owner-only atomic file write (`write`, `create`). The CLI writes its credential store and config through it. |
| `src/theme.rs`, `home.rs`, `tier.rs` | Claude theme + config home per route; model tiers. |
| `src/testmock.rs` | `#[cfg(test)]` hand-rolled cloud over TCP. |

## The Host

Gen signs nobody in and stores no credential. Everything account-shaped comes
from the program it runs in:

```rust
pub trait Host: Send + Sync {
    fn profile(&self) -> Profile;                                   // api, local, org, provider, identities, link, theme, refresh
    fn account<'a>(&'a self, brand: &'a str) -> Pending<'a, Result<Option<Account>>>;  // fresh token
    fn key(&self, provider: Provider) -> Result<Option<String>>;    // stored provider key, read lazily
    fn meter<'a>(&'a self, query: &'a [String]) -> Pending<'a, Result<Value, String>>; // GET /v1/usage/summary data
}
```

- **hanzo-cli** (`commands::dev::Store`): its IAM store (`store::active_token`,
  refreshing), its vault (`provider::key`), its network/org/`code.*` config, its
  authenticated seam for the meter. `refresh` is `current_exe auth token`.
- **gen** (`host::Command`): the `hanzo` command on PATH, as Hanzo Dev does it.
  `account` = `hanzo auth token [--brand B]`, owner/name read off the token's
  claims as a label (never a decision; the server verifies). `key(Hanzo)` =
  `HANZO_API_KEY`. `meter` = `hanzo usage summary --range … --start … --end …`.
  `profile.api` = `https://api.hanzo.ai`, `refresh` = `hanzo auth token`. No CLI
  on PATH = signed out: the session runs unlinked and says so.

`Profile.refresh` matters: codex-grammar agents on a custom provider are told
`auth.command`/`auth.args` so they re-ask for the hour-long IAM token. It must
name the token's owner (the CLI), never `gen` itself.

## Rules

- Gen depends on nothing in hanzo-cli. `hanzo-client` (the platform client) is
  a separate crate in hanzoai/cli's tree, consumed by pinned rev as it intends;
  hanzo-cli `[patch]`es that git source to its own path so one copy links.
- One way each: `warn`, `private`, `savings` formatting, `native` live here and
  the CLI uses them. The CLI keeps only its seam-bound `savings::summary`.
- Strings say `hanzo dev` / `hanzo --resume`: they are the CLI's surface and the
  CLI resumes a `gen` session from the same `~/.hanzo` store.
- Tests are hermetic: `testmock` for the cloud, a stand-in `hanzo` script for
  `host::Command`. Some target tests write a record under the data dir and
  remove it.

## Work

    cargo build            # target/debug/gen
    cargo test
    cargo clippy --all-targets -- -D warnings

## Release

The version is set by hand in Cargo.toml and the tag `vX.Y.Z` names it;
`.github/workflows/release.yml` refuses a mismatch and mints nothing. It builds
five targets from one amd64 runner (musl + darwin via cargo-zigbuild, msvc via
cargo-xwin), sccache through hanzoai/ci `bin/cache`, uploads
`gen-<os>-<arch>.tar.gz` + `.sha256` into a draft, verifies all ten, smokes the
linux binary and publishes. hanzo.sh installs `gen` once a release carries this
machine's archive.

After a release: bump hanzoai/cli's `hanzo-gen` rev to the tag's commit.
