//! The coding agents `hanzo` launches beyond its own (`dev`, `claude`, `codex`,
//! `interpreter`): one file each, and adding an agent is adding a file and a line
//! in [`AGENTS`].
//!
//! A file holds everything about its agent: its names, the binary that runs it,
//! where `hanzo upgrade` installs it from, the command line, environment and
//! files of one run, and how its output reads when the run is streamed to the
//! cloud. No agent reads another's options, so changing how one is launched
//! cannot change another. What they share is plumbing ([`Run`], [`Reader`]) and
//! the wire formats more than one of them speaks ([`wire`]).

use anyhow::{Context, Result};
use serde_json::Value;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::backend::{Approval, Backend, Launch, McpAttach, Mode, Route, Routing, Spec};
use crate::root::Root;
use super::event::{Mapped, Usage};

mod agy;
mod aider;
mod amp;
mod cline;
mod cn;
mod copilot;
mod crush;
mod cursor;
mod gemini;
mod goose;
mod grok;
mod hermes;
mod kilo;
mod kimi;
mod kode;
mod letta;
mod omp;
mod opencode;
mod openhands;
mod pi;
mod qwen;
pub mod wire;

/// Every agent, in the order help lists them.
pub const AGENTS: &[Agent] = &[
    cursor::AGENT,
    agy::AGENT,
    gemini::AGENT,
    qwen::AGENT,
    opencode::AGENT,
    kilo::AGENT,
    crush::AGENT,
    pi::AGENT,
    copilot::AGENT,
    amp::AGENT,
    grok::AGENT,
    goose::AGENT,
    aider::AGENT,
    hermes::AGENT,
    cline::AGENT,
    cn::AGENT,
    kimi::AGENT,
    omp::AGENT,
    letta::AGENT,
    kode::AGENT,
    openhands::AGENT,
];

/// When the Hanzo toolset reaches an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toolset {
    /// It has no way to take a server for one run.
    Never,
    /// On every run.
    Always,
    /// Only on a run routed through Hanzo, when it reads a home of Hanzo's own.
    Routed,
}

/// How `hanzo upgrade` brings an agent to its current release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upgrade {
    /// `npm install` of this package under `~/.hanzo`, its binary in `~/.hanzo/bin`.
    Npm(&'static str),
    /// Its own updater, these arguments to its binary, as in `goose update`.
    Own(&'static [&'static str]),
    /// `uv tool upgrade` of this package, for an agent installed with `uv tool install`.
    Uv(&'static str),
}

/// One agent.
#[derive(Clone, Copy)]
pub struct Agent {
    /// The backend name: `hanzo --<name>`, `--backend <name>`.
    pub name: &'static str,
    /// Other spellings of the same agent.
    pub aliases: &'static [&'static str],
    pub title: &'static str,
    /// Binaries to try, in order.
    pub programs: &'static [&'static str],
    /// How `hanzo upgrade <name>` brings it to its current release.
    pub upgrade: Upgrade,
    /// Whether its own model calls can go through api.hanzo.ai under `--cloud`.
    pub routable: bool,
    pub toolset: Toolset,
    /// The command line, environment and files of one run.
    pub build: fn(&mut Run) -> Result<()>,
    /// One line of its output on a run streamed to the cloud.
    pub read: fn(&mut Reader, &str) -> Vec<Mapped>,
}

impl PartialEq for Agent {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Agent {}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

impl Agent {
    /// Whether the Hanzo toolset reaches this agent on a run, `routed` or not.
    pub fn takes_mcp(&self, routed: bool) -> bool {
        match self.toolset {
            Toolset::Never => false,
            Toolset::Always => true,
            Toolset::Routed => routed && self.routable,
        }
    }
}

/// The agent a spelling names: its name or one of its aliases, any case.
pub fn find(name: &str) -> Option<&'static Agent> {
    let name = name.trim().to_ascii_lowercase();
    AGENTS.iter().find(|a| a.name == name || a.aliases.contains(&name.as_str()))
}

