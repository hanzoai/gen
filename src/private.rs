//! Writing a file that only its owner can read — ONE way, everywhere.
//!
//! Two properties, always together, because every caller wants both and neither
//! is safe to forget:
//!
//! - **Owner-only.** The mode is set on the temp file BEFORE it is published, so
//!   the bytes are never momentarily world-readable. `fs::write` + a later
//!   `set_permissions` leaves exactly that window.
//! - **Atomic.** A reader sees the old file or the new one, never a half-written
//!   one. `fs::write` truncates in place: a crash mid-write leaves a torn file,
//!   which for a parsed file (the config) breaks every command, not just the one
//!   that crashed.
//!
//! The temp lives in the SAME directory, so `rename` stays within one filesystem
//! and is therefore atomic, and it is named UNIQUELY PER WRITE and created with
//! `O_EXCL`. Both properties are load-bearing:
//!
//! - **Unique.** Concurrent writers to ONE path are the normal case here, not an
//!   exotic one: the per-machine records (`machine-id`, the run-target) are a
//!   single file per install that every concurrent `hanzo` writes. A shared temp
//!   name means one writer's `rename` publishes another's half-written bytes —
//!   a value NOBODY wrote. This primitive must be safe on its own terms; it
//!   cannot assume a lock, because two of its three callers do not hold one.
//! - **`O_EXCL`.** We only ever write a temp we ourselves created, so a stale
//!   temp cannot be inherited and a planted symlink cannot be followed.
//!
//! The cost is litter on `SIGKILL` (the error path cleans up otherwise). That is
//! the right trade: a leftover temp is inert, a torn file is not.

use std::io;
use std::path::{Path, PathBuf};

/// The temp `write` publishes from — unique per write, a sibling of the target.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{:016x}.tmp", rand::random::<u64>()));
    path.with_file_name(name)
}

/// Write `bytes` to `path`, atomically, readable only by the owner.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_path(path);
    let out = match publish(&tmp, path, bytes) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Never leave a partial temp lying next to a good file.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    };
    sweep(path);
    out
}

/// Reap temps abandoned by a `SIGKILL`ed run.
///
/// Crash litter is a SWEEP's job, not a reason to share a temp name — sharing
/// one is what tears files. Only temps older than this bound are removed, so a
/// concurrent writer's live temp is never touched: no write survives an hour, and
/// nothing here waits on a human or a network.
fn sweep(path: &Path) {
    const STALE: std::time::Duration = std::time::Duration::from_secs(3600);

    let (Some(dir), Some(base)) = (path.parent(), path.file_name()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // best-effort: a sweep must never fail a write
    };
    let prefix = format!("{}.", base.to_string_lossy());
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") {
            continue;
        }
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(|e| io::Error::other(e.to_string())))
            .map(|age| age > STALE)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn publish(tmp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let mut opts = std::fs::OpenOptions::new();
    // `create_new` is O_CREAT|O_EXCL: it fails rather than opening something that
    // already exists. So this handle is always a file WE made — never a stale
    // temp with foreign permissions, never a symlink someone else planted.
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(tmp)?;
    // `mode` above is masked by the umask, which can only NARROW it. `chmod` is
    // not masked, so this pins exactly 0600 — the file is never wider at any
    // instant, and a hostile umask cannot widen it either.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(bytes)?;
    // Durable BEFORE the swap: otherwise a crash can leave the rename landed
    // with unflushed content, i.e. an atomically-published empty file.
    f.sync_all()?;
    drop(f); // Windows will not rename an open file.

    // The rename carries the temp's inode — and therefore its 0600 — onto the
    // target. This is why the mode is set on the temp rather than afterwards:
    // a `chmod` after the rename would be a second window, and a `set_permissions`
    // on the TARGET would be undone by the next write.
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "hanzo-private-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(tmp_path(&p));
        p
    }

    #[test]
    fn writes_are_owner_only() {
        let p = scratch("mode");
        write(&p, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "must be owner-only, got {mode:o}");
        }
        let _ = std::fs::remove_file(&p);
    }

    /// The reversion red measured: hardened perms must SURVIVE a rewrite. The
    /// old `fs::write` preserved an existing file's mode; a naive tmp+rename
    /// replaces the inode and silently hands it back to `0666 & ~umask`.
    #[test]
    fn a_rewrite_does_not_revert_hardened_permissions() {
        let p = scratch("revert");
        write(&p, b"one").unwrap();
        write(&p, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a rewrite must not widen the mode, got {mode:o}");
        }
        let _ = std::fs::remove_file(&p);
    }

    /// A temp left by a crashed run must never be adopted: `O_EXCL` means we only
    /// ever write a file we created, so foreign permissions and planted symlinks
    /// cannot be inherited.
    #[test]
    fn a_stale_temp_is_never_adopted() {
        let p = scratch("stale");
        // A stale temp under SOME name must not stop or taint the next write —
        // the new write picks its own unique name.
        let stale = p.with_file_name(format!(
            "{}.dead0000dead0000.tmp",
            p.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&stale, b"leftover garbage from a crash").unwrap();

        write(&p, b"fresh").unwrap();

        assert_eq!(std::fs::read_to_string(&p).unwrap(), "fresh");
        let _ = std::fs::remove_file(&stale);
        let _ = std::fs::remove_file(&p);
    }

    /// The temp is a sibling (so `rename` stays on one filesystem and is atomic)
    /// and is UNIQUE per write (so concurrent writers cannot share one).
    #[test]
    fn the_temp_is_a_unique_sibling_of_the_target() {
        let p = PathBuf::from("/a/b/config.toml");
        let (a, b) = (tmp_path(&p), tmp_path(&p));
        assert_eq!(a.parent(), p.parent());
        assert_ne!(a, b, "two writers must never pick the same temp");
        for t in [&a, &b] {
            let n = t.file_name().unwrap().to_string_lossy().to_string();
            assert!(n.starts_with("config.toml."), "{n}");
            assert!(n.ends_with(".tmp"), "{n}");
        }
    }

    /// THE case red proved broken, and the one nothing covered: MANY writers, ONE
    /// path, NO lock. That is not exotic — `machine-id` and the run-target record
    /// are a single file per install that every concurrent `hanzo` writes.
    ///
    /// A shared temp name made one writer's `rename` publish another's
    /// half-written bytes: a value nobody wrote, and invalid JSON. The published
    /// file must ALWAYS be exactly one writer's complete value.
    #[test]
    fn concurrent_writers_to_one_path_never_publish_a_torn_file() {
        let p = scratch("torn");
        const WRITERS: usize = 12;
        // Distinct lengths and bytes, so any interleaving is unmistakable.
        let values: Vec<String> = (0..WRITERS).map(|i| format!("{}", char::from(b'a' + i as u8)).repeat(200 + i * 97)).collect();

        for _round in 0..12 {
            std::thread::scope(|s| {
                for v in &values {
                    let p = p.clone();
                    s.spawn(move || write(&p, v.as_bytes()).unwrap());
                }
            });
            let got = std::fs::read_to_string(&p).unwrap();
            assert!(
                values.contains(&got),
                "published a TORN value nobody wrote: {} bytes starting {:?}",
                got.len(),
                &got[..got.len().min(24)]
            );
        }

        // No temp survives a clean run.
        let dir = p.parent().unwrap();
        let base = p.file_name().unwrap().to_string_lossy().to_string();
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(&base) && n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temps orphaned by a clean run: {leftovers:?}");
        let _ = std::fs::remove_file(&p);
    }
}
