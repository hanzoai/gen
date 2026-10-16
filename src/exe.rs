//! THE way Gen finds a program to run: by its full path, from the absolute
//! entries of PATH only.
//!
//! An empty or relative PATH entry (`PATH=/usr/bin:` ends in one) is wherever the
//! session happens to start, and a session starts in a repository it has not
//! read yet — one that can carry a `./grok`, a `./hanzo-mcp` or a `./git`. Such an
//! entry is never searched, and nothing Gen starts is named by a bare word: an
//! agent, the toolset, a version probe, a machine probe. Every caller asks here,
//! and `root_is_the_one_resolver` holds that nothing else reads PATH.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// `name` by its full path: itself when it is one, else the first match in an
/// absolute PATH entry. `None` when there is none to run.
pub fn find(name: &str) -> Option<PathBuf> {
    find_in(name, &std::env::var_os("PATH")?)
}

/// [`find`] over a given PATH.
pub fn find_in(name: &str, path: &OsStr) -> Option<PathBuf> {
    let named = Path::new(name);
    if named.is_absolute() {
        return named.is_file().then(|| named.to_path_buf());
    }
    if named.components().count() != 1 {
        return None; // `bin/x` is relative to wherever this runs
    }
    let joined = search(path)?;
    which::which_in(name, Some(joined), Path::new("/")).ok().filter(|p| p.is_absolute())
}

/// THE way Gen starts a program: `program` (a full path, from [`find`]) with the
/// PATH Gen searches, its empty and relative entries dropped. The child's own
/// lookups — an interpreter named by a `#!/usr/bin/env node` line, the tools an
/// agent runs — then search the same absolute entries, so a repository's `./node`
/// is never one of them. On Windows, where cmd.exe looks in the working directory
/// before PATH (an npm `.cmd` shim runs `"node"` through it), that look is turned
/// off for the child and everything it starts. Nothing else makes a `Command`.
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    confine(&mut cmd);
    cmd
}

/// [`command`] with nothing else in its environment: a probe hands a program no
/// value of the session's.
pub fn bare(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.env_clear();
    confine(&mut cmd);
    cmd
}

fn confine(cmd: &mut std::process::Command) {
    match std::env::var_os("PATH").and_then(|p| search(&p)) {
        Some(p) => cmd.env("PATH", p),
        None => cmd.env_remove("PATH"),
    };
    #[cfg(windows)]
    cmd.env("NoDefaultCurrentDirectoryInExePath", "1");
}

fn search(path: &OsStr) -> Option<OsString> {
    let dirs: Vec<PathBuf> = std::env::split_paths(path).filter(|d| d.is_absolute()).collect();
    if dirs.is_empty() {
        return None;
    }
    std::env::join_paths(dirs).ok()
}

