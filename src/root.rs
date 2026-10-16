//! Where Gen keeps what it writes on this machine.
//!
//! Every path under the user's home that Gen reads or writes — settings, each
//! agent's home, the kit, ledgers, resume and run-target records, the machine id,
//! Claude's config home — is derived from ONE value, decided by the program that
//! launches a session and handed down from there: the signed-in user's own
//! ([`Root::user`]), or one directory a test or a sandbox names ([`Root::at`]).
//! Nothing below that resolves `$HOME` for itself, which is what keeps a test
//! suite out of the developer's home (`root_is_the_one_resolver` holds it).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    home: PathBuf,
    data: PathBuf,
}

impl Root {
    /// This user's: `$HOME`, and the platform's per-user data directory
    /// (`~/.local/share` on Linux). `None` when the process has no home, in which
    /// case nothing is written anywhere.
    pub fn user() -> Option<Root> {
        let home = dirs::home_dir().filter(|h| h.is_absolute())?;
        let data = dirs::data_local_dir()
            .filter(|d| d.is_absolute())
            .unwrap_or_else(|| home.join(".local").join("share"));
        Some(Root { home, data: data.join("hanzo") })
    }

    /// This user's, or why there is none: the message a launcher stops on.
    pub fn need() -> anyhow::Result<Root> {
        Root::user().ok_or_else(|| {
            anyhow::anyhow!("no home directory: Hanzo keeps a session's state under $HOME — set HOME and run again")
        })
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

    /// THE gate for keeping tests out of the developer's home: only this file
    /// resolves a home or a data directory. A second resolver is how a test came
    /// to rewrite the agent configs of whoever ran the suite.
    #[test]
    fn root_is_the_one_resolver() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = Vec::new();
        let mut dirs_left = vec![src];
        while let Some(d) = dirs_left.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs_left.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("src/root.rs") {
                    let body = std::fs::read_to_string(&p).unwrap();
                    for (n, line) in body.lines().enumerate() {
                        if ["dirs::home_dir", "dirs::data_local_dir", "dirs::data_dir", "dirs::config_dir", "env::home_dir", "var(\"HOME\")", "var_os(\"HOME\")"]
                            .iter()
                            .any(|f| line.contains(f))
                        {
                            found.push(format!("{}:{}", p.display(), n + 1));
                        }
                    }
                }
            }
        }
        assert!(found.is_empty(), "a home is resolved outside root.rs: {found:?}");
    }
}
