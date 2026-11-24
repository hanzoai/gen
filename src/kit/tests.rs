use super::*;
use crate::testmock::MockCloud;
use serde_json::json;

fn wire(v: serde_json::Value) -> Kit {
    Kit::from(serde_json::from_value::<Wire>(v).unwrap())
}

fn skill(name: &str, body: &str) -> Skill {
    Skill { name: name.into(), content: body.into() }
}

const KIT: &str = r#"{
    "skills": [
        {"name":"review","content":"---\nname: review\ndescription: review a diff\n---\nread it"}
    ],
    "servers": [{"name":"github","tools":["search_code"]}],
    "muted": ["skill_deploy","linear"]
}"#;

fn caller<'a>(api: &'a str, who: &'a str) -> Caller<'a> {
    // The mock is plain http on loopback, so it stands in for the local network.
    Caller { api, who, token: "IAM-BEARER", local: true }
}

/// The kit is carried as the cloud answers it: the cloud already left out what
/// the person muted, so nothing here re-decides it.
#[test]
fn the_kit_is_carried_as_the_cloud_answers_it() {
    let kit = wire(serde_json::from_str(KIT).unwrap());
    assert_eq!(kit.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["review"]);
    assert_eq!(kit.servers, ["github"]);
    assert!(kit.omitted.is_empty(), "a muted skill is not 'left out': {:?}", kit.omitted);

    // A name twice is carried once.
    let kit = wire(json!({"skills":[{"name":"a","content":"x"},{"name":"a","content":"y"}]}));
    assert_eq!(kit.skills, [skill("a", "x")]);
}

/// A name no directory can safely take, or a document with nothing in it, is
/// left out and SAID — never written, and never printed raw.
#[test]
fn an_unwritable_skill_is_left_out_and_named_printably() {
    let kit = wire(json!({"skills":[
        {"name":"../../.bashrc","content":"x"},
        {"name":"Upper","content":"x"},
        {"name":"empty","content":"  \n"},
        {"name":"\u{1b}[2Jwipe","content":"x"}
    ],"servers":[{"name":"\u{1b}]52;c;evil\u{7}gh"}]}));
    assert!(kit.skills.is_empty());
    assert_eq!(kit.omitted, [".....bashrc", "Upper", "empty", "2Jwipe"]);
    for s in kit.omitted.iter().chain(&kit.servers) {
        assert!(!s.contains('\u{1b}') && !s.contains('\u{7}') && !s.contains('/'), "{s:?}");
    }
}

/// The tool app's bounds hold here too: 16 skills and 512 KiB, the rest named.
#[test]
fn a_run_carries_at_most_sixteen_skills_and_512_kib() {
    let rows: Vec<_> = (0..18).map(|i| json!({"name":format!("s{i}"),"content":"x"})).collect();
    let kit = wire(json!({ "skills": rows }));
    assert_eq!(kit.skills.len(), 16);
    assert_eq!(kit.omitted, ["s16", "s17"]);

    let big = "x".repeat(300 << 10);
    let kit = wire(json!({"skills":[
        {"name":"a","content":big},
        {"name":"b","content":big},
        {"name":"c","content":"small"}
    ]}));
    assert_eq!(kit.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["a", "c"]);
    assert_eq!(kit.omitted, ["b"]);
}

#[tokio::test]
async fn the_kit_is_read_as_the_caller_with_the_iam_bearer() {
    let mock = MockCloud::start_kit(KIT).await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.base_url();
    let kit = load(&dir.path().join("kit.json"), Some(&caller(&api, "hanzo/z"))).await.unwrap();
    assert_eq!(kit.skills[0].name, "review");

    let r = &mock.requests()[0];
    assert_eq!((r.method.as_str(), r.path.as_str()), ("GET", "/v1/tool/kit"));
    assert_eq!(r.header("authorization").as_deref(), Some("Bearer IAM-BEARER"));
    assert!(r.header("x-org-id").is_none(), "the org is the bearer's, never sent");
}

