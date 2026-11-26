//! OS-enforced read-only containment for ONE routed run.
//!
//! A prompt line is not containment: the agent is a program with shell tools and
//! can ignore prose. Here the workspace is mounted read-only for the child process
//! by the OS — macOS Seatbelt (`sandbox-exec`) or Linux bubblewrap (`bwrap`) — and
//! a write probe must FAIL before the agent is started. No tool, no containment:
//! the run is refused with `read_only_unsupported`, never run writable.
//!
//! This contains the harness for one run on a machine the owner controls. It is
//! NOT a security boundary against the machine's owner, and it makes no promise
//! about writes to paths outside the workspace that the harness is otherwise
//! allowed to use (its own config home, scratch, the network).

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::backend::Launch;
use crate::root::Root;

/// The refusal code cloud uses when a target cannot honour a read-only run.
pub const READ_ONLY_UNSUPPORTED: &str = "read_only_unsupported";

/// What a launch needs to become a read-only child. Kept on [`Launch`] so every
/// turn of a supervised run (steers, continuations) is guarded, not just the first.
#[derive(Debug, Clone)]
pub struct Recipe {
    pub workspace: PathBuf,
    pub root: Root,
}

/// Whether this machine has an OS sandbox to try. A true is only a claim that a
/// guard can be BUILT; the guard itself still probes a real denied write before
/// it lets any agent start.
pub fn supported() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::exe::find("sandbox-exec").is_some()
    }
    #[cfg(target_os = "linux")]
    {
        crate::exe::find("bwrap").is_some()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

impl Recipe {
    /// Wrap `launch`'s command in the OS sandbox and keep its profile alive until
    /// the child is reaped. Fail closed: any error here prevents the run.
    pub fn guard(&self, launch: &mut Launch) -> Result<()> {
        let workspace = std::fs::canonicalize(&self.workspace)
            .with_context(|| format!("read-only workspace {} is not on disk", self.workspace.display()))?;
        if !workspace.is_dir() {
            bail!("read-only workspace {} is not a directory", workspace.display());
        }
        std::fs::create_dir_all(self.root.data())
            .context("creating the scratch root for a read-only run")?;
        let scratch = tempfile::Builder::new()
            .prefix("readonly-")
            .tempdir_in(self.root.data())
            .context("making scratch for a read-only run")?;

        #[cfg(target_os = "macos")]
        self.guard_macos(launch, &workspace, scratch.path())?;
        #[cfg(target_os = "linux")]
        self.guard_linux(launch, &workspace, scratch.path())?;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        bail!("{}: this platform has no read-only workspace sandbox", READ_ONLY_UNSUPPORTED);

        launch.cleanup.dirs.push(scratch);
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn guard_macos(&self, launch: &mut Launch, workspace: &Path, scratch: &Path) -> Result<()> {
        let tool = crate::exe::find("sandbox-exec")
            .ok_or_else(|| anyhow!("{}: sandbox-exec not found", READ_ONLY_UNSUPPORTED))?;
        let profile = format!(
            "(version 1)\n(allow default)\n(deny file-write* (subpath \"{}\"))\n",
            sbpl_quote(&workspace.to_string_lossy())
        );
        let holder = tempfile::NamedTempFile::new_in(scratch).context("making a sandbox profile")?;
        let path = holder.path().to_path_buf();
        crate::private::write(&path, profile.as_bytes()).context("writing a sandbox profile")?;
        let temp = holder.into_temp_path();
        // Prove the profile DENIES a write before trusting it. Without this a
        // typo in an SBPL path would look like a read-only run that wrote freely.
        denied(
            &tool,
            &["-f".into(), path.clone().into_os_string(), "--".into(), shell().into(), "-c".into(),
              format!("printf x > {}", shell_quote(&workspace.join(".hanzo-readonly-probe"))).into()],
            &workspace.join(".hanzo-readonly-probe"),
        )?;
        replace_command(launch, &tool, &[
            OsString::from("-f"), path.into_os_string(), OsString::from("--"),
        ])?;
        launch.cleanup.files.push(temp);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn guard_linux(&self, launch: &mut Launch, workspace: &Path, scratch: &Path) -> Result<()> {
        let tool = crate::exe::find("bwrap")
            .ok_or_else(|| anyhow!("{}: bwrap not found", READ_ONLY_UNSUPPORTED))?;
        // Everything is readable; only the workspace is read-only. The scratch
        // and the session home stay writable so the agent can keep its own state.
        let mut prefix: Vec<OsString> = vec![
            "--ro-bind".into(), "/".into(), "--bind".into(), scratch.into(),
            "--bind".into(), self.root.home().into(),
        ];
        if Path::new("/tmp").is_dir() {
            prefix.extend(["--bind".into(), "/tmp".into()]);
        }
        // Last bind wins: a workspace under the home must still be read-only.
        prefix.extend(["--ro-bind".into(), workspace.into(), "--die-with-parent".into(), "--".into()]);
        denied(
            &tool,
            &prefix.iter().cloned().chain([
                shell().into(), "-c".into(),
                format!("printf x > {}", shell_quote(&workspace.join(".hanzo-readonly-probe"))).into(),
            ]).collect::<Vec<_>>(),
            &workspace.join(".hanzo-readonly-probe"),
        )?;
        replace_command(launch, &tool, &prefix)?;
        Ok(())
    }
}

/// Run a command that must NOT be able to write `probe`, and confirm it did not.
fn denied(tool: &Path, args: &[OsString], probe: &Path) -> Result<()> {
    let _ = std::fs::remove_file(probe);
    let status = crate::exe::command(tool)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("probing the read-only sandbox {}", tool.display()))?;
    let wrote = probe.exists();
    let _ = std::fs::remove_file(probe);
    if status.success() || wrote {
        bail!("{}: the read-only sandbox allowed a write to {}", READ_ONLY_UNSUPPORTED, probe.display());
    }
    Ok(())
}

/// Put `prefix` in front of the launch's program and args, preserving cwd + env.
fn replace_command(launch: &mut Launch, tool: &Path, prefix: &[OsString]) -> Result<()> {
    let (program, args, cwd, env) = {
        let s = launch.command.as_std();
        (s.get_program().to_os_string(), s.get_args().map(|a| a.to_os_string()).collect::<Vec<_>>(),
         s.get_current_dir().map(Path::to_path_buf),
         s.get_envs().map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string()))).collect::<Vec<_>>())
    };
    let mut next: tokio::process::Command = crate::exe::command(tool).into();
    {
        let s = next.as_std_mut();
        s.args(prefix);
        s.arg(program);
        s.args(args);
        if let Some(dir) = cwd {
            s.current_dir(dir);
        }
        for (k, v) in env {
            match v {
                Some(v) => { s.env(k, v); }
                None => { s.env_remove(k); }
            }
        }
    }
    launch.command = next;
    Ok(())
}

