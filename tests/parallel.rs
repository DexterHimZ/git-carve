//! D51: checks run in parallel worker worktrees give exactly the serial results, never touch
//! the working tree, and `--clean` removes the workers.

mod common;

use std::process::Output;

use common::{TestRepo, stderr};

/// A compiler-like check: each BROKEN line is a tsc-style error.
const TSC_LIKE: &str = "out=$(grep -n BROKEN a.txt b.txt c.txt d.txt 2>/dev/null); \
    test -z \"$out\" || { echo \"$out\" | sed 's/^\\([^:]*\\):\\([0-9]*\\):\\(.*\\)/\\1(\\2,1): error TS9999: \\3/'; exit 1; }";

/// base, then: a.txt changes (fine), b.txt breaks, c.txt changes, d.txt changes.
fn four_commits() -> TestRepo {
    let repo = TestRepo::new();
    for f in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        repo.write(f, "base\n");
    }
    repo.commit_all("base");
    for (f, text) in [
        ("a.txt", "alpha\n"),
        ("b.txt", "BROKEN\n"),
        ("c.txt", "charlie\n"),
        ("d.txt", "delta\n"),
    ] {
        repo.write(f, text);
        repo.commit_all(&format!("change {f}"));
    }
    repo
}

fn verify(repo: &TestRepo, jobs: &str, extra: &[&str]) -> (Output, serde_json::Value) {
    let out = repo
        .carve()
        .env("GIT_CARVE_DEV_CHECK_JOBS", jobs)
        .args(["verify", "HEAD~4..HEAD", "--json", "--check", TSC_LIKE])
        .args(extra)
        .output()
        .unwrap();
    let doc =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", stderr(&out)));
    (out, doc)
}

/// What a report says, without its timings and cache counters.
fn verdicts(doc: &serde_json::Value) -> Vec<serde_json::Value> {
    doc["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| serde_json::json!([c["oid"], c["passed"], c["errors"], c.get("inherits_from")]))
        .collect()
}

#[test]
fn parallel_verify_reports_exactly_what_serial_does() {
    let repo = four_commits();
    let (_, serial) = verify(&repo, "1", &[]);
    repo.carve().arg("--clean").output().unwrap();
    let (out, parallel) = verify(&repo, "3", &[]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(parallel["runs"].as_u64().unwrap() >= 4, "{parallel}");
    assert_eq!(verdicts(&parallel), verdicts(&serial));
    let carve = repo.run(&["rev-parse", "--git-path", "carve"]);
    let carve = repo.path().join(carve.trim());
    assert!(carve.join("w1/scratch").is_dir() && carve.join("w2/scratch").is_dir());
}

#[test]
fn parallel_fail_fast_reports_up_to_the_first_failure() {
    let repo = four_commits();
    let (out, doc) = verify(&repo, "3", &["--fail-fast"]);
    assert_eq!(out.status.code(), Some(3));
    let passed: Vec<bool> = doc["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["passed"].as_bool().unwrap())
        .collect();
    assert_eq!(passed, [true, false]);
    assert_eq!(doc["skipped"], 2);
}

#[test]
fn safety_parallel_checks_never_touch_the_working_tree_index_or_refs() {
    let repo = four_commits();
    repo.write("a.txt", "dirty\n");
    repo.write("new.txt", "untracked\n");
    repo.run(&["add", "new.txt"]);
    let (state, files) = (repo.observable(), repo.worktree_digest());
    let (out, _) = verify(&repo, "3", &[]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert_eq!(repo.observable(), state, "status, index, HEAD unchanged");
    assert_eq!(
        repo.worktree_digest(),
        files,
        "bytes, modes, mtimes unchanged"
    );
}

#[test]
fn clean_removes_the_parallel_worktrees() {
    let repo = four_commits();
    verify(&repo, "3", &[]);
    let listed = |repo: &TestRepo| {
        repo.run(&["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
    };
    assert_eq!(listed(&repo), 4, "the repository, scratch, w1 and w2");
    let out = repo.carve().arg("--clean").output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("2 parallel check worktree(s)"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(listed(&repo), 1);
}

#[test]
fn check_jobs_config_turns_parallelism_on_for_an_explicit_check() {
    let repo = four_commits();
    repo.run(&["config", "carve.checkJobs", "3"]);
    let out = repo
        .carve()
        .args(["verify", "HEAD~4..HEAD", "--check", TSC_LIKE])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let carve = repo.run(&["rev-parse", "--git-path", "carve"]);
    assert!(repo.path().join(carve.trim()).join("w2/scratch").is_dir());
}

/// The carve itself: the demo planned with real tsc at 1 and 3 jobs, from a cold state each
/// time, gives the same buffer byte for byte (non-negotiable 7).
#[test]
fn parallel_carve_plans_exactly_like_serial_with_real_tsc() {
    let repo = TestRepo::demo();
    if !repo.link_typescript() {
        return;
    }
    let plan = |jobs: &str| {
        repo.carve().arg("--clean").output().unwrap();
        let out = repo
            .carve()
            .env("GIT_CARVE_DEV_CHECK_JOBS", jobs)
            .arg("--plan-only")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        // The last line is the run counter, "(N check runs, M cached, T s)".
        text.lines()
            .filter(|l| !l.starts_with('(') || !l.contains("check runs"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(plan("3"), plan("1"));
}
