use super::*;
use crate::testmock::MockCloud;

struct StubHost {
    api: String,
    org: Option<String>,
}

impl Host for StubHost {
    fn profile(&self) -> crate::host::Pending<'_, crate::Profile> {
        let mut p = crate::Profile::unread();
        p.api = self.api.clone();
        p.org = self.org.clone();
        Box::pin(async move { p })
    }
    fn account<'a>(&'a self, _: &'a str) -> crate::host::Pending<'a, Result<Option<crate::Account>>> {
        Box::pin(async {
            Ok(Some(crate::Account {
                owner: "jamessburnss1313".into(),
                name: "james".into(),
                token: "TOK".into(),
            }))
        })
    }
    fn refresh(&self, _: &str) -> Vec<String> {
        Vec::new()
    }
    fn key(&self, _: crate::Provider) -> Result<Option<String>> {
        Ok(None)
    }
    fn meter<'a>(&'a self, _: &'a str, _: &'a [String]) -> crate::host::Pending<'a, std::result::Result<serde_json::Value, String>> {
        Box::pin(async { Err("no meter".into()) })
    }
}

async fn until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..3000 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn runner_mints_key_claims_clones_commits_pushes_and_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    let seed = tmp.path().join("seed");

    run_git(None, &["init", "--bare", "--initial-branch=main", &origin.to_string_lossy()]).await.unwrap();
    run_git(None, &["clone", &origin.to_string_lossy(), &seed.to_string_lossy()]).await.unwrap();
    run_git(Some(&seed), &["config", "user.email", "test@hanzo.ai"]).await.unwrap();
    run_git(Some(&seed), &["config", "user.name", "Test"]).await.unwrap();
    std::fs::write(seed.join("README.md"), "hello\n").unwrap();
    run_git(Some(&seed), &["add", "."]).await.unwrap();
    run_git(Some(&seed), &["commit", "-m", "initial"]).await.unwrap();
    run_git(Some(&seed), &["push", "-u", "origin", "HEAD:refs/heads/main"]).await.unwrap();

    let queued = RoutedRun {
        session_id: "sess_routed_1".into(),
        repo: "jamessburnss1313-dev/guided-growth".into(),
        project: String::new(),
        base: "main".into(),
        branch: "hanzo/fix-landing".into(),
        prompt: "Update README".into(),
        clone_url: origin.to_string_lossy().to_string(),
        timeout_seconds: 60,
        model: "enso-auto".into(),
        effort: "medium".into(),
        instructions: String::new(),
        deploy_key: Secret::default(),
        known_hosts: String::new(),
        dir: String::new(),
       read_only: false,
 };

    let mock = MockCloud::start_with_routed_runs(vec![queued], true).await;
    let root = crate::root::Root::at(tmp.path().join("hanzo-root"));
    let host = Arc::new(StubHost {
        api: mock.base_url(),
        org: Some("jamessburnss1313".into()),
    });

    let exec: Executor = Arc::new(|run, cwd| {
        Box::pin(async move {
            assert_eq!(run.session_id, "sess_routed_1");
            std::fs::write(cwd.join("README.md"), "hello from live runner\n")?;
            Ok(None)
        })
    });

    let held = serve_with(host, &root, "hanzo", &mock.base_url(), "mach_1", "James-MacBook-Air.local", target::grants(), exec);
    until("routed run report to land", || {
        mock.requests().iter().any(|r| r.method == "POST" && r.path == "/v1/agent/targets/tgt_mock/runs/sess_routed_1/report")
    })
    .await;
    drop(held);

    let reqs = mock.requests();
    let key_mints = reqs.iter().filter(|r| r.method == "POST" && r.path == "/v1/agent/targets/tgt_mock/key").count();
    assert!(key_mints >= 2, "must mint initial key and re-mint after simulated 403 rotation");

    let report_req = reqs
        .iter()
        .find(|r| r.method == "POST" && r.path == "/v1/agent/targets/tgt_mock/runs/sess_routed_1/report")
        .unwrap();
    assert_eq!(report_req.header("x-org-id").as_deref(), Some("jamessburnss1313"));
    assert_eq!(report_req.header("x-target-key").as_deref(), Some("ck_fresh"));
    let body = report_req.json();
    assert_eq!(body["ok"], true);
    assert_eq!(body["changed"], true);
    assert_eq!(body["branch"], "hanzo/fix-landing");
    assert!(!body["commitSha"].as_str().unwrap_or_default().is_empty());
    assert!(body["diffstat"].as_str().unwrap_or_default().contains("README.md"));

    let pushed_sha = run_git(Some(&origin), &["rev-parse", "refs/heads/hanzo/fix-landing"]).await.unwrap();
    assert_eq!(pushed_sha, body["commitSha"].as_str().unwrap());

    let commit = run_git(Some(&origin), &["log", "-1", "--format=%an|%s", "refs/heads/hanzo/fix-landing"]).await.unwrap();
    let (author, subject) = commit.split_once('|').unwrap();
    assert_ne!(author, "Hanzo Dev");
    assert_eq!(subject, "chore: apply changes from Hanzo run");
}

