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
    /// The npm package `hanzo upgrade <name>` installs, when it ships there.
    pub package: Option<&'static str>,
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
pub fn prefix() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".hanzo"))
}

/// The binary that runs `a`: the first of its programs on PATH, else in `~/.hanzo/bin`.
pub fn program(a: &Agent) -> Option<PathBuf> {
    a.programs.iter().find_map(|p| which::which(p).ok()).or_else(|| {
        let bin = prefix()?.join("bin");
        a.programs.iter().map(|p| bin.join(p)).find(|p| p.is_file())
    })
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
    cleanup: Vec<tempfile::TempPath>,
}

impl<'a> Run<'a> {
    fn new(spec: &'a Spec, program: &str) -> Run<'a> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.current_dir(&spec.cwd);
        Run { spec, cmd, cleanup: Vec::new() }
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
        self.cleanup.push(path);
        Ok(p)
    }

    /// A directory of Hanzo's own for an agent's state, `~/.hanzo/<name>`.
    pub fn home(&self, name: &str) -> Result<PathBuf> {
        let d = prefix().context("no home directory")?.join(name);
        super::kit::private_dir(&d)?;
        Ok(d)
    }

    /// Write `doc` to `path`, readable by this user only.
    pub fn write(&self, path: &Path, doc: &Value) -> Result<()> {
        crate::private::write(path, doc.to_string().as_bytes()).with_context(|| format!("writing {}", path.display()))
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

    fn version(&self) -> Option<String> {
        let p = program(self.agent)?;
        super::backend::backend_version(&p.display().to_string())
    }

    fn build(&self, spec: &Spec) -> Result<Launch> {
        let a = self.agent;
        let program = program(a).map(|p| p.display().to_string()).unwrap_or_else(|| a.programs[0].to_string());
        let mut run = Run::new(spec, &program);
        (a.build)(&mut run)?;
        run.cmd.args(&spec.passthrough);
        Ok(Launch { command: run.cmd, cleanup: run.cleanup })
    }

    fn parse(&self, line: &str) -> Vec<Mapped> {
        let mut reader = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        (self.agent.read)(&mut reader, line)
    }

    fn transcript_path(&self, _route: &Route, _cwd: &Path, _backend_session_id: &str) -> Option<PathBuf> {
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
