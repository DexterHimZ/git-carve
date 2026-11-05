//! D50: the push gate. git's pre-push hook enforces; agent hooks explain and deny bypasses.

mod common;

use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

use common::{TestRepo, stderr};

/// A check that fails while any tracked .txt file says BROKEN.
const CHECK: &str = "! grep -l BROKEN *.txt";

/// A repository with a bare remote that has `main`, and the gate's check configured.
fn pushed_repo() -> (TestRepo, tempfile::TempDir) {
    let repo = TestRepo::new();
    repo.write("a.txt", "fine\n");
    repo.commit_all("base");
    let remote = tempfile::tempdir().unwrap();
    repo.run(&["init", "-q", "--bare", remote.path().to_str().unwrap()]);
    repo.run(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    repo.run(&["push", "-q", "-u", "origin", "main"]);
    repo.run(&["config", "carve.check", CHECK]);
    (repo, remote)
}

/// PATH with the built git-carve first, as an install would have it.
fn path_with_carve() -> String {
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_git-carve"))
        .parent()
        .unwrap()
        .to_owned();
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
}

fn git_push(repo: &TestRepo) -> Output {
    Command::new("git")
        .current_dir(repo.path())
        .env("PATH", path_with_carve())
        .envs(common::ISOLATION.iter().copied())
        .args(["push", "-q", "origin", "main"])
        .output()
        .unwrap()
}

fn hook_run(agent: &str, event: &serde_json::Value) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_git-carve"))
        .envs(common::ISOLATION.iter().copied())
        .args(["hooks", "run", agent])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn bash_event(repo: &TestRepo, command: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": repo.path(),
    })
}

#[test]
fn installed_pre_push_hook_refuses_a_failing_commit() {
    let (repo, _remote) = pushed_repo();
    let out = repo.carve().args(["hooks", "install"]).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    repo.write("b.txt", "BROKEN\n");
    repo.commit_all("wip");
    let out = git_push(&repo);
    assert!(!out.status.success(), "the push must be refused");
    // git sends a hook's output to its own stderr.
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out));
    assert!(text.contains("FAIL") && text.contains("wip"), "{text}");
    assert_eq!(
        repo.run(&["rev-parse", "origin/main"]),
        repo.run(&["rev-parse", "HEAD~1"]),
        "nothing was pushed"
    );

    repo.write("b.txt", "fixed\n");
    repo.commit_all("fix");
    // The fix is on top, but `wip` itself still fails: still refused.
    assert!(!git_push(&repo).status.success());
    repo.run(&["reset", "-q", "--soft", "HEAD~2"]);
    repo.commit_all("b, working");
    assert!(git_push(&repo).status.success(), "every commit passes now");
}

#[test]
fn install_never_replaces_a_foreign_hook_and_uninstall_removes_only_its_own() {
    let (repo, _remote) = pushed_repo();
    let hook = repo.path().join(".git/hooks/pre-push");
    fs::write(&hook, "#!/bin/sh\necho mine\n").unwrap();
    let out = repo.carve().args(["hooks", "install"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&hook).unwrap(), "#!/bin/sh\necho mine\n");
    let out = repo.carve().args(["hooks", "uninstall"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(hook.exists());

    fs::remove_file(&hook).unwrap();
    repo.carve().args(["hooks", "install"]).output().unwrap();
    let again = repo.carve().args(["hooks", "install"]).output().unwrap();
    assert!(String::from_utf8_lossy(&again.stdout).contains("Already installed"));
    repo.carve().args(["hooks", "uninstall"]).output().unwrap();
    assert!(!hook.exists());
}

#[test]
fn install_leaves_a_managed_hooks_path_alone() {
    let (repo, _remote) = pushed_repo();
    repo.run(&["config", "core.hooksPath", ".husky"]);
    let out = repo.carve().args(["hooks", "install"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("hooks print git"), "{}", stderr(&out));
    assert!(!repo.path().join(".husky").exists());
    assert!(!repo.path().join(".git/hooks/pre-push").exists());
}

#[test]
fn agent_hook_denies_a_push_whose_commit_fails_and_says_how_to_fix_it() {
    let (repo, _remote) = pushed_repo();
    repo.write("b.txt", "BROKEN\n");
    repo.commit_all("wip");
    let out = hook_run("claude", &bash_event(&repo, "npm test && git push"));
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let reason = stderr(&out);
    assert!(reason.contains("wip"), "{reason}");
    assert!(reason.contains("git carve @{u}"), "{reason}");
    assert!(reason.contains("--plan-file <file> --edit"), "{reason}");

    // Codex has the same contract; Cursor reads JSON.
    let out = hook_run("codex", &bash_event(&repo, "git push origin main"));
    assert_eq!(out.status.code(), Some(2));
    let cursor = serde_json::json!({"command": "git push", "cwd": repo.path()});
    let out = hook_run("cursor", &cursor);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["permission"], "deny");
    assert!(doc["agent_message"].as_str().unwrap().contains("wip"));
}

#[test]
fn agent_hook_allows_passing_pushes_and_other_commands() {
    let (repo, _remote) = pushed_repo();
    repo.write("b.txt", "fine too\n");
    repo.commit_all("good");
    let out = hook_run("claude", &bash_event(&repo, "git push"));
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    repo.write("c.txt", "BROKEN\n");
    repo.commit_all("bad");
    let out = hook_run("claude", &bash_event(&repo, "git log --oneline | head"));
    assert_eq!(out.status.code(), Some(0), "not a push");
}

#[test]
fn agent_hook_denies_bypasses_without_running_anything() {
    let dir = tempfile::tempdir().unwrap();
    let event =
        serde_json::json!({"tool_input": {"command": "git push --no-verify"}, "cwd": dir.path()});
    let out = hook_run("claude", &event);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("--no-verify"));
    let event = serde_json::json!({"tool_input": {"command": "git -c core.hooksPath=/dev/null push"}, "cwd": dir.path()});
    assert_eq!(hook_run("codex", &event).status.code(), Some(2));
}

#[test]
fn agent_hook_fails_open_outside_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let event = serde_json::json!({"tool_input": {"command": "git push"}, "cwd": dir.path()});
    let out = hook_run("claude", &event);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("push not checked"),
        "{}",
        stderr(&out)
    );
    let out = hook_run("claude", &serde_json::json!("not an event"));
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn printed_agent_snippets_are_valid_json() {
    for agent in ["claude", "cursor", "codex"] {
        let out = common::carve_in(std::path::Path::new("."))
            .args(["hooks", "print", agent])
            .output()
            .unwrap();
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout);
        let json: String = text.lines().filter(|l| !l.starts_with('#')).collect();
        let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            json.contains(&format!("git-carve hooks run {agent}")),
            "{doc}"
        );
        // Claude Code lets a timed-out hook through unless told otherwise; only its snippet
        // has the documented field.
        let hook = &doc["hooks"]["PreToolUse"][0]["hooks"][0];
        if agent == "claude" {
            // With `onFailure: block`, a machine without git-carve must not block every
            // command: the guarded command exits 0 when the binary is missing.
            let command = hook["command"].as_str().unwrap();
            let out = Command::new("sh")
                .args(["-c", command])
                .env("PATH", "/usr/bin:/bin")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(0), "{command}");
        }
        assert_eq!(
            hook.get("onFailure").and_then(|v| v.as_str()),
            (agent == "claude").then_some("block"),
            "{doc}"
        );
    }
}