/// Where `hanzo upgrade` installs agents that ship on npm: `~/.hanzo`, so their
/// binaries land in `~/.hanzo/bin` and nothing global is touched.
pub fn prefix(root: &Root) -> PathBuf {
    root.hanzo()
}

/// The binary that runs one of `programs`: the copy `hanzo upgrade` keeps in
/// `~/.hanzo/bin` first, so upgrading it changes what runs, else the first on PATH.
pub fn find_program(root: &Root, programs: &[&str]) -> Option<PathBuf> {
    let bin = prefix(root).join("bin");
    programs
        .iter()
        .map(|p| bin.join(p))
        .find(|p| p.is_file())
        .or_else(|| programs.iter().find_map(|p| crate::exe::find(p)))
}

/// The binary that runs `a`.
pub fn program(root: &Root, a: &Agent) -> Option<PathBuf> {
    find_program(root, a.programs)
}

/// The variable a routed agent's configuration names for the bearer, so the key is never written down.
pub const KEY: &str = "HANZO_API_KEY";

/// Where a routed run's model calls go.
pub struct Gateway<'a> {
    pub api: &'a str,
    pub token: &'a str,
    pub model: &'a str,
}

impl Gateway<'_> {
    /// The gateway's OpenAI-compatible base: `{api}/v1`.
    pub fn v1(&self) -> String {
        format!("{}/v1", self.api.trim_end_matches('/'))
    }
}

/// One run being put together: the agent's file adds its arguments, variables
/// and files, and [`Session::build`] closes it with the person's own arguments.
pub struct Run<'a> {
    spec: &'a Spec,
    cmd: tokio::process::Command,
    cleanup: super::backend::Scratch,
}

impl<'a> Run<'a> {
    fn new(spec: &'a Spec, program: &str) -> Run<'a> {
        let mut cmd = tokio::process::Command::from(crate::exe::command(program));
        cmd.current_dir(&spec.cwd);
        Run { spec, cmd, cleanup: Default::default() }
    }

    pub fn arg(&mut self, a: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.arg(a);
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(&mut self, args: I) -> &mut Self {
        self.cmd.args(args);
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env(key, value);
        self
    }

    /// Act without asking: the person's approval is auto or bypass.
    pub fn auto(&self) -> bool {
        matches!(self.spec.approval, Approval::Auto | Approval::Bypass)
    }

    /// A run with a task that ends when the task does.
    pub fn headless(&self) -> bool {
        self.spec.mode == Mode::Headless
    }

    /// Whether the run is streamed to the cloud and so should print structured events.
    pub fn structured(&self) -> bool {
        self.spec.structured
    }

    pub fn task(&self) -> &'a str {
        self.spec.task.as_deref().unwrap_or_default()
    }

    /// The Hanzo toolset's server, when one is attached.
    pub fn mcp(&self) -> Option<&'a McpAttach> {
        self.spec.mcp.as_ref()
    }

