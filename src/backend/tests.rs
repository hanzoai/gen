use super::*;

#[test]
fn the_toolset_is_attached_through_the_lazy_proxy() {
    let direct = McpAttach { program: "hanzo-mcp".into(), args: vec!["--project-dir".into(), "/p".into()] };
    let got = lazy(direct.clone(), Some("/opt/hanzo".into()), None);
    assert_eq!(got.program, "/opt/hanzo");
    assert_eq!(got.args, ["__mcp", "--", "hanzo-mcp", "--project-dir", "/p"]);
    let kept = lazy(direct.clone(), Some("/opt/hanzo".into()), Some(Path::new("/l.jsonl")));
    assert_eq!(kept.args, ["__mcp", "--ledger", "/l.jsonl", "--", "hanzo-mcp", "--project-dir", "/p"]);
    // No path to this binary: the server is attached as it is.
    assert_eq!(lazy(direct, None, None).program, "hanzo-mcp");
}

#[test]
fn the_server_beside_this_binary_wins_over_path() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("hanzo");
    std::fs::write(&exe, b"").unwrap();
    assert_eq!(beside(Some(&exe), MCP_BIN), None);
    std::fs::write(dir.path().join(MCP_BIN), b"").unwrap();
    assert_eq!(beside(Some(&exe), MCP_BIN), Some(dir.path().canonicalize().unwrap().join(MCP_BIN)));
}

fn sel(positional: Option<&str>, tail: Option<&str>, named: Option<&str>) -> Selection {
    Selection {
        positional: positional.map(String::from),
        tail: tail.map(String::from),
        named: named.map(String::from),
        configured: None,
    }
}

/// Same, with an agent configured in `~/.hanzo/settings.json`.
fn sel_cfg(named: Option<&str>, configured: &str) -> Selection {
    Selection {
        positional: None,
        tail: None,
        named: named.map(String::from),
        configured: Some(configured.to_string()),
    }
}

#[test]
fn a_configured_agent_runs_when_nothing_names_one() {
    let (kind, _) = select(sel_cfg(None, "claude")).unwrap();
    assert_eq!(kind, BackendKind::Claude);
    let (kind, _) = select(sel_cfg(None, "codex")).unwrap();
    assert_eq!(kind, BackendKind::Codex);
}

#[test]
fn naming_one_beats_the_configured_agent() {
    // An invocation is more specific than a default, in both directions.
    let (kind, _) = select(sel_cfg(Some("dev"), "claude")).unwrap();
    assert_eq!(kind, BackendKind::Dev);
    let (kind, _) = select(sel_cfg(Some("codex"), "claude")).unwrap();
    assert_eq!(kind, BackendKind::Codex);
}

#[test]
fn an_unreadable_configured_agent_is_refused_not_ignored() {
    // Falling back to ours would run a different product than the reader
    // asked for, and say nothing about it.
    let err = select(sel_cfg(None, "clod")).unwrap_err().to_string();
    assert!(err.contains("clod"), "{err}");
    assert!(err.contains("settings.json"), "{err}");
}

#[test]
fn no_agent_configured_is_still_ours() {
    let (kind, _) = select(sel(None, None, None)).unwrap();
    assert_eq!(kind, DEFAULT);
}

/// The point of [`select`]: every spelling is the SAME resolution. If these
/// ever disagree, someone has forked the command.
#[test]
fn every_spelling_resolves_to_the_same_backend() {
    for (positional, named) in [(Some("dev"), None), (None, Some("dev"))] {
        let (kind, task) = select(sel(positional, None, named)).unwrap();
        assert_eq!(kind, BackendKind::Dev);
        assert_eq!(task, None);
    }
    for (positional, named) in [(Some("claude"), None), (None, Some("claude"))] {
        assert_eq!(
            select(sel(positional, None, named)).unwrap().0,
            BackendKind::Claude
        );
    }
    for (positional, named) in [(Some("codex"), None), (None, Some("codex"))] {
        assert_eq!(
            select(sel(positional, None, named)).unwrap().0,
            BackendKind::Codex
        );
    }
}

