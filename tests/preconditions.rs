//! Refusal on each SPEC §5.1 precondition: exit 2, a clear reason, and nothing changed.

mod common;

use common::{TestRepo, carve_in, stderr};

/// Runs `git carve --plan-only`, expects exit 2 with `reason` in stderr, and checks that
/// HEAD and the worktree are unchanged.
fn refuses(repo: &TestRepo, reason: &str) {
    let head = repo
        .git()
        .run_str(["rev-parse", "-q", "--verify", "HEAD"])
        .ok();
    let before = repo.worktree_digest();
    let out = repo.carve().arg("--plan-only").output().unwrap();
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains(reason),
        "expected {reason:?} in: {}",
        stderr(&out)
    );
    assert_eq!(
        repo.git()
            .run_str(["rev-parse", "-q", "--verify", "HEAD"])
            .ok(),
        head
    );
    assert_eq!(repo.worktree_digest(), before);
}

fn based() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("f.txt", "base\n");
    repo.commit_all("base");
    repo
}

/// Two branches that edit the same line: `main` at "ours", `theirs` at "theirs".
fn diverged() -> TestRepo {
    let repo = based();
    repo.run(&["checkout", "-q", "-b", "theirs"]);
    repo.write("f.txt", "theirs\n");
    repo.commit_all("theirs");
    repo.run(&["checkout", "-q", "main"]);
    repo.write("f.txt", "ours\n");
    repo.commit_all("ours");
    repo
}

#[test]
fn safety_refuses_outside_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let out = carve_in(dir.path())
        .env("GIT_CEILING_DIRECTORIES", dir.path().parent().unwrap())
        .arg("--plan-only")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("not inside a git repository"));
}

#[test]
fn safety_refuses_a_bare_repository() {
    let dir = tempfile::tempdir().unwrap();
    common::isolated(git_carve::git::Git::new(dir.path()))
        .run(["init", "-q", "--bare"])
        .unwrap();
    let out = carve_in(dir.path()).arg("--plan-only").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("not inside a work tree"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn safety_refuses_an_unborn_head() {
    let repo = TestRepo::new();
    repo.write("f.txt", "x\n");
    refuses(&repo, "no commits yet");
}

#[test]
fn safety_refuses_a_merge_in_progress() {
    let repo = diverged();
    let _ = repo.git().run(["merge", "theirs"]);
    refuses(&repo, "merge is in progress");
}

#[test]
fn safety_refuses_a_cherry_pick_in_progress() {
    let repo = diverged();
    let _ = repo.git().run(["cherry-pick", "theirs"]);
    refuses(&repo, "cherry-pick is in progress");
}

#[test]
fn safety_refuses_a_revert_in_progress() {
    let repo = based();
    repo.write("f.txt", "second\n");
    repo.commit_all("second");
    repo.write("f.txt", "third\n");
    repo.commit_all("third");
    let _ = repo.git().run(["revert", "--no-edit", "HEAD~1"]);
    refuses(&repo, "revert is in progress");
}

#[test]
fn safety_refuses_a_rebase_in_progress() {
    let repo = diverged();
    let _ = repo.git().run(["rebase", "theirs"]);
    refuses(&repo, "rebase is in progress");
}

#[test]
fn safety_refuses_a_bisect_in_progress() {
    let repo = based();
    repo.run(&["bisect", "start"]);
    repo.write("f.txt", "dirty\n");
    refuses(&repo, "bisect is in progress");
}

#[test]
fn safety_refuses_staged_changes() {
    let repo = based();
    repo.write("f.txt", "staged\n");
    repo.run(&["add", "f.txt"]);
    refuses(&repo, "staged changes");
}

#[test]
fn safety_refuses_intent_to_add() {
    let repo = based();
    repo.write("new.txt", "n\n");
    repo.run(&["add", "-N", "new.txt"]);
    refuses(&repo, "intent-to-add");
}

#[test]
fn safety_refuses_unmerged_entries_without_a_merge() {
    let repo = based();
    repo.write("f.txt", "stashed\n");
    repo.run(&["stash"]);
    repo.write("f.txt", "committed\n");
    repo.commit_all("conflicting");
    let _ = repo.git().run(["stash", "apply"]);
    refuses(&repo, "unmerged");
}

#[test]
fn safety_refuses_a_changed_submodule() {
    let sub = TestRepo::new();
    sub.write("s.txt", "s\n");
    sub.commit_all("sub base");
    let repo = based();
    let url = sub.path().to_str().unwrap();
    repo.run(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        url,
        "sub",
    ]);
    repo.commit_all("add submodule");
    let inner = repo.git().with_dir(repo.path().join("sub"));
    inner
        .run(["commit", "-q", "--allow-empty", "-m", "move"])
        .unwrap();
    refuses(&repo, "submodule sub changed");
}

#[test]
fn safety_refuses_a_nested_repository() {
    let repo = based();
    let nested = repo.git().with_dir(repo.path());
    nested.run(["init", "-q", "nested"]).unwrap();
    repo.write("nested/n.txt", "n\n");
    refuses(&repo, "nested git repository");
}

#[test]
fn safety_refuses_a_sparse_checkout() {
    let repo = based();
    repo.run(&["config", "core.sparseCheckout", "true"]);
    repo.write("f.txt", "changed\n");
    refuses(&repo, "sparse");
}

#[test]
fn safety_refuses_nothing_to_carve() {
    refuses(&based(), "nothing to carve");
}

#[test]
fn safety_refuses_too_many_paths() {
    let repo = based();
    for i in 0..201 {
        repo.write(&format!("many/{i}.txt"), "x\n");
    }
    refuses(&repo, "more than 200");
}

#[test]
fn safety_refuses_too_many_atoms() {
    let repo = TestRepo::new();
    repo.write("f.txt", "1\n2\n3\n4\n5\n");
    repo.commit_all("base");
    repo.run(&["config", "carve.maxAtoms", "2"]);
    repo.write("f.txt", "A\n2\nC\n4\nE\n");
    refuses(&repo, "carve.maxAtoms");
}

#[test]
fn safety_refuses_while_another_carve_runs() {
    let repo = based();
    repo.write("f.txt", "changed\n");
    std::fs::create_dir_all(repo.path().join(".git/carve")).unwrap();
    std::fs::write(repo.path().join(".git/carve/lock"), "12345\n").unwrap();
    refuses(&repo, "another git carve is running");
}