/// Back-to-back runs cost one request; another identity, another network, or a
/// stale entry is a fresh read. The cache is owner-only.
#[tokio::test]
async fn a_fresh_cache_answers_for_the_same_caller_only() {
    let mock = MockCloud::start_kit(KIT).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kit.json");
    let api = mock.base_url();

    load(&path, Some(&caller(&api, "hanzo/z"))).await.unwrap();
    load(&path, Some(&caller(&api, "hanzo/z"))).await.unwrap();
    assert_eq!(mock.requests().len(), 1, "a fresh cache answers the second run");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    load(&path, Some(&caller(&api, "acme/z"))).await.unwrap();
    assert_eq!(mock.requests().len(), 2, "another org's run never reads this org's kit");

    let other = format!("{api}/");
    load(&path, Some(&caller(&other, "acme/z"))).await.unwrap();
    assert_eq!(mock.requests().len(), 3, "another network is another cache key");

    let mut c: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    c["at"] = json!(c["at"].as_i64().unwrap() - TTL);
    std::fs::write(&path, c.to_string()).unwrap();
    load(&path, Some(&caller(&other, "acme/z"))).await.unwrap();
    assert_eq!(mock.requests().len(), 4, "a stale entry is read again");
}

#[tokio::test]
async fn no_kit_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kit.json");
    assert_eq!(load(&path, None).await, Err(Miss::SignedOut));

    let mock = MockCloud::start_status(401).await;
    let api = mock.base_url();
    assert_eq!(load(&path, Some(&caller(&api, "hanzo/z"))).await, Err(Miss::Refused(401)));

    // Before the server serves a kit at all.
    let mock = MockCloud::start().await;
    let api = mock.base_url();
    assert_eq!(load(&path, Some(&caller(&api, "hanzo/z"))).await, Err(Miss::Refused(404)));

    // Nothing listening.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let api = format!("http://127.0.0.1:{port}");
    assert_eq!(load(&path, Some(&caller(&api, "hanzo/z"))).await, Err(Miss::Unreachable));
    assert!(!path.exists(), "a miss caches nothing");
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

/// Each carried skill is `<root>/<name>/SKILL.md`, recorded in a ledger outside
/// the root; a skill the kit stops carrying is withdrawn; the person's own are
/// never touched, and nothing but SKILL.md is written into the root.
#[test]
fn sync_writes_the_kit_and_withdraws_only_what_it_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let (root, ledger) = (dir.path().join("skills"), dir.path().join(LEDGER));
    std::fs::create_dir_all(root.join("mine")).unwrap();
    std::fs::write(root.join("mine").join(DOC), "my own").unwrap();

    let s = sync(&root, &ledger, &[skill("review", "r1"), skill("lint", "l1")]).unwrap();
    assert_eq!(s.written, ["review", "lint"]);
    assert_eq!(read(&root.join("review").join(DOC)), "r1");
    let entries: Vec<_> = std::fs::read_dir(root.join("review")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(entries, [DOC], "only SKILL.md is written into the root");
    assert!(read(&ledger).contains(&digest(b"r1")), "the ledger records what was written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root.join("review")), 0o700);
        assert_eq!(mode(&root.join("review").join(DOC)), 0o600);
        assert_eq!(mode(&ledger), 0o600);
    }

    // The kit changed: `lint` went away, `review` was rewritten.
    let s = sync(&root, &ledger, &[skill("review", "r2")]).unwrap();
    assert_eq!(s.written, ["review"]);
    assert_eq!(read(&root.join("review").join(DOC)), "r2");
    assert!(!root.join("lint").exists(), "a skill the kit dropped is withdrawn");
    assert_eq!(read(&root.join("mine").join(DOC)), "my own", "the person's own is untouched");

    // No kit: every org skill goes, the person's stays.
    sync(&root, &ledger, &[]).unwrap();
    assert!(!root.join("review").exists());
    assert_eq!(read(&root.join("mine").join(DOC)), "my own");
}

/// A SKILL.md the person edited is theirs: it is neither rewritten nor removed,
/// on this run or any later one, and whatever they put beside an org skill
/// outlives its withdrawal.
#[test]
fn a_customized_copy_survives() {
    let dir = tempfile::tempdir().unwrap();
    let (root, ledger) = (dir.path().join("skills"), dir.path().join(LEDGER));
    sync(&root, &ledger, &[skill("review", "org"), skill("lint", "org")]).unwrap();

    std::fs::write(root.join("review").join(DOC), "my review").unwrap();
    std::fs::write(root.join("lint").join("notes.md"), "mine").unwrap();

    let s = sync(&root, &ledger, &[skill("review", "org v2")]).unwrap();
    assert_eq!(s.yours, ["review"]);
    assert_eq!(read(&root.join("review").join(DOC)), "my review", "an edit is never overwritten");
    assert!(!root.join("lint").join(DOC).exists(), "an unchanged org file is withdrawn");
    assert_eq!(read(&root.join("lint").join("notes.md")), "mine", "the person's file beside it stays");

    sync(&root, &ledger, &[]).unwrap();
    assert_eq!(read(&root.join("review").join(DOC)), "my review", "an edit is never withdrawn");
    assert!(!read(&ledger).contains("review"), "an edited file leaves the ledger");
}