/// Naming nothing runs OUR agent — `hanzo dev` and a bare `hanzo` alike.
#[test]
fn the_default_backend_is_ours() {
    assert_eq!(select(sel(None, None, None)).unwrap().0, BackendKind::Dev);
}

/// A lone operand is the TASK unless it is exactly a backend name — the one
/// rule that lets a bare `hanzo claude` and `hanzo "fix it"` both work.
#[test]
fn a_lone_operand_is_a_task_unless_it_names_a_backend() {
    let (kind, task) = select(sel(Some("fix the failing test"), None, None)).unwrap();
    assert_eq!(
        kind,
        BackendKind::Dev,
        "an unrecognised operand must not change the backend"
    );
    assert_eq!(task.as_deref(), Some("fix the failing test"));

    // `hanzo claude "fix it"` — backend positionally, task after it.
    let (kind, task) = select(sel(Some("claude"), Some("fix it"), None)).unwrap();
    assert_eq!(kind, BackendKind::Claude);
    assert_eq!(task.as_deref(), Some("fix it"));
}

/// Naming the backend twice is refused outright. A precedence rule here is
/// one nobody could remember, and silently running an agent the user did not
/// name is worse than refusing.
#[test]
fn naming_the_backend_twice_is_an_error() {
    let err = select(sel(Some("claude"), None, Some("codex")))
        .unwrap_err()
        .to_string();
    assert!(err.contains("named twice"), "got: {err}");
    assert!(
        err.contains("claude") && err.contains("codex"),
        "both spellings named: {err}"
    );

    // An unquoted multi-word task is a mistake worth catching, not a silent join.
    let err = select(sel(Some("fix"), Some("it"), None))
        .unwrap_err()
        .to_string();
    assert!(err.contains("unexpected argument"), "got: {err}");
}

#[test]
fn backend_kind_parse() {
    assert_eq!(BackendKind::parse("dev").unwrap(), BackendKind::Dev);
    assert_eq!(BackendKind::parse("claude").unwrap(), BackendKind::Claude);
    assert_eq!(BackendKind::parse("CC").unwrap(), BackendKind::Claude);
    // `codex` is its OWN backend. Aliasing it to `dev` would silently run a
    // different agent than the one the user named.
    assert_eq!(BackendKind::parse("codex").unwrap(), BackendKind::Codex);
    for (spelling, name) in [("agy", "agy"), ("antigravity", "agy"), ("cursor", "cursor"), ("agent", "cursor"), ("cursor-agent", "cursor"), ("qwen-code", "qwen"), ("kilocode", "kilo"), ("gemini", "gemini")] {
        assert_eq!(BackendKind::parse(spelling).unwrap().as_str(), name, "{spelling}");
    }
    assert!(BackendKind::parse("gemini").unwrap().own_account_only());
    assert!(!BackendKind::parse("opencode").unwrap().own_account_only());
    assert!(BackendKind::parse("gpt").is_err());
}

/// `Routing`'s `Debug` must REDACT the secret. No path prints it today,
/// but `-vvv` wires `trace!`, so a future `trace!(?routing)` would otherwise
/// leak an `sk-ant-…`/`hk-…`/bearer. The non-secret `api` stays for debugging.
#[test]
fn routing_debug_redacts_the_secret() {
    let g = Routing::Gateway {
        api: "https://api.hanzo.ai".into(),
        token: "sk-SECRET-TOKEN".into(),
        org: Some("hanzo".into()),
        model: "enso".into(),
        small_fast_model: "enso-flash".into(),
        context_window: 1_000_000,
    };
    let s = format!("{g:?}");
    assert!(!s.contains("sk-SECRET-TOKEN"), "token leaked in Debug: {s}");
    assert!(s.contains("***"), "expected a redaction marker: {s}");
    assert!(
        s.contains("api.hanzo.ai"),
        "the non-secret api should survive: {s}"
    );

    assert!(!format!(
        "{:?}",
        Routing::Anthropic {
            key: "sk-ant-SECRET".into()
        }
    )
    .contains("sk-ant-SECRET"));
    assert!(!format!(
        "{:?}",
        Routing::OpenAI {
            key: "sk-proj-SECRET".into()
        }
    )
    .contains("sk-proj-SECRET"));

    // `Route` composes the redacting `Debug`, so wrapping never re-exposes it.
    let r = Route::Via(Routing::Gateway {
        api: "x".into(),
        token: "sk-INNER".into(),
        org: None,
        model: "enso".into(),
        small_fast_model: "enso-flash".into(),
        context_window: 1_000_000,
    });
    assert!(
        !format!("{r:?}").contains("sk-INNER"),
        "Route::Via leaked the inner secret"
    );
}

