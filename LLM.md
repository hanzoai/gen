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
| `src/root.rs` | `Root`: every path Gen writes under a home, from one value the launcher picks (`Root::user()` / `Root::need()` in a `main`, `Root::at(dir)` in tests). Reads `$HOME` itself: an unset, empty or relative home is no home. |
| `src/exe.rs` | THE program resolver: `find` (full path, absolute PATH entries only), `confine` (a child's PATH), `version` (with a time limit). Every spawn and lookup goes through it; `lib.rs`'s `spawn` refuses a program not named by full path. |
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
    fn profile(&self) -> Pending<'_, Profile>;                      // api, local, org, provider, identities, link, theme
    fn account<'a>(&'a self, brand: &'a str) -> Pending<'a, Result<Option<Account>>>;  // fresh token
    fn refresh(&self, brand: &str) -> Vec<String>;                  // `<cli> auth token --brand B`
    fn key(&self, provider: Provider) -> Result<Option<String>>;    // stored provider key, read lazily
    fn meter<'a>(&'a self, brand: &'a str, query: &'a [String]) -> Pending<'a, Result<Value, String>>; // GET /v1/usage/summary data, as brand
}
```

- **hanzo-cli** (`commands::dev::Store`): its IAM store (`store::active_token`,
  refreshing, on ONE config behind one lock held for the whole ask, so an IAM
  identity move made by one ask is never overwritten by another),
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
  `key(Hanzo)` = `HANZO_API_KEY`. `meter` = `hanzo usage summary --range … --start … --end …`
  for the default brand; another brand's meter is refused rather than read wrong
  (the CLI's generated commands take no brand).

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

**Each Hanzo-routed launch of an agent that reads a home (grok, cline, hermes,
omp, pi, crush) gets its own**, `~/.hanzo/<agent>/run-<pid>-XXXX`, mode 0700, held
in `Launch.cleanup` (`Scratch { files, dirs }`) and removed when the session ends.
Its config names that session's `--project-dir` and `--ledger`; a shared home let
one session's tools act on another's repository. On its own account an agent gets
no home of ours (crush keeps its own data directory and history). While a child
runs, `control::relay` holds SIGINT, SIGTERM and SIGHUP (passing TERM and HUP to
the child) so the session always closes out and removes its homes; a SIGKILLed
one leaves `run-<pid>-*`, which the next run of that agent sweeps once the pid is
gone.

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
cargo-xwin), verifies all ten files, smokes the linux binary, attests the five
archives and publishes. hanzo.sh installs `gen` once a release carries this
machine's archive.

Permissions are per job: nothing by default; `version` and `build` read only,
hold no secret and restore no cache (builds run cold: a build compiles
third-party build scripts, and a cache it trusted would be code anyone who can
write the store could plant). Each build hands publish its archive as an artifact
named for its target and the archive's sha256 as a job output; publish downloads
each artifact by exact name, refuses an archive whose digest is not its build's,
attests the five archives (`actions/attest-build-provenance`), and only then
opens, fills and publishes the release. Actions, and hanzoai/ci in `cicd.yml`,
are pinned by commit. hanzoai/cli's install.sh verifies a gen download's
attestation against `.github/workflows/release.yml` at its tag with a signed-in
`gh` (hanzo.sh names the signer in gen's row).

Repository: immutable releases on; rulesets keep `main` from force-push and
deletion (no bypass), require a pull request on `main` from anyone but an admin,
and let only an admin create `v*` tags, which nobody can move or delete; the
default workflow token is read-only.

**The org build cache is code a build trusts, and more can write it than any one
repository (which is why this release does not use it).** hanzoai/ci `bin/cache`
keeps sccache objects in the org's bucket under `<GITHUB_REPOSITORY>/<name>/<key>`
and `…/latest`, and it decides "trusted ref" from the job's own environment: any
tag, or the default branch. The prefix is the caller's to declare and the IAM
identity is the org's, so any workflow in the org holding the KMS pair can write
another repository's prefix, and a later build restores `latest` and links what
it finds. The fix belongs in hanzoai/ci and the store (server-side per-repo
scoping).

After a release: bump hanzoai/cli's `hanzo-gen` rev to the tag's commit.
