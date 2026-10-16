//! Where Gen keeps what it writes on this machine.
//!
//! Every path under the user's home that Gen reads or writes — settings, each
//! agent's home, the kit, ledgers, resume and run-target records, the machine id,
//! Claude's config home — is derived from ONE value, decided by the program that
//! launches a session and handed down from there: the signed-in user's own
//! ([`Root::user`], called only by a binary's `main`), or one directory a test or
//! a sandbox names ([`Root::at`]). Nothing below that reads the environment for a
//! home, which is what keeps a test suite out of the developer's
//! (`root_is_the_one_resolver` holds it, and Gen links no crate that guesses one).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    home: PathBuf,
    data: PathBuf,
}

impl Root {
    /// This user's, from the environment and nothing else: `$HOME` (`%USERPROFILE%`
    /// on Windows), and the platform's per-user data directory under it
    /// (`~/.local/share` or `$XDG_DATA_HOME` on Linux, `~/Library/Application
    /// Support` on macOS, `%LOCALAPPDATA%` on Windows). An unset, empty or relative
    /// home is no home — never the account database's, which is how a process
    /// told to have no home came to write into one anyway.
    pub fn user() -> Option<Root> {
        Root::from(&|name| std::env::var_os(name))
    }

    /// This user's, or why there is none: the message a launcher stops on.
    pub fn need() -> anyhow::Result<Root> {
        Root::user().ok_or_else(|| {
            anyhow::anyhow!("no home directory: Hanzo keeps a session's state under $HOME — set HOME and run again")
        })
    }

    /// [`Root::user`] over `var`, so the rule is testable without the process's own environment.
    fn from(var: &dyn Fn(&str) -> Option<OsString>) -> Option<Root> {
        let absolute = |name: &str| var(name).map(PathBuf::from).filter(|p| p.is_absolute());
        let home = absolute(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
        let data = if cfg!(windows) {
            absolute("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData").join("Local"))
        } else if cfg!(target_os = "macos") {
            home.join("Library").join("Application Support")
        } else {
            absolute("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local").join("share"))
        };
        Some(Root { home, data: data.join("hanzo") })
    }

    /// Everything under `dir`, laid out as a home: `dir/.hanzo`, `dir/.local/share/hanzo`.
    pub fn at(dir: impl Into<PathBuf>) -> Root {
        let home = dir.into();
        let data = home.join(".local").join("share").join("hanzo");
        Root { home, data }
    }

    /// `~`: where agents keep homes of their own (`~/.claude`, `~/.agents`).
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// `~/.hanzo`: settings, ledgers, the kit, and the homes Hanzo gives agents.
    pub fn hanzo(&self) -> PathBuf {
        self.home.join(".hanzo")
    }

    /// `~/.local/share/hanzo`: resume records, run-targets, the machine id.
    pub fn data(&self) -> &Path {
        &self.data
    }
}

/// A root of its own for one test process, removed when the process ends.
#[cfg(test)]
pub(crate) fn scratch() -> Root {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    Root::at(DIR.get_or_init(|| tempfile::tempdir().expect("a temp dir")).path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_lays_its_directories_out_as_a_home() {
        let r = Root::at("/h");
        assert_eq!(r.home(), Path::new("/h"));
        assert_eq!(r.hanzo(), Path::new("/h/.hanzo"));
        assert_eq!(r.data(), Path::new("/h/.local/share/hanzo"));
    }

    /// No home is no root: unset, empty and relative all refuse, and nothing
    /// falls back to the account database's idea of a home.
    #[cfg(unix)]
    #[test]
    fn an_unset_empty_or_relative_home_is_no_home() {
        fn env(home: Option<&'static str>) -> impl Fn(&str) -> Option<OsString> {
            move |name| (name == "HOME").then_some(home).flatten().map(OsString::from)
        }
        assert_eq!(Root::from(&env(None)), None);
        assert_eq!(Root::from(&env(Some(""))), None);
        assert_eq!(Root::from(&env(Some("rel/home"))), None);
        let r = Root::from(&env(Some("/h"))).expect("an absolute home is one");
        assert_eq!(r.hanzo(), Path::new("/h/.hanzo"));
        if !cfg!(target_os = "macos") {
            assert_eq!(r.data(), Path::new("/h/.local/share/hanzo"));
            let xdg = |name: &str| match name {
                "HOME" => Some(OsString::from("/h")),
                "XDG_DATA_HOME" => Some(OsString::from("/d")),
                _ => None,
            };
            assert_eq!(Root::from(&xdg).unwrap().data(), Path::new("/d/hanzo"));
        }
    }

    /// THE gate for keeping tests out of the developer's home, and programs out of
    /// a repository's reach. Gen links no crate that resolves a home, so the only
    /// way in is the environment, and only this file reads a home from it; only a
    /// binary's `main` asks for the user's root; and only `exe.rs` searches PATH.
    #[test]
    fn root_is_the_one_resolver() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let rules: [(&[&str], &[&str]); 3] = [
            (&["src/root.rs"], &["\"HOME\"", "\"USERPROFILE\"", "\"LOCALAPPDATA\"", "\"XDG_", "home_dir", "dirs::"]),
            (&["src/main.rs", "src/root.rs"], &["Root::user(", "Root::need("]),
            (&["src/exe.rs"], &["which::", "\"PATH\""]),
        ];
        let mut found = Vec::new();
        let mut left = vec![src];
        while let Some(d) = left.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    left.push(p);
                    continue;
                }
                // This file holds the gate and the one home resolver.
                if p.extension().is_none_or(|x| x != "rs") || p.ends_with("src/root.rs") {
                    continue;
                }
                let body = std::fs::read_to_string(&p).unwrap();
                for (n, line) in body.lines().enumerate() {
                    if line.trim_start().starts_with("//") {
                        continue;
                    }
                    for (owners, words) in &rules {
                        if owners.iter().any(|o| p.ends_with(o)) {
                            continue;
                        }
                        for w in words.iter().filter(|w| line.contains(**w)) {
                            found.push(format!("{}:{} uses {w}, which only {owners:?} may", p.display(), n + 1));
                        }
                    }
                }
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }
}