/// `Route::via()` yields the credential only for `Via`; the two no-credential
/// outcomes both read as `None` (what the banner/status line consume).
#[test]
fn route_via_exposes_only_the_resolved_credential() {
    assert!(Route::Via(Routing::OpenAI { key: "sk-x".into() })
        .via()
        .is_some());
    assert!(Route::Inherit.via().is_none());
    assert!(Route::FailClosed.via().is_none());
}

#[test]
fn mcp_release_target_matches_published_archive_names() {
    assert_eq!(mcp_release_target("linux", "x86_64"), Some("linux-amd64"));
    assert_eq!(mcp_release_target("linux", "aarch64"), Some("linux-arm64"));
    assert_eq!(mcp_release_target("macos", "x86_64"), Some("darwin-amd64"));
    assert_eq!(mcp_release_target("macos", "aarch64"), Some("darwin-arm64"));
    assert_eq!(mcp_release_target("windows", "x86_64"), Some("windows-amd64"));
    assert_eq!(mcp_release_target("freebsd", "x86_64"), None);
}

fn make_mcp_tarball(exe_name: &str, contents: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut builder = tar::Builder::new(&mut gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, exe_name, contents).unwrap();
        builder.finish().unwrap();
    }
    gz.finish().unwrap()
}

async fn serve_mcp_release(tarball: Vec<u8>, sha_line: String) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { break };
            let tarball = tarball.clone();
            let sha_line = sha_line.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
                let (status, body): (&str, Vec<u8>) = if path.ends_with(".sha256") {
                    ("200 OK", sha_line.into_bytes())
                } else if path.ends_with(".tar.gz") {
                    ("200 OK", tarball)
                } else {
                    ("404 Not Found", b"not found".to_vec())
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn ensure_mcp_installed_verifies_sha256_and_installs_atomically() {
    use sha2::{Digest, Sha256};

    let bin_bytes = b"#!/bin/sh\necho hanzo-mcp-stub\n";
    let tarball = make_mcp_tarball("hanzo-mcp", bin_bytes);
    let sum = format!("{:x}  hanzo-mcp-linux-amd64.tar.gz\n", Sha256::digest(&tarball));
    let base = serve_mcp_release(tarball.clone(), sum).await;

    let dir = tempfile::tempdir().unwrap();
    let root = Root::at(dir.path());
    let installed = ensure_mcp_installed_from(&root, &base, "linux-amd64").await.unwrap();
    assert_eq!(installed, dir.path().join(".local/bin/hanzo-mcp"));
    assert_eq!(std::fs::read(&installed).unwrap(), bin_bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&installed).unwrap().permissions().mode() & 0o777, 0o755);
    }

    // And a tampered checksum is refused without leaving any binary behind.
    let bad_base = serve_mcp_release(tarball, format!("{}\n", "0".repeat(64))).await;
    let dir2 = tempfile::tempdir().unwrap();
    let root2 = Root::at(dir2.path());
    let err = ensure_mcp_installed_from(&root2, &bad_base, "linux-amd64").await.unwrap_err().to_string();
    assert!(err.contains("SHA-256 mismatch"), "got: {err}");
    assert!(!dir2.path().join(".local/bin/hanzo-mcp").exists());
}
