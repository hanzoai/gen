//! Isolated git checkout, deploy-key SSH confinement, and commit/push helpers.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// A run's deploy key and the host key it is checked against, on disk for the run:
/// the directory goes when the value does.
pub(super) struct Access {
    dir: PathBuf,
    pub(super) ssh: String,
}

impl Access {
    pub(super) fn make(work_dir: &Path, key: &str, known_hosts: &str) -> Result<Access> {
        let dir = PathBuf::from(format!("{}.ssh", work_dir.display()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).context("creating the key directory")?;
        let mut access = Access { dir, ssh: String::new() };
        let key_file = access.dir.join("key");
        let hosts_file = access.dir.join("known_hosts");
        write_private(&key_file, &format!("{}\n", key.trim_end()))?;
        write_private(&hosts_file, &format!("{}\n", known_hosts.trim_end()))?;
        access.ssh = format!(
            "ssh -i {} -o IdentitiesOnly=yes -o UserKnownHostsFile={} -o StrictHostKeyChecking=yes -o BatchMode=yes",
            quoted(&key_file),
            quoted(&hosts_file),
        );
        Ok(access)
    }
}

impl Drop for Access {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\\', "/").replace('\'', "'\\''"))
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("writing {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}


pub(super) async fn prepare_git_workspace(work_dir: &Path, clone_url: &str, base: &str, branch: &str, ssh: Option<&str>) -> Result<()> {
    let base = base.trim();
    let dest = work_dir.to_string_lossy();
    let cloned = if !base.is_empty() {
        clone(ssh, &["--branch", base], clone_url, &dest).await
    } else {
        Err(anyhow!("no explicit base branch"))
    };
    if cloned.is_err() {
        let _ = std::fs::remove_dir_all(work_dir);
        clone(ssh, &[], clone_url, &dest).await.context("git clone failed")?;
        if !base.is_empty() {
            run_git(Some(work_dir), &["checkout", base])
                .await
                .with_context(|| format!("git checkout {base} failed"))?;
        }
    }
    // The key serves the pushes after the clone too, the agent's own among them.
    if let Some(s) = ssh {
        run_git(Some(work_dir), &["config", "core.sshCommand", s]).await?;
    }

    if git_stdout(work_dir, &["config", "user.email"]).await.unwrap_or_default().is_empty() {
        let _ = run_git(Some(work_dir), &["config", "user.email", "dev@hanzo.ai"]).await;
    }
    if git_stdout(work_dir, &["config", "user.name"]).await.unwrap_or_default().is_empty() {
        let _ = run_git(Some(work_dir), &["config", "user.name", "Hanzo"]).await;
    }

    let branch = branch.trim();
    if !branch.is_empty() {
        run_git(Some(work_dir), &["checkout", "-B", branch])
            .await
            .with_context(|| format!("git checkout -B {branch} failed"))?;
    }
    Ok(())
}

/// `git clone`, through the run's own key when it has one.
async fn clone(ssh: Option<&str>, options: &[&str], url: &str, dest: &str) -> Result<String> {
    let through = ssh.map(|s| format!("core.sshCommand={s}"));
    let mut args: Vec<&str> = Vec::new();
    if let Some(t) = &through {
        args.extend(["-c", t]);
    }
    args.push("clone");
    args.extend(options);
    args.extend([url, dest]);
    run_git(None, &args).await
}

pub(super) async fn finalize_git_changes(work_dir: &Path, base_sha: &str, branch: &str) -> Result<Option<(String, String)>> {
    let status = git_stdout(work_dir, &["status", "--porcelain"]).await?;
    if !status.is_empty() {
        run_git(Some(work_dir), &["add", "-A"]).await?;
        run_git(Some(work_dir), &["commit", "-m", "chore: apply changes from Hanzo run"]).await?;
    }

    let head_sha = git_stdout(work_dir, &["rev-parse", "HEAD"]).await?;
    if head_sha.is_empty() || head_sha == base_sha {
        return Ok(None);
    }

    let refspec = format!("HEAD:refs/heads/{}", branch.trim());
    run_git(Some(work_dir), &["push", "-u", "origin", &refspec])
        .await
        .with_context(|| format!("git push origin {refspec} failed"))?;

    let stat = if !base_sha.is_empty() {
        git_stdout(work_dir, &["diff", "--stat", &format!("{base_sha}..HEAD")])
            .await
            .unwrap_or_default()
    } else {
        String::new()
    };
    Ok(Some((head_sha, stat)))
}

pub(super) async fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String> {
    let git = crate::exe::find("git").context("git is not installed")?;
    let mut cmd = Command::from(crate::exe::command(git));
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().await.context("spawning git")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = redact_git_detail(if !stderr.is_empty() { &stderr } else { &stdout });
        bail!("git {} exited {}: {detail}", args.first().copied().unwrap_or(""), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Scrub URL userinfo (`https://user:secret@host` / `https://token@host`), auth
/// headers (`Bearer ...`, `Basic ...`), and known token prefixes from git output
/// before reporting an error chain back to the session transcript.
pub(super) fn redact_git_detail(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(pos) = rest.find("://") {
        out.push_str(&rest[..pos + 3]);
        rest = &rest[pos + 3..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '/' | '\'' | '"' | ')' | '>'))
            .unwrap_or(rest.len());
        let authority = &rest[..end];
        if let Some(at) = authority.rfind('@') {
            out.push_str("***@");
            out.push_str(&authority[at + 1..]);
        } else {
            out.push_str(authority);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);

    let prefixes = [
        "ghp_",
        "ghs_",
        "gho_",
        "ghu_",
        "github_pat_",
        "glpat-",
        "sk-",
        "hk-",
    ];
    let mut tokens_scrubbed = String::with_capacity(out.len());
    let mut prev_auth_scheme = false;
    for (idx, chunk) in out.split_inclusive(|c: char| c.is_whitespace()).enumerate() {
        let trimmed_ws = chunk.trim_end_matches(|c: char| c.is_whitespace());
        let trailing = &chunk[trimmed_ws.len()..];
        let core = trimmed_ws.trim_matches(|c: char| matches!(c, '\'' | '"' | ',' | ';' | ':' | '(' | ')' | '[' | ']'));
        let lead_len = trimmed_ws.find(core).unwrap_or(0);
        let lead = &trimmed_ws[..lead_len];
        let tail = &trimmed_ws[lead_len + core.len()..];

        let scrub = prev_auth_scheme
            || prefixes.iter().any(|p| core.starts_with(p) && core.len() > p.len() + 3);
        let _ = idx;
        prev_auth_scheme = core.eq_ignore_ascii_case("bearer") || core.eq_ignore_ascii_case("basic");
        if scrub {
            tokens_scrubbed.push_str(lead);
            tokens_scrubbed.push_str("***");
            tokens_scrubbed.push_str(tail);
            tokens_scrubbed.push_str(trailing);
        } else {
            tokens_scrubbed.push_str(chunk);
        }
    }
    tokens_scrubbed
}

pub(super) async fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String> {
    run_git(Some(cwd), args).await
}