    /// The gateway, when this run's model calls go through Hanzo.
    pub fn gateway(&self) -> Option<Gateway<'a>> {
        match &self.spec.routing {
            Route::Via(Routing::Gateway { api, token, model, .. }) => Some(Gateway { api, token, model }),
            _ => None,
        }
    }

    /// A file of this run's, removed when the session ends.
    pub fn file(&mut self, doc: &Value) -> Result<PathBuf> {
        let mut f = tempfile::Builder::new().prefix("hanzo-agent-").suffix(".json").tempfile().context("creating a config file")?;
        f.write_all(doc.to_string().as_bytes()).context("writing a config file")?;
        let path = f.into_temp_path();
        let p = path.to_path_buf();
        self.cleanup.files.push(path);
        Ok(p)
    }

    /// A home of Hanzo's own for this one run of an agent, under
    /// `~/.hanzo/<name>/`, owner-only, removed when the session ends. Its config
    /// names this session's project and ledger, so a second session of the same
    /// agent gets a home of its own rather than rewriting this one's.
    ///
    /// The session removes it on any ending it lives through (a signal included,
    /// see `control::relay`). One it does not (SIGKILL, power) is left behind with
    /// its lock free, and the next run of that agent sweeps it.
    pub fn home(&mut self, name: &str) -> Result<PathBuf> {
        let parent = self.spec.root.hanzo().join(name);
        super::kit::private_dir(&parent)?;
        sweep(&parent);
        let (d, lock) = claim(&parent).with_context(|| format!("making a home for {name} in {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            use std::os::unix::io::AsRawFd;
            std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("making {} owner-only", d.path().display()))?;
            // The agent holds the lock too, so its home outlives Gen as long as it does.
            let fd = lock.as_raw_fd();
            // SAFETY: fcntl(2) is async-signal-safe, which is all pre_exec asks.
            unsafe {
                self.cmd.pre_exec(move || match libc::fcntl(fd, libc::F_SETFD, 0) {
                    -1 => Err(std::io::Error::last_os_error()),
                    _ => Ok(()),
                });
            }
        }
        let path = d.path().to_path_buf();
        self.cleanup.locks.push(lock);
        self.cleanup.dirs.push(d);
        Ok(path)
    }

    /// Write `doc` to `path`, readable by this user only.
    pub fn write(&self, path: &Path, doc: &Value) -> Result<()> {
        crate::private::write(path, doc.to_string().as_bytes()).with_context(|| format!("writing {}", path.display()))
    }
}

/// The file in a run home whose lock says a session lives in it.
const LOCK: &str = "gen.lock";

/// A new run home in `parent` with its lock held. A sweep may take a home in the
/// moment between its lock file being made and being locked, so a lock is good
/// only if its file is still there once it is held.
fn claim(parent: &Path) -> std::io::Result<(tempfile::TempDir, std::fs::File)> {
    loop {
        let d = tempfile::Builder::new().prefix("run-").tempdir_in(parent)?;
        let lock = std::fs::File::create_new(d.path().join(LOCK))?;
        match lock.try_lock() {
            Ok(()) if d.path().join(LOCK).is_file() => return Ok((d, lock)),
            Ok(()) | Err(std::fs::TryLockError::WouldBlock) => continue,
            // Where nothing can be locked, nothing is swept either.
            Err(std::fs::TryLockError::Error(_)) => return Ok((d, lock)),
        }
    }
}