// A run with its own deploy key clones and pushes through it, and the key is gone
// with the run. A local path stands in for the forge: git never asks it for ssh.
#[tokio::test]
async fn a_runs_deploy_key_is_held_for_the_run_and_removed_after() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    run_git(None, &["init", "--bare", "--initial-branch=main", &origin.to_string_lossy()]).await.unwrap();
    let seed = tmp.path().join("seed");
    run_git(None, &["clone", &origin.to_string_lossy(), &seed.to_string_lossy()]).await.unwrap();
    run_git(Some(&seed), &["config", "user.email", "t@hanzo.ai"]).await.unwrap();
    run_git(Some(&seed), &["config", "user.name", "T"]).await.unwrap();
    std::fs::write(seed.join("a"), "a\n").unwrap();
    run_git(Some(&seed), &["add", "."]).await.unwrap();
    run_git(Some(&seed), &["commit", "-m", "init"]).await.unwrap();
    run_git(Some(&seed), &["push", "origin", "HEAD:refs/heads/main"]).await.unwrap();

    let work_dir = tmp.path().join("workspace");
    let keys = tmp.path().join("workspace.ssh");
    let run = RoutedRun {
        session_id: "sess_key_1".into(),
        repo: "api".into(),
        project: String::new(),
        base: "main".into(),
        branch: "agent/key".into(),
        prompt: "x".into(),
        clone_url: origin.to_string_lossy().to_string(),
        timeout_seconds: 30,
        model: String::new(),
        effort: String::new(),
        instructions: String::new(),
        deploy_key: serde_json::from_str("\"PRIVATE-KEY\"").unwrap(),
        known_hosts: "git.test ssh-ed25519 AAAA".into(),
        dir: String::new(),
       read_only: false,
 };
    let seen = keys.clone();
    let exec: Executor = Arc::new(move |_run, cwd| {
        let keys = seen.clone();
        Box::pin(async move {
            let key = std::fs::read_to_string(keys.join("key"))?;
            anyhow::ensure!(key.trim() == "PRIVATE-KEY", "the key was not written");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(keys.join("key"))?.permissions().mode() & 0o777;
                anyhow::ensure!(mode == 0o600, "the key is mode {mode:o}");
            }
            let through = git_stdout(&cwd, &["config", "core.sshCommand"]).await?;
            anyhow::ensure!(through.contains("IdentitiesOnly=yes") && through.contains("StrictHostKeyChecking=yes"), "{through}");
            std::fs::write(cwd.join("b"), "b\n")?;
            Ok(None)
        })
    });
    let report = execute_workspace_run(&work_dir, &run, exec).await;
    assert!(report.ok, "{}", report.error);
    assert!(report.changed);
    assert!(!keys.exists(), "the key outlived its run");
    assert_eq!(format!("{:?}", run.deploy_key), "Secret(..)");
    assert!(!format!("{run:?}").contains("PRIVATE-KEY"));
}

// A clone that fails reports git's own reason, not only the wrapper that names the step:
// "Permission denied (publickey)" is what tells the owner what to fix.
#[tokio::test]
async fn a_failed_clone_reports_gits_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let mut run = folder_run(tmp.path());
    run.dir = String::new();
    run.session_id = "sess_clone_fail".into();
    run.base = "main".into();
    run.clone_url = tmp.path().join("no-such-repo").to_string_lossy().into_owned();
    let exec: Executor = Arc::new(|_run, _cwd| Box::pin(async { Ok(None) }));
    let report = execute_workspace_run(&tmp.path().join("workspace"), &run, exec).await;
    assert!(!report.ok);
    assert!(report.error.starts_with("workspace setup failed: git clone failed"), "{}", report.error);
    assert!(report.error.contains("does not exist"), "git's reason was lost: {}", report.error);
}