/// An org skill named like one of the person's yields to theirs, and is SAID.
/// A link in the root is never written through or removed.
#[test]
fn sync_never_writes_over_the_persons_skill_or_through_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let (root, ledger) = (dir.path().join("skills"), dir.path().join(LEDGER));
    std::fs::create_dir_all(root.join("review")).unwrap();
    std::fs::write(root.join("review").join(DOC), "my review").unwrap();

    let s = sync(&root, &ledger, &[skill("review", "org review")]).unwrap();
    assert!(s.written.is_empty());
    assert_eq!(s.yours, ["review"]);
    assert_eq!(read(&root.join("review").join(DOC)), "my review");

    #[cfg(unix)]
    {
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join(DOC), "precious").unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("linked")).unwrap();

        let s = sync(&root, &ledger, &[skill("linked", "planted")]).unwrap();
        assert_eq!(s.yours, ["linked"], "a link is not ours to write through");
        sync(&root, &ledger, &[]).unwrap();
        assert_eq!(read(&elsewhere.join(DOC)), "precious", "nor ours to remove");
        assert!(root.join("linked").exists());

        // An org skill the person replaced with a link is theirs, and what the
        // link points at is untouched when the kit drops it.
        sync(&root, &ledger, &[skill("lint", "l1")]).unwrap();
        std::fs::remove_dir_all(root.join("lint")).unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join(DOC), "l1").unwrap();
        std::os::unix::fs::symlink(&target, root.join("lint")).unwrap();
        sync(&root, &ledger, &[]).unwrap();
        assert_eq!(read(&target.join(DOC)), "l1", "a link's target is never removed");

        // A SKILL.md planted as a link inside an org skill is the person's.
        sync(&root, &ledger, &[skill("fmt", "f1")]).unwrap();
        let bashrc = dir.path().join("bashrc");
        std::fs::write(&bashrc, "precious").unwrap();
        std::fs::remove_file(root.join("fmt").join(DOC)).unwrap();
        std::os::unix::fs::symlink(&bashrc, root.join("fmt").join(DOC)).unwrap();
        let s = sync(&root, &ledger, &[skill("fmt", "f2")]).unwrap();
        assert_eq!(s.yours, ["fmt"]);
        assert_eq!(read(&bashrc), "precious");
    }

    // A name that would climb out of the root never reaches the filesystem, even
    // from a cache or a ledger someone edited.
    for name in ["../escape", "..", ".", "a/b", "/etc", "a\\b", ""] {
        let s = sync(&root, &ledger, &[skill(name, "x")]).unwrap();
        assert!(s.written.is_empty(), "{name:?} was written");
    }
    assert!(!dir.path().join("escape").exists());
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join(DOC), "x").unwrap();
    let forged = serde_json::json!({ root.to_string_lossy(): { "../outside": digest(b"x") } });
    std::fs::write(&ledger, forged.to_string()).unwrap();
    sync(&root, &ledger, &[]).unwrap();
    assert_eq!(read(&outside.join(DOC)), "x", "a forged ledger entry cannot remove a file outside the root");
}

/// The bearer goes to https, or to plain http on the local network only; a
/// refused origin is never asked.
#[tokio::test]
async fn the_bearer_goes_only_to_an_https_origin() {
    assert!(trusted("https://api.hanzo.ai", false));
    assert!(trusted("http://localhost:3690", true));
    for (api, local) in [
        ("http://api.hanzo.ai", false),
        ("ftp://api.hanzo.ai", true),
        ("https://user:pw@api.hanzo.ai", false),
        ("not a url", true),
        ("https://", false),
    ] {
        assert!(!trusted(api, local), "{api} local={local}");
    }

    let mock = MockCloud::start_kit(KIT).await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.base_url();
    let remote = Caller { api: &api, who: "hanzo/z", token: "IAM-BEARER", local: false };
    assert_eq!(load(&dir.path().join("kit.json"), Some(&remote)).await, Err(Miss::Insecure));
    assert!(mock.requests().is_empty(), "the bearer was sent over plain http");
    let e = equip(&dir.path().join("kit.json"), &dir.path().join("skills"), Some(&remote), true).await;
    assert!(e.remote.is_none());
}