/// Remove the run homes in `parent` that no session holds: a home goes only once
/// its lock can be taken, which no pid can answer for — a session on another
/// host, in another pid namespace, or whose agent outlived Gen holds it still.
/// One whose lock file is not there yet is being made, and is left.
fn sweep(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else { return };
    for e in entries.flatten() {
        if !e.file_name().to_str().is_some_and(|n| n.starts_with("run-")) {
            continue;
        }
        let Ok(lock) = std::fs::File::open(e.path().join(LOCK)) else { continue };
        if lock.try_lock().is_ok() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// What an agent's reader keeps between lines: text that arrives in pieces, and
/// usage that arrives per step.
#[derive(Default)]
pub struct Reader {
    text: String,
    usage: Usage,
}

impl Reader {
    /// A piece of the assistant's reply.
    pub fn delta(&mut self, s: &str) {
        self.text.push_str(s);
    }

    /// The reply so far, as one message, once it is whole.
    pub fn flush(&mut self) -> Option<Mapped> {
        let t = std::mem::take(&mut self.text);
        (!t.trim().is_empty()).then(|| Mapped::message("assistant", t.trim().to_string()))
    }

    /// Add one step's usage and give the running total: the orchestrator keeps the last.
    pub fn add(&mut self, step: Usage) -> Mapped {
        let sum = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        let u = &mut self.usage;
        u.input_tokens = sum(u.input_tokens, step.input_tokens);
        u.output_tokens = sum(u.output_tokens, step.output_tokens);
        u.cache_read_tokens = sum(u.cache_read_tokens, step.cache_read_tokens);
        u.cache_write_tokens = sum(u.cache_write_tokens, step.cache_write_tokens);
        u.num_turns = sum(u.num_turns, step.num_turns);
        Mapped::Usage(u.clone())
    }
}

/// A report that counts cache reads and writes apart from input, as the summary
/// reads every agent but Claude: cache inside input.
pub fn inside(input: Option<u64>, read: Option<u64>, written: Option<u64>) -> Option<u64> {
    input.map(|i| i + read.unwrap_or(0) + written.unwrap_or(0))
}

/// `w` as one word of a command line: quoted when it holds anything a shell would split on.
pub fn shell_word(w: &str) -> String {
    if !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@+%,".contains(c)) {
        w.to_string()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

/// An agent as a backend: its file builds the run, its reader reads the stream.
pub struct Session {
    agent: &'static Agent,
    reader: Mutex<Reader>,
}

impl Session {
    pub fn new(agent: &'static Agent) -> Session {
        Session { agent, reader: Mutex::new(Reader::default()) }
    }
}

impl Backend for Session {
    fn label(&self) -> &'static str {
        self.agent.name
    }

    fn version(&self, root: &Root) -> Option<String> {
        let p = program(root, self.agent)?;
        crate::exe::version(&p.display().to_string(), super::backend::VERSION_WAIT)
    }

    fn build(&self, spec: &Spec) -> Result<Launch> {
        let a = self.agent;
        let program = program(&spec.root, a).map(|p| p.display().to_string()).unwrap_or_else(|| a.programs[0].to_string());
        let mut run = Run::new(spec, &program);
        (a.build)(&mut run)?;
        run.cmd.args(&spec.passthrough);
        Ok(Launch { command: run.cmd, cleanup: run.cleanup })
    }

    fn parse(&self, line: &str) -> Vec<Mapped> {
        let mut reader = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        (self.agent.read)(&mut reader, line)
    }

    fn transcript_path(&self, _root: &Root, _route: &Route, _cwd: &Path, _backend_session_id: &str) -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
pub(crate) mod test {
    //! A run of an agent's file the way a session builds one, for each file's tests.

    use super::*;

    pub fn spec(mode: Mode, routing: Route) -> Spec {
        Spec {
            mode,
            task: Some("fix the test".into()),
            cwd: PathBuf::from("/tmp"),
            routing,
            approval: Approval::Auto,
            mcp: Some(McpAttach { program: "/opt/hanzo".into(), args: vec!["__mcp".into(), "--".into(), "hanzo-mcp".into()] }),
            remote: None,
            structured: false,
            preset_session: None,
            trust_project: false,
            resume: None,
            passthrough: vec![],
            refresh: vec!["hanzo".into(), "auth".into(), "token".into()],
            root: crate::root::scratch(),
        }
    }

    pub fn gateway() -> Route {
        Route::Via(Routing::Gateway {
            api: "https://api.hanzo.ai".into(),
            token: "tok".into(),
            model: "enso".into(),
            small_fast_model: "enso-flash".into(),
            context_window: 1_000_000,
        })
    }

    pub fn build(name: &str, spec: &Spec) -> Launch {
        Session::new(find(name).unwrap()).build(spec).unwrap()
    }

    pub fn argv(launch: &Launch) -> Vec<String> {
        launch.command.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    pub fn env(launch: &Launch, key: &str) -> Option<String> {
        launch.command.as_std().get_envs().find(|(k, _)| *k == key).and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    /// Every line of a captured stream through the agent's reader, in order.
    pub fn read(name: &str, stream: &str) -> Vec<Mapped> {
        let s = Session::new(find(name).unwrap());
        stream.lines().flat_map(|l| s.parse(l)).collect()
    }

    /// The assistant's messages among `out`.
    pub fn said(out: &[Mapped]) -> Vec<String> {
        out.iter()
            .filter_map(|m| match m {
                Mapped::Event { kind: super::super::event::Kind::Message, payload } if payload["role"] == "assistant" => {
                    payload["text"].as_str().map(String::from)
                }
                _ => None,
            })
            .collect()
    }

    /// The tools called among `out`.
    pub fn called(out: &[Mapped]) -> Vec<String> {
        out.iter()
            .filter_map(|m| match m {
                Mapped::Event { kind: super::super::event::Kind::ToolCall, payload } => payload["name"].as_str().map(String::from),
                _ => None,
            })
            .collect()
    }

    pub fn terminal(out: &[Mapped]) -> Option<bool> {
        out.iter().rev().find_map(|m| match m {
            Mapped::Terminal { ok, .. } => Some(*ok),
            _ => None,
        })
    }

    pub fn usage(out: &[Mapped]) -> Option<Usage> {
        out.iter().rev().find_map(|m| match m {
            Mapped::Usage(u) => Some(u.clone()),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test::*;
    use super::*;

    /// Two sessions of one agent never share a home: each is pointed at its own,
    /// whose config names that session's project and ledger, and the home is gone
    /// when its session is. A shared `~/.hanzo/<agent>` let one session's tools act
    /// on the other's repository.
    #[test]
    fn each_session_of_an_agent_has_a_home_of_its_own() {
        let s = test::spec(Mode::Headless, test::gateway());
        let (one, two) = (test::build("grok", &s), test::build("grok", &s));
        let (h1, h2) = (test::env(&one, "GROK_HOME").unwrap(), test::env(&two, "GROK_HOME").unwrap());
        assert_ne!(h1, h2, "one home for two sessions");
        assert!(Path::new(&h1).starts_with(s.root.hanzo().join("grok")), "{h1}");
        assert!(Path::new(&h2).join("config.toml").is_file());
        drop(one);
        assert!(!Path::new(&h1).exists(), "a session's home outlived it");
        assert!(Path::new(&h2).join("config.toml").is_file(), "ending one session ended the other's home");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&h2).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "a session's home is its user's alone, whatever the umask");
        }
    }

    /// A home is swept once no session holds its lock, whatever pid made it: one
    /// whose session died without cleaning up (SIGKILL) goes with the next run of
    /// that agent; one held elsewhere (another host, another pid namespace), one
    /// being made, and this session's own are kept.
    #[test]
    fn a_home_is_swept_once_its_lock_is_free() {
        let s = test::spec(Mode::Headless, test::gateway());
        let parent = s.root.hanzo().join("pi");
        std::fs::create_dir_all(&parent).unwrap();
        let home = |name: &str, lock: bool| {
            let d = parent.join(name);
            std::fs::create_dir_all(d.join("x")).unwrap();
            lock.then(|| std::fs::File::create(d.join(LOCK)).unwrap())
        };
        drop(home("run-dead", true));
        let held = home("run-held", true).unwrap();
        held.lock().unwrap();
        home("run-new", false);
        let live = test::build("pi", &s);
        let mine = test::env(&live, "PI_CODING_AGENT_DIR").unwrap();
        let other = test::build("pi", &s);
        assert!(!parent.join("run-dead").exists(), "a dead session's home was left behind");
        assert!(parent.join("run-held").is_dir(), "a home held elsewhere was swept");
        assert!(parent.join("run-new").is_dir(), "a home being made was swept");
        assert!(Path::new(&mine).is_dir(), "a live session's home was swept");
        drop((live, other, held));
        sweep(&parent);
        assert!(!parent.join("run-held").exists(), "a home was kept after its lock was let go");
        let _ = std::fs::remove_dir_all(parent.join("run-new"));
    }

    /// The agent holds its home's lock as well, so a home outlives a Gen that died
    /// while its agent runs on, and goes once the agent does.
    #[cfg(unix)]
    #[test]
    fn a_home_lives_as_long_as_its_agent() {
        let s = test::spec(Mode::Headless, test::gateway());
        let sh = crate::exe::find("sh").unwrap();
        let mut run = Run::new(&s, sh.to_str().unwrap());
        let home = run.home("hold").unwrap();
        run.args(["-c", "read x"]).cmd.stdin(std::process::Stdio::piped()).current_dir(&home);
        let mut agent = run.cmd.as_std_mut().spawn().unwrap();
        let crate::backend::Scratch { locks, dirs, .. } = std::mem::take(&mut run.cleanup);
        drop(locks);
        let kept: Vec<PathBuf> = dirs.into_iter().map(tempfile::TempDir::keep).collect();
        let parent = s.root.hanzo().join("hold");
        sweep(&parent);
        assert!(home.is_dir(), "a home was swept while its agent ran");
        drop(agent.stdin.take());
        agent.wait().unwrap();
        sweep(&parent);
        assert!(!home.exists(), "a home outlived its agent");
        assert_eq!(kept, vec![home]);
    }

    #[test]
    fn every_spelling_is_one_agent_and_no_two_rows_share_one() {
        let mut seen = std::collections::HashSet::new();
        for a in AGENTS {
            for s in std::iter::once(&a.name).chain(a.aliases) {
                assert!(seen.insert(*s), "{s} names two agents");
                assert_eq!(find(&s.to_ascii_uppercase()).unwrap().name, a.name);
            }
            assert!(!a.programs.is_empty(), "{} runs nothing", a.name);
        }
        // Our own are not rows: they have backends of their own.
        for own in ["dev", "claude", "codex", "interpreter"] {
            assert!(find(own).is_none(), "{own}");
        }
    }

    /// Everything a launch hands the agent: its argv, its environment and the
    /// files those name, so a check reads all three at once.
    fn handed(l: &Launch) -> String {
        let mut out = argv(l);
        out.extend(l.command.as_std().get_envs().filter_map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned())));
        let files: Vec<String> = out.iter().filter_map(|w| std::fs::read_to_string(w.trim_start_matches('@')).ok()).collect();
        out.extend(files);
        out.join("\n")
    }

    #[test]
    fn each_agent_is_handed_the_toolset_exactly_when_it_says_it_takes_one() {
        for a in AGENTS {
            for (routed, route) in [(false, Route::Inherit), (true, gateway())] {
                let l = build(a.name, &spec(Mode::Headless, route));
                let mut seen = handed(&l).contains("/opt/hanzo");
                // A home of Hanzo's own carries it in a file the launch names by directory.
                for dir in l.command.as_std().get_envs().filter_map(|(_, v)| v).chain(l.command.as_std().get_args()) {
                    if let Ok(entries) = std::fs::read_dir(dir) {
                        for e in entries.flatten() {
                            seen |= std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains("/opt/hanzo"))
                                || std::fs::read_dir(e.path()).is_ok_and(|d| {
                                    d.flatten().any(|f| std::fs::read_to_string(f.path()).is_ok_and(|s| s.contains("/opt/hanzo")))
                                });
                        }
                    }
                }
                assert_eq!(seen, a.takes_mcp(routed), "{} routed={routed}", a.name);
            }
        }
    }

    #[test]
    fn a_routed_agent_holds_the_key_in_its_environment_and_an_unrouted_one_never_sees_it() {
        for a in AGENTS {
            let l = build(a.name, &spec(Mode::Headless, gateway()));
            assert!(!argv(&l).iter().any(|w| w.contains("tok")), "{}: the key reached argv", a.name);
            let in_env = l.command.as_std().get_envs().any(|(_, v)| v.is_some_and(|v| v == "tok"));
            assert_eq!(in_env, a.routable, "{}", a.name);
            if !a.routable {
                assert!(!handed(&l).contains("api.hanzo.ai"), "{}: told of a gateway it cannot use", a.name);
            }
        }
    }

    #[test]
    fn a_reply_in_pieces_is_one_message_and_usage_per_step_is_a_running_total() {
        let mut r = Reader::default();
        r.delta("man");
        r.delta("go\n");
        assert_eq!(said(&[r.flush().unwrap()]), ["mango"]);
        assert!(r.flush().is_none());
        r.add(Usage { input_tokens: Some(10), output_tokens: Some(2), ..Default::default() });
        let Mapped::Usage(u) = r.add(Usage { input_tokens: Some(5), ..Default::default() }) else { panic!() };
        assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_tokens), (Some(15), Some(2), None));
    }

    #[test]
    fn a_command_line_word_is_quoted_only_when_a_shell_would_split_it() {
        assert_eq!(shell_word("/opt/hanzo"), "/opt/hanzo");
        assert_eq!(shell_word("/my dir/x"), "'/my dir/x'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
    }
}