// A push the origin refuses reports the origin's words too, not only "git push ... failed".
#[cfg(unix)]
#[tokio::test]
async fn a_refused_push_reports_gits_reason() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    let seed = tmp.path().join("seed");
    run_git(None, &["init", "--bare", "--initial-branch=main", &origin.to_string_lossy()]).await.unwrap();
    run_git(None, &["clone", &origin.to_string_lossy(), &seed.to_string_lossy()]).await.unwrap();
    run_git(Some(&seed), &["config", "user.email", "test@hanzo.ai"]).await.unwrap();
    run_git(Some(&seed), &["config", "user.name", "Test"]).await.unwrap();
    std::fs::write(seed.join("README.md"), "hello\n").unwrap();
    run_git(Some(&seed), &["add", "."]).await.unwrap();
    run_git(Some(&seed), &["commit", "-m", "initial"]).await.unwrap();
    run_git(Some(&seed), &["push", "origin", "HEAD:refs/heads/main"]).await.unwrap();
    let hook = origin.join("hooks").join("pre-receive");
    std::fs::write(&hook, "#!/bin/sh\necho \"refused by test\" >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut run = folder_run(tmp.path());
    run.dir = String::new();
    run.session_id = "sess_push_refused".into();
    run.base = "main".into();
    run.branch = "hanzo/refused".into();
    run.clone_url = origin.to_string_lossy().into_owned();
    let exec: Executor = Arc::new(|_run, cwd| {
        Box::pin(async move {
            std::fs::write(cwd.join("README.md"), "changed\n")?;
            Ok(None)
        })
    });
    let report = execute_workspace_run(&tmp.path().join("workspace"), &run, exec).await;
    assert!(!report.ok);
    assert!(report.error.contains("git push origin HEAD:refs/heads/"), "{}", report.error);
    assert!(report.error.contains("refused by test"), "the origin's reason was lost: {}", report.error);
}

// A run with no repository still works in a directory the agent accepts: `dev`
// (codex exec) refuses one that is not inside a git work tree and exits at once, which is
// how every bare run on a linked machine ended in an error nobody read.
#[tokio::test]
async fn a_run_without_a_repository_works_inside_a_git_work_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().join("workspace");
    let run = RoutedRun {
        session_id: "sess_bare_1".into(),
        repo: String::new(),
        project: String::new(),
        base: String::new(),
        branch: String::new(),
        prompt: "Say ok".into(),
        clone_url: String::new(),
        timeout_seconds: 30,
        model: "enso-auto".into(),
        effort: "medium".into(),
        instructions: String::new(),
        deploy_key: Secret::default(),
        known_hosts: String::new(),
        dir: String::new(),
       read_only: false,
 };
    let exec: Executor = Arc::new(|_run, cwd| {
        Box::pin(async move {
            let inside = git_stdout(&cwd, &["rev-parse", "--is-inside-work-tree"]).await?;
            anyhow::ensure!(inside == "true", "the workspace is not a git work tree: {inside:?}");
            Ok(None)
        })
    });
    let report = execute_workspace_run(&work_dir, &run, exec).await;
    assert!(report.ok, "a bare run failed: {}", report.error);
    assert!(!report.changed);
}

// A read-only run answers with changed:false and NEVER reaches the commit/push
// finalize step, even when the workspace has a branch to push to.
#[tokio::test]
async fn a_read_only_workspace_run_never_pushes_and_answers_changed_false() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().join("workspace");
    let mut run = RoutedRun {
        session_id: "sess_readonly_1".into(),
        repo: String::new(),
        project: String::new(),
        base: String::new(),
        branch: "agent/should-not-be-written".into(),
        prompt: "review it".into(),
        clone_url: String::new(),
        timeout_seconds: 30,
        model: String::new(),
        effort: String::new(),
        instructions: String::new(),
        deploy_key: Secret::default(),
        known_hosts: String::new(),
        dir: String::new(),
        read_only: true,
    };
    let exec: Executor = Arc::new(|_run, cwd| Box::pin(async move {
        std::fs::write(cwd.join("notes.txt"), "answer only\n")?;
        // The runner's second check sees the write and refuses the run. A real
        // agent is also blocked by the OS guard before it can get here.
        Ok(None)
    }));
    let report = execute_workspace_run(&work_dir, &run, exec).await;
    assert!(!report.ok, "a write in a read-only run must fail closed");
    assert!(!report.changed, "no commit may be reported for a read-only run");
    assert!(report.commit_sha.is_empty());
    assert!(report.error.contains("read_only_violation"), "{}", report.error);
    assert!(git_stdout(&work_dir, &["status", "--porcelain"]).await.unwrap().contains("notes.txt"));

    // The same run with no writes is an answer: ok, changed:false, no push.
    run.session_id = "sess_readonly_2".into();
    let clean_dir = tmp.path().join("workspace-clean");
    let exec: Executor = Arc::new(|_run, _cwd| Box::pin(async move { Ok(None) }));
    let report = execute_workspace_run(&clean_dir, &run, exec).await;
    assert!(report.ok, "{}", report.error);
    assert!(!report.changed && report.commit_sha.is_empty());
}