/// The whole step against a mock kit and MCP server: the skills land where the
/// backend reads them, and the server a backend is handed answers an MCP
/// `initialize` for the caller — the bearer its configuration names, sent.
#[tokio::test]
async fn equip_writes_the_skills_and_hands_over_an_authenticated_mcp_server() {
    let mock = MockCloud::start_kit(KIT).await;
    let dir = tempfile::tempdir().unwrap();
    let (cache, root) = (dir.path().join("kit.json"), dir.path().join("skills"));
    let api = mock.base_url();

    let e = equip(&cache, &root, Some(&caller(&api, "hanzo/z")), true).await;
    assert!(read(&root.join("review").join(DOC)).contains("description: review a diff"));
    let remote = e.remote.expect("the kit answered and MCP is on");
    assert_eq!(remote.url, format!("{api}/v1/mcp"));

    // Each backend is handed this server; ask it what the harness would.
    let reply = reqwest::Client::new()
        .post(&remote.url)
        .bearer_auth(&remote.token)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    let body: serde_json::Value = reply.json().await.unwrap();
    assert_eq!(body["result"]["serverInfo"]["name"], "mock");
    let r = mock.requests().into_iter().find(|r| r.path == "/v1/mcp").unwrap();
    assert_eq!(r.header("authorization").as_deref(), Some("Bearer IAM-BEARER"));

    // --no-mcp: the skills still come, the server does not.
    let e = equip(&cache, &root, Some(&caller(&api, "hanzo/z")), false).await;
    assert!(e.remote.is_none() && root.join("review").exists());

    // Signed out: no server, and the org's skills are withdrawn so the run
    // truly starts without the kit the banner says it lacks.
    let e = equip(&cache, &root, None, true).await;
    assert!(e.remote.is_none());
    assert!(!root.join("review").exists());
}

/// The MCP server refuses a call without the bearer — so the test above proves
/// the harness's configuration is what authenticates it.
#[tokio::test]
async fn the_mock_mcp_server_refuses_an_anonymous_call() {
    let mock = MockCloud::start_kit(KIT).await;
    let reply = reqwest::Client::new()
        .post(format!("{}/v1/mcp", mock.base_url()))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 401);
}

#[test]
fn only_the_backends_that_read_skills_carry_a_kit() {
    let via = Route::Inherit;
    let at = Root::at("/h");
    assert!(root(&at, BackendKind::parse("agy").unwrap(), &via).is_none());
    assert!(root(&at, BackendKind::parse("cursor").unwrap(), &via).is_none());
    assert_eq!(root(&at, BackendKind::Dev, &via), Some(PathBuf::from("/h/.agents/skills")));
    assert_eq!(root(&at, BackendKind::Codex, &via), Some(PathBuf::from("/h/.agents/skills")));
    assert_eq!(root(&at, BackendKind::Claude, &via), Some(PathBuf::from("/h/.claude/skills")));
}