/// The first line `program --version` prints, given `patience` to print it and
/// exit. A program that is not found, fails, or takes longer has no version. It
/// runs in its own directory, never the session's: a version is the program's,
/// and a repository has no say in it. Only the first line is read, so a child it
/// leaves holding the output open costs nothing.
pub fn version(program: &str, patience: Duration) -> Option<String> {
    let bin = find(program)?;
    let mut child = command(&bin)
        .arg("--version")
        .current_dir(bin.parent()?)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + patience;
    let out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let mut first = std::io::BufReader::new(std::io::Read::take(out, 4096));
        let _ = tx.send(std::io::BufRead::read_line(&mut first, &mut line).map(|_| line));
    });
    let read = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let status = loop {
        match (&read, child.try_wait()) {
            (Ok(Ok(_)), Ok(Some(status))) => break status,
            (Ok(Ok(_)), Ok(None)) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let line = read.ok()?.ok()?.trim().to_string();
    (status.success() && !line.is_empty()).then_some(line)
}

/// A stand-in program `name` running `body` in `dir`, once the kernel will run it.
#[cfg(all(test, unix))]
pub(crate) fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A thread of another test that forks while this file is open for writing
    // carries that descriptor until it execs, and the kernel refuses to run a
    // file held open for writing. Hand it over once that has passed.
    loop {
        match command(&p).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(mut c) => {
                let _ = c.kill();
                let _ = c.wait();
                return p;
            }
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ExecutableFileBusy, "{e}"),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in program in a directory of its own.
    #[cfg(unix)]
    fn program(name: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = stub(dir.path(), name, body);
        (dir, p)
    }

    /// Only an absolute PATH entry supplies a program: an empty or relative one is
    /// wherever the session starts, which a repository controls.
    #[cfg(unix)]
    #[test]
    fn a_program_is_found_by_its_full_path_in_absolute_entries_only() {
        let (dir, p) = program("grok", "exit 0\n");
        let rel = OsString::from("::.:bin:rel/dir");
        assert_eq!(find_in("grok", &rel), None, "relative entries are never searched");
        let mixed = std::env::join_paths([PathBuf::from(""), PathBuf::from("."), dir.path().to_path_buf()]).unwrap();
        assert_eq!(find_in("grok", &mixed), Some(p.clone()));
        assert_eq!(find_in(p.to_str().unwrap(), &rel), Some(p.clone()), "a full path is itself");
        assert_eq!(find_in("./grok", &mixed), None, "a relative path names nothing");
        assert_eq!(find_in("bin/grok", &mixed), None);
    }

    /// A version probe is a program run at startup, so it is found the same way and
    /// given a moment, not forever.
    #[cfg(unix)]
    #[test]
    fn a_version_is_read_in_time_or_not_at_all() {
        let (_d1, quick) = program("quick", "echo 'quick 1.2.3'\n");
        assert_eq!(version(quick.to_str().unwrap(), Duration::from_secs(5)).as_deref(), Some("quick 1.2.3"));
        let (_d2, hang) = program("hang", "exec sleep 3600\n");
        let started = Instant::now();
        assert_eq!(version(hang.to_str().unwrap(), Duration::from_millis(200)), None);
        assert!(started.elapsed() < Duration::from_secs(5), "it gave up when it said it would");
        assert_eq!(version("./quick", Duration::from_secs(1)), None);
        // A child left holding the output open is no reason to wait for it.
        let (_d3, linger) = program("linger", "sleep 5 &\necho 'linger 2.0'\n");
        let started = Instant::now();
        assert_eq!(version(linger.to_str().unwrap(), Duration::from_secs(3)).as_deref(), Some("linger 2.0"));
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        // Nor is endless output: only the first line is ever read.
        let (_d4, flood) = program("flood", "exec yes\n");
        assert_eq!(version(flood.to_str().unwrap(), Duration::from_millis(500)), None);
    }

    /// A version is the program's own: it is asked in the program's directory,
    /// never the session's, which a repository controls.
    #[cfg(unix)]
    #[test]
    fn a_version_is_asked_where_the_program_lives() {
        let (dir, pwd) = program("pwd", "pwd -P\n");
        let here = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(version(pwd.to_str().unwrap(), Duration::from_secs(5)), Some(here.display().to_string()));
    }

    /// Every child is handed the PATH Gen searched, so an interpreter a `#!` line
    /// names by a bare word is found where Gen would find it, never in `.`.
    #[test]
    fn a_child_is_handed_only_the_absolute_entries_of_path() {
        let cmd = command("/bin/true");
        let handed = cmd.get_envs().find(|(k, _)| *k == "PATH").and_then(|(_, v)| v).map(OsStr::to_os_string);
        let expected = std::env::var_os("PATH").and_then(|p| search(&p));
        assert_eq!(handed, expected);
        for d in std::env::split_paths(handed.as_deref().unwrap_or_default()) {
            assert!(d.is_absolute(), "{}", d.display());
        }
        let bare = bare("/bin/true");
        let names: Vec<_> = bare.get_envs().map(|(k, _)| k.to_os_string()).collect();
        assert!(names.iter().all(|k| k == "PATH" || k == "NoDefaultCurrentDirectoryInExePath"), "{names:?}");
    }
}