#[tokio::test]
async fn execute_workspace_run_propagates_valid_agent_run_telemetry() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().join("workspace");
    let run = RoutedRun {
        session_id: "sess_telemetry_1".into(),
        repo: String::new(),
        project: String::new(),
        base: String::new(),
        branch: String::new(),
        prompt: "Verify telemetry".into(),
        clone_url: String::new(),
        timeout_seconds: 30,
        model: "enso-auto".into(),
        effort: "medium".into(),
        instructions: String::new(),
        deploy_key: Secret::default(),
        known_hosts: String::new(),
        dir: String::new(),
       read_only: false,
 };
    let exec: Executor = Arc::new(|r, _cwd| {
        Box::pin(async move {
            let mut telemetry = AgentRun::new(
                &r.session_id,
                &r.session_id,
                "0.1.0",
                &r.model,
                true,
                crate::summary::url(&r.session_id),
            );
            telemetry.execution.steps = 2;
            telemetry.execution.tool_calls = 3;
            telemetry.execution.elapsed_ms = 250;
            telemetry.usage.input_tokens = 100;
            telemetry.usage.output_tokens = 40;
            telemetry.usage.cost_usd = 0.001;
            telemetry.verification.checks_passed = true;
            Ok(Some(telemetry))
        })
    });

    let report = execute_workspace_run(&work_dir, &run, exec).await;
    assert!(report.ok);
    let telemetry = report.telemetry.as_ref().expect("telemetry should be populated");
    assert!(telemetry.validate().passed);

    let serialized = serde_json::to_value(&report).unwrap();
    let check = crate::contract::validate_agent_run_json(&serialized["telemetry"]);
    assert!(check.passed, "violations: {:?}", check.violations);
    assert_eq!(serialized["telemetry"]["runId"], "sess_telemetry_1");
    assert_eq!(serialized["telemetry"]["execution"]["toolCalls"], 3);
}

fn folder_run(dir: &Path) -> RoutedRun {
    RoutedRun {
        session_id: "sess_folder_1".into(),
        repo: String::new(),
        project: String::new(),
        base: String::new(),
        branch: String::new(),
        prompt: "Tidy the notes".into(),
        clone_url: String::new(),
        timeout_seconds: 30,
        model: String::new(),
        effort: String::new(),
        instructions: String::new(),
        deploy_key: Secret::default(),
        known_hosts: String::new(),
        dir: dir.to_string_lossy().into_owned(),
       read_only: false,
 }
}

fn granted(folders: &[&Path]) -> Grants {
    let g = target::grants();
    g.send_replace(Some(folders.iter().map(|f| f.to_string_lossy().into_owned()).collect()));
    g
}

fn refused(r: Result<PathBuf>) -> bool {
    r.is_err_and(|e| e.to_string().starts_with(FOLDER_NOT_GRANTED))
}