#[test]
fn detect_signals_extracts_workspace_stack_and_prompt_terms() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    assert!(detect_signals(p, None).is_empty(), "empty dir with no task yields no signals");

    std::fs::write(p.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
    std::fs::write(p.join("tauri.conf.json"), "{}").unwrap();
    std::fs::write(
        p.join("package.json"),
        r#"{"dependencies":{"next":"15.0.0","react":"19.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(p.join("pyproject.toml"), "[project]\nname = \"py\"\n").unwrap();
    std::fs::write(p.join("go.mod"), "module example.com/m\n").unwrap();
    std::fs::write(p.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::create_dir_all(p.join(".github").join("workflows")).unwrap();
    std::fs::write(p.join("foundry.toml"), "[profile.default]\n").unwrap();

    let sig = detect_signals(p, Some("Use $hanzo-mcp to deploy kubernetes cluster"));
    assert_eq!(sig.explicit, ["hanzo-mcp"]);
    for expected in [
        "tauri",
        "rust",
        "typescript",
        "nextjs",
        "react",
        "python",
        "go",
        "docker",
        "github-actions",
        "solidity",
        "evm",
    ] {
        assert!(sig.stack.iter().any(|s| s == expected), "missing stack signal {expected}: {:?}", sig.stack);
    }
    assert!(sig.terms.iter().any(|t| t == "deploy"));
    assert!(sig.terms.iter().any(|t| t == "kubernetes"));
    assert!(!sig.query().is_empty());
}

#[tokio::test]
async fn hanzoskills_api_endpoint_fetches_caches_and_synthesizes_skill_md() {
    let api_json = r##"{
        "skills": [
            {
                "slug": "rust-sdk",
                "name": "Hanzo Rust SDK",
                "description": "Full Rust infrastructure SDK for agents, PQC, and MCP."
            },
            {
                "slug": "wasm-rust-toolchain",
                "name": "WASM Rust Toolchain",
                "description": "Compile Rust crates to WebAssembly.",
                "content": "# WASM Toolchain\n\nBuild with wasm-bindgen."
            }
        ]
    }"##;
    let mock = MockCloud::start_hanzoskills_api(
        api_json,
        &[("rust-sdk", "# Hanzo Rust SDK\n\n14 active crates.")],
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();

    let hs_cache = dir.path().join("hanzoskills.json");
    let base = mock.base_url();
    let auto = AutoSkills {
        cache: &hs_cache,
        api: &base,
        cwd: &ws,
        task: Some("optimize tokio async"),
    };

    let skills = load_hanzoskills(&auto).await;
    assert_eq!(
        skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["rust-sdk", "wasm-rust-toolchain"]
    );
    assert!(skills[0].content.starts_with("---\nname: rust-sdk\n"));
    assert!(skills[0].content.contains("14 active crates."));
    assert!(skills[1].content.starts_with("---\nname: wasm-rust-toolchain\n"));
    assert!(skills[1].content.contains("Build with wasm-bindgen."));

    let initial_reqs = mock.requests().len();
    assert!(
        mock.requests().iter().any(|r| r.path.starts_with("/api/skills?search=rust")),
        "expected /api/skills?search=... call, got: {:?}",
        mock.requests()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&hs_cache).unwrap().permissions().mode() & 0o777, 0o600);
    }

    // Second run within the 10-minute TTL hits 0 network requests.
    let cached_skills = load_hanzoskills(&auto).await;
    assert_eq!(cached_skills, skills);
    assert_eq!(mock.requests().len(), initial_reqs, "10-minute TTL cache must prevent repeat network fetches");
}

#[tokio::test]
async fn hanzoskills_live_search_index_merges_with_kit_and_works_when_signed_out() {
    let search_json = r#"{
        "v": 1,
        "t": 3,
        "s": [
            {"i": "rust-sdk", "c": "hanzo", "d": "Hanzo Rust infrastructure SDK.", "g": ["hanzo", "rust", "sdk"]},
            {"i": "review", "c": "hanzo", "d": "Public review fallback.", "g": ["review", "rust"]},
            {"i": "python-sdk", "c": "hanzo", "d": "Hanzo Python SDK.", "g": ["hanzo", "python"]}
        ]
    }"#;
    let mock = MockCloud::start_hanzoskills_site(
        Some(KIT),
        search_json,
        &[
            ("rust-sdk", "# Hanzo Rust SDK\n\nNative crates."),
            ("review", "# Public Review\n\nPublic body."),
        ],
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("repo");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("Cargo.toml"), "[package]\nname = \"gen\"\n").unwrap();

    let (kit_cache, hs_cache, skills_root) = (
        dir.path().join("kit.json"),
        dir.path().join("hanzoskills.json"),
        dir.path().join("skills"),
    );
    let api = mock.base_url();
    let auto = AutoSkills {
        cache: &hs_cache,
        api: &api,
        cwd: &ws,
        task: None,
    };

    // 1. Signed out (`caller = None`): org kit is absent, yet public hanzoskills.com
    //    skills STILL install via kit::sync + kit-written.json!
    let e = equip_with(&kit_cache, &skills_root, None, true, Some(&auto)).await;
    assert!(e.remote.is_none());
    assert!(skills_root.join("rust-sdk").join(DOC).is_file());
    assert!(read(&skills_root.join("rust-sdk").join(DOC)).contains("Native crates."));
    assert!(read(&skills_root.join("review").join(DOC)).contains("Public body."));
    assert!(!skills_root.join("python-sdk").exists(), "unrelated language skills are not installed");

    // 2. Signed in (`caller = Some(...)`): org skill `review` overrides public `review`,
    //    while public `rust-sdk` stays alongside it!
    let e = equip_with(&kit_cache, &skills_root, Some(&caller(&api, "hanzo/z")), true, Some(&auto)).await;
    assert!(e.remote.is_some());
    assert!(
        read(&skills_root.join("review").join(DOC)).contains("description: review a diff"),
        "org kit skill must win over public skill of the same name"
    );
    assert!(skills_root.join("rust-sdk").join(DOC).is_file());
}