#[cfg(unix)]
fn shell() -> PathBuf {
    crate::exe::find("sh").unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

#[cfg(not(unix))]
fn shell() -> PathBuf {
    PathBuf::from("cmd.exe")
}

fn shell_quote(s: &Path) -> String {
    let s = s.to_string_lossy().replace('\'', "'\\''");
    format!("'{s}'")
}

#[cfg(target_os = "macos")]
fn sbpl_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_only_run_requires_a_tool_on_this_platform() {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            // The probe is a claim about the platform, never about a particular
            // workspace: a machine without the tool must not advertise it.
            let _ = supported();
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        assert!(!supported());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_seatbelt_profile_denies_writes_and_allows_reads_in_the_workspace() {
        if !supported() { return; }
        let dir = tempfile::tempdir().expect("a workspace");
        let real = std::fs::canonicalize(dir.path()).expect("a real workspace");
        let file = real.join("tracked.txt");
        std::fs::write(&file, "unchanged").unwrap();
        let scratch = tempfile::tempdir().expect("scratch");
        let holder = tempfile::NamedTempFile::new_in(scratch.path()).unwrap();
        let profile = holder.path().to_path_buf();
        crate::private::write(&profile,
            format!("(version 1)\n(allow default)\n(deny file-write* (subpath \"{}\"))\n",
                    sbpl_quote(&real.to_string_lossy())).as_bytes()).unwrap();
        let tool = crate::exe::find("sandbox-exec").unwrap();
        let denied = crate::exe::command(&tool).args([
            OsString::from("-f"), profile.clone().into_os_string(), OsString::from("--"),
            shell().into(), OsString::from("-c"),
            format!("printf x > {}", shell_quote(&file)).into(),
        ]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
        assert!(!denied.success(), "the sandbox let a child write into the read-only workspace");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "unchanged");
        let read = crate::exe::command(&tool).args([
            OsString::from("-f"), profile.into_os_string(), OsString::from("--"),
            shell().into(), OsString::from("-c"),
            format!("cat {}", shell_quote(&file)).into(),
        ]).output().unwrap();
        assert!(read.status.success() && read.stdout == b"unchanged", "the sandbox denied a read: {read:?}");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_recipe_wraps_every_launch_and_keeps_its_program_and_args() {
        if !supported() { return; }
        let root = crate::root::scratch();
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let mut cmd = crate::exe::command(shell());
        cmd.arg("-c").arg("true").current_dir(&real).env("HANZO_READONLY_TEST", "1");
        let mut launch = Launch { command: cmd.into(), cleanup: Default::default(), readonly: None };
        Recipe { workspace: dir.path().to_path_buf(), root }.guard(&mut launch).unwrap();
        let s = launch.command.as_std();
        assert_ne!(s.get_program(), shell().as_os_str(), "the child was not wrapped in the sandbox");
        let args = s.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert!(args.iter().any(|a| a == "true"), "the original argv was lost: {args:?}");
        assert_eq!(s.get_current_dir(), Some(real.as_path()));
        assert_eq!(s.get_envs().find(|(k, _)| *k == "HANZO_READONLY_TEST").unwrap().1.unwrap(), "1");
    }

    #[test]
    fn shell_quotes_a_path_with_spaces_and_quotes() {
        let p = Path::new("/tmp/a b's");
        assert_eq!(shell_quote(p), "'/tmp/a b'\\''s'");
    }
}