// A grant covers itself and what lies under it, and nothing beside it.
#[test]
fn a_grant_holds_its_folder_and_what_is_under_it() {
    let tmp = tempfile::tempdir().unwrap();
    let site = tmp.path().join("site");
    std::fs::create_dir_all(site.join("src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("site2")).unwrap();
    let grant = [site.to_string_lossy().into_owned()];
    let real = std::fs::canonicalize(&site).unwrap();
    assert_eq!(inside(&site, &grant).unwrap(), real);
    assert_eq!(inside(&site.join("src"), &grant).unwrap(), real.join("src"));
    assert!(refused(inside(&tmp.path().join("site2"), &grant)), "a sibling sharing a prefix is not inside");
    assert!(refused(inside(tmp.path(), &grant)), "a parent is not inside");
    assert!(refused(inside(Path::new("site/src"), &grant)), "a relative dir is not inside");
    assert!(refused(inside(&site, &[])), "no grant, no folder");
}

// A symlink inside a grant that points out of it is refused; one pointing in is not.
#[cfg(unix)]
#[test]
fn a_symlink_out_of_a_grant_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let site = tmp.path().join("site");
    let secrets = tmp.path().join("secrets");
    std::fs::create_dir_all(site.join("src")).unwrap();
    std::fs::create_dir_all(&secrets).unwrap();
    std::os::unix::fs::symlink(&secrets, site.join("out")).unwrap();
    std::os::unix::fs::symlink(site.join("src"), tmp.path().join("in")).unwrap();
    let grant = [site.to_string_lossy().into_owned()];
    assert!(refused(inside(&site.join("out"), &grant)));
    assert!(refused(inside(&site.join("out").join("."), &grant)));
    assert_eq!(inside(&tmp.path().join("in"), &grant).unwrap(), std::fs::canonicalize(site.join("src")).unwrap());
}

// `..` is resolved before the check: climbing out is refused, climbing back in is not.
#[test]
fn dot_dot_cannot_climb_out_of_a_grant() {
    let tmp = tempfile::tempdir().unwrap();
    let site = tmp.path().join("site");
    std::fs::create_dir_all(site.join("src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("other")).unwrap();
    let grant = [site.to_string_lossy().into_owned()];
    assert!(refused(inside(&site.join("..").join("other"), &grant)));
    assert!(refused(inside(&site.join(".."), &grant)));
    assert_eq!(inside(&site.join("src").join(".."), &grant).unwrap(), std::fs::canonicalize(&site).unwrap());
}

// A folder that is not on disk is refused, and a grant that is not on disk grants nothing.
#[test]
fn a_missing_folder_or_grant_grants_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let site = tmp.path().join("site");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(site.join("notes.md"), "x").unwrap();
    let grant = [site.to_string_lossy().into_owned()];
    assert!(refused(inside(&site.join("gone"), &grant)));
    assert!(refused(inside(&site.join("notes.md"), &grant)), "a file is not a folder to work in");
    let gone = [tmp.path().join("gone").to_string_lossy().into_owned()];
    assert!(refused(inside(&site, &gone)));
}

// A run in a granted folder works there in place: no clone, no repository made, and
// what the agent wrote stays.
#[tokio::test]
async fn a_run_in_a_granted_folder_works_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let site = tmp.path().join("site");
    std::fs::create_dir_all(&site).unwrap();
    let real = std::fs::canonicalize(&site).unwrap();
    let exec: Executor = Arc::new(move |_run, cwd| {
        let real = real.clone();
        Box::pin(async move {
            anyhow::ensure!(cwd == real, "the run worked in {}", cwd.display());
            std::fs::write(cwd.join("notes.md"), "done\n")?;
            Ok(None)
        })
    });
    let report = execute_folder_run(&folder_run(&site), &granted(&[&site]), exec).await;
    assert!(report.ok, "{}", report.error);
    assert!(!report.changed);
    assert_eq!(std::fs::read_to_string(site.join("notes.md")).unwrap(), "done\n");
    assert!(!site.join(".git").exists(), "the folder was made a repository");
}

// A run outside every grant runs nothing and reports folder_not_granted.
#[tokio::test]
async fn a_run_outside_every_grant_runs_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let site = tmp.path().join("site");
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = ran.clone();
    let exec: Executor = Arc::new(move |_run, _cwd| {
        seen.store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    });
    for grants in [granted(&[&site]), target::grants()] {
        let report = execute_folder_run(&folder_run(&other), &grants, exec.clone()).await;
        assert!(!report.ok);
        assert!(report.error.starts_with(FOLDER_NOT_GRANTED), "{}", report.error);
    }
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst), "a refused run executed");
}

