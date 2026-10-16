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

/// Hand `cmd`'s child the PATH Gen searches, its empty and relative entries
/// dropped, so the child finds its own helpers the same way (a probe runs with
/// nothing else in its environment). Without one, the child gets none.
pub fn confine(cmd: &mut std::process::Command) {
    match std::env::var_os("PATH").and_then(|p| search(&p)) {
        Some(p) => cmd.env("PATH", p),
        None => cmd.env_remove("PATH"),
    };
}

fn search(path: &OsStr) -> Option<OsString> {
    let dirs: Vec<PathBuf> = std::env::split_paths(path).filter(|d| d.is_absolute()).collect();
    if dirs.is_empty() {
        return None;
    }
    std::env::join_paths(dirs).ok()
}

/// The first line `program --version` prints, given `patience` to print it. A
/// program that is not found, fails, or hangs has no version.
pub fn version(program: &str, patience: Duration) -> Option<String> {
    let bin = find(program)?;
    let mut child = std::process::Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + patience;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    out.lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in program in a directory of its own.
    #[cfg(unix)]
    fn program(name: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Another test's fork may hold the file open for writing until it execs.
        loop {
            match std::process::Command::new(&p).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
                Ok(mut c) => {
                    let _ = c.kill();
                    let _ = c.wait();
                    break;
                }
                Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ExecutableFileBusy, "{e}"),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
    }
}
