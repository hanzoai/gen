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
| `src/host.rs` | `Host` trait, `Account`, `Profile`, `Provider`, `BRAND`, `token(brand)`, `claim` (THE owner/name validator, the CLI's too), and `host::Command` (the standalone host). |
| `src/root.rs` | `Root`: every path Gen writes under a home, from one value the launcher picks (`Root::user()` / `Root::need()`, or `Root::at(dir)` in tests). |
| `src/main.rs` | the `gen` binary: `__mcp` proxy, `expand`, parse, `dev("gen")`, `into_options(Root::need()?)`, `run(host::Command::found())`. |
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
    fn profile(&self) -> Profile;                                   // api, local, org, provider, identities, link, theme
    fn account<'a>(&'a self, brand: &'a str) -> Pending<'a, Result<Option<Account>>>;  // fresh token
    fn refresh(&self, brand: &str) -> Vec<String>;                  // `<cli> auth token --brand B`
    fn key(&self, provider: Provider) -> Result<Option<String>>;    // stored provider key, read lazily
    fn meter<'a>(&'a self, query: &'a [String]) -> Pending<'a, Result<Value, String>>; // GET /v1/usage/summary data
}
```

- **hanzo-cli** (`commands::dev::Store`): its IAM store (`store::active_token`,
  refreshing, on ONE config kept across asks so an IAM identity move sticks),
  its vault (`provider::key`), its network/org/`code.*` config, its
  authenticated seam for the meter. `refresh` is `current_exe auth token --brand B`.
  `hanzo config profile` (hidden) prints `Store::profile()` as JSON.
- **gen** (`host::Command`): the `hanzo` command on PATH, found by full path in
  absolute PATH entries only, asked with a 20 s limit. `found()` reads
  `hanzo config profile` once (the CLI is the ONE reader of its config); a CLI
  that cannot say leaves `Profile::unread()`: api.hanzo.ai and NOT linked.
  `account` = `hanzo auth token --brand B`, owner/name read off the token and held
  to `claim` (a label, never a decision). "not signed in" is `None`; any other
  refusal is the CLI's own words and stops the session, as `hanzo dev` does.
  `key(Hanzo)` = `HANZO_API_KEY`. `meter` = `hanzo usage summary --range … --start … --end …`.

The brand rides everything a token is asked for: the session's account, the
refresh command codex-grammar agents re-run (`auth.command`/`auth.args`), and the
run-target beat. It must name the token's owner (the CLI), never `gen` itself.

## Where state lives

One `Root` per session, in `Options.root`, chosen by the launcher (`gen`'s main,
the CLI's `commands::dev::session`, the `__mcp` proxy's own entry). Everything
under it: `~/.hanzo/settings.json`, `~/.hanzo/sessions/*.jsonl` (ledgers),
`~/.hanzo/kit.json`, `~/.hanzo/claude` (a routed Claude's home),
`~/.hanzo/<agent>/run-*` (below), `~/.agents/skills`, `~/.claude`, and under the
data dir `code/sessions`, `code/targets`, `machine-id`. `root_is_the_one_resolver`
fails the build if anything else resolves `$HOME`; tests use `root::scratch()`, a
temp dir per test process.

**Each launch of an agent that reads a home (grok, cline, hermes, omp, pi, crush)
gets its own**, `~/.hanzo/<agent>/run-XXXX`, owner-only, held in `Launch.cleanup`
(`Scratch { files, dirs }`) and removed when the session ends. Its config names
that session's `--project-dir` and `--ledger`; a shared home let one session's
tools act on another's repository. Cost: those agents keep no history between
sessions run through Hanzo (`hanzo --resume` never resumed them), and a SIGKILLed
session leaves its `run-*` behind.

## Rules

- Gen depends on nothing in hanzo-cli. `hanzo-client` (the platform client) is
  a separate crate in hanzoai/cli's tree, consumed by pinned rev as it intends;
  hanzo-cli `[patch]`es that git source to its own path so one copy links.
- One way each: `warn`, `private`, `savings` formatting, `native` live here and
  the CLI uses them. The CLI keeps only its seam-bound `savings::summary`.
- Strings say `hanzo dev` / `hanzo --resume`: they are the CLI's surface and the
  CLI resumes a `gen` session from the same `~/.hanzo` store.
- Tests never touch a home: `testmock` for the cloud, a stand-in `hanzo` script
  for `host::Command`, `root::scratch()` for every path. hanzo.yml runs the suite
  with HOME an empty read-only dir, then an empty writable one that must still be
  empty after; locally, the suite also passes with the real HOME bind-mounted
  read-only (bwrap, TMPDIR outside it).

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

Permissions are per job: nothing by default; `version` and `build` read only and
hand archives over as artifacts (retention one day); `publish` alone writes, and
attests the five archives (`actions/attest-build-provenance`; check one with
`gh attestation verify gen-linux-amd64.tar.gz -R hanzoai/gen`). Actions are pinned
by commit. Repository rules protect `main` (no force-push, no deletion) and `v*`
tags (no update, no deletion); the default workflow token is read-only.

**The build cache is code the release trusts, and more can write it than this
repository.** hanzoai/ci `bin/cache` keeps sccache objects in the org's bucket
under `<GITHUB_REPOSITORY>/<name>/<key>` and `…/latest`, and it decides "trusted
ref" from the job's own environment: any tag, or the default branch. The prefix is
the caller's to declare and the IAM identity is the org's, so any workflow in the
org holding the KMS pair can write `hanzoai/gen/*`, and a pushed tag of any name
here writes too; a later release restores `latest` and links what it finds.
Nothing in this repo fixes that; the fix belongs in hanzoai/ci and the store
(server-side per-repo scoping, or releases that restore nothing).

After a release: bump hanzoai/cli's `hanzo-gen` rev to the tag's commit.