// A beat that no longer carries the run's folder stops the run at once.
#[tokio::test]
async fn revoking_the_folder_stops_the_run() {
    struct Dropped(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let site = tmp.path().join("site");
    let keep = tmp.path().join("keep");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::create_dir_all(&keep).unwrap();
    let grants = granted(&[&site, &keep]);
    let started = Arc::new(tokio::sync::Notify::new());
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (go, flag) = (started.clone(), stopped.clone());
    let exec: Executor = Arc::new(move |_run, _cwd| {
        let (go, guard) = (go.clone(), Dropped(flag.clone()));
        Box::pin(async move {
            let _guard = guard;
            go.notify_one();
            std::future::pending::<()>().await;
            Ok(None)
        })
    });
    let run = folder_run(&site);
    let revoke = async {
        started.notified().await;
        grants.send_replace(Some(vec![keep.to_string_lossy().into_owned()]));
    };
    let (report, ()) = tokio::join!(execute_folder_run(&run, &grants, exec), revoke);
    assert!(!report.ok);
    assert!(report.error.starts_with(FOLDER_NOT_GRANTED) && report.error.contains("revoked"), "{}", report.error);
    assert!(stopped.load(std::sync::atomic::Ordering::SeqCst), "the agent outlived the revoke");
}

// The runner learns the granted folders from cloud's answer and checks a claimed run's
// dir against them: one inside runs, one outside is reported folder_not_granted.
#[tokio::test]
async fn the_runner_checks_a_claimed_runs_folder_against_the_beat() {
    let tmp = tempfile::tempdir().unwrap();
    let site = tmp.path().join("site");
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let mut outside = folder_run(&other);
    outside.session_id = "sess_folder_2".into();
    let mock = MockCloud::start_with_routed_runs(vec![folder_run(&site), outside], false).await;
    mock.grant(&[&site.to_string_lossy()]);
    let root = crate::root::Root::at(tmp.path().join("hanzo-root"));
    let host = Arc::new(StubHost { api: mock.base_url(), org: None });
    let exec: Executor = Arc::new(|run, _cwd| {
        Box::pin(async move {
            anyhow::ensure!(run.session_id == "sess_folder_1", "{} ran", run.session_id);
            Ok(None)
        })
    });
    let held = serve_with(host, &root, "hanzo", &mock.base_url(), "mach_1", "evo", target::grants(), exec);
    let report = |id: &str| {
        let path = format!("/v1/agent/targets/tgt_mock/runs/{id}/report");
        mock.requests().into_iter().find(|r| r.path == path).map(|r| r.json())
    };
    until("both reports", || report("sess_folder_1").is_some() && report("sess_folder_2").is_some()).await;
    drop(held);
    assert_eq!(report("sess_folder_1").unwrap()["ok"], true);
    let denied = report("sess_folder_2").unwrap();
    assert_eq!(denied["ok"], false);
    assert!(denied["error"].as_str().unwrap().starts_with(FOLDER_NOT_GRANTED), "{denied}");
}

// The runner reports what became of a folder run queued after it started, with the
// folder granted (or not) only after the runner last heard the grants.
async fn folder_run_after_the_runner_heard_the_grants(grant_it: bool) -> serde_json::Value {
    let tmp = tempfile::tempdir().unwrap();
    let site = tmp.path().join("site");
    std::fs::create_dir_all(&site).unwrap();
    let mock = MockCloud::start_with_routed_runs(Vec::new(), false).await;
    let root = crate::root::Root::at(tmp.path().join("hanzo-root"));
    let host = Arc::new(StubHost { api: mock.base_url(), org: None });
    let exec: Executor = Arc::new(|_run, _cwd| Box::pin(async { Ok(None) }));
    let held = serve_with(host, &root, "hanzo", &mock.base_url(), "mach_1", "evo", target::grants(), exec);
    let path = "/v1/agent/targets/tgt_mock/runs/sess_folder_1/report";
    // A claim poll means the runner has already heard the (empty) grants.
    until("the first claim", || mock.requests().iter().any(|r| r.path.ends_with("/claim"))).await;
    if grant_it {
        mock.grant(&[&site.to_string_lossy()]);
    }
    mock.queue(folder_run(&site));
    until("the report", || mock.requests().iter().any(|r| r.path == path)).await;
    drop(held);
    mock.requests().into_iter().find(|r| r.path == path).unwrap().json()
}

// A folder granted after the runner's last beat runs: the runner asks cloud again
// before it refuses, instead of waiting for the next beat.
#[tokio::test]
async fn a_folder_granted_since_the_last_beat_runs() {
    let report = folder_run_after_the_runner_heard_the_grants(true).await;
    assert_eq!(report["ok"], true, "{report}");
}

// The fresh answer decides: a folder cloud still does not list is refused.
#[tokio::test]
async fn a_folder_still_not_granted_after_the_refresh_is_refused() {
    let report = folder_run_after_the_runner_heard_the_grants(false).await;
    assert_eq!(report["ok"], false, "{report}");
    assert!(report["error"].as_str().unwrap().starts_with(FOLDER_NOT_GRANTED), "{report}");
}

#[test]
fn default_routed_run_timeout_matches_four_hour_lease_ceiling() {
    assert_eq!(DEFAULT_RUN_TIMEOUT, Duration::from_secs(14_400));
}
