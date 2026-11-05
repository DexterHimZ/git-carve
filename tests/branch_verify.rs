//! `git carve verify` (DECISIONS.md D44): every commit in a range is checked, oldest first, in
//! the scratch worktree; failures name the commit that introduced each error; the working
//! tree, index, and refs are never touched.

mod common;

use std::process::Output;

use common::{TestRepo, stderr};

/// A compiler-like check: each OLDBAD or BROKEN line is a tsc-style error, and the check fails
/// if there is any.
const TSC_LIKE: &str = "out=$(grep -n -e OLDBAD -e BROKEN a.txt b.txt c.txt 2>/dev/null); \
    test -z \"$out\" || { echo \"$out\" | sed 's/^\\([^:]*\\):\\([0-9]*\\):\\(.*\\)/\\1(\\2,1): error TS9999: \\3/'; exit 1; }";

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", stderr(out)))
}

fn passed(doc: &serde_json::Value) -> Vec<bool> {
    doc["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["passed"].as_bool().unwrap())
        .collect()
}

/// base, then: a.txt changes (fine), b.txt breaks, c.txt changes (inherits b's error).
fn three_commits() -> (TestRepo, Vec<String>) {
    let repo = TestRepo::new();
    for f in ["a.txt", "b.txt", "c.txt"] {
        repo.write(f, "base\n");
    }
    repo.commit_all("base");
    let mut oids = Vec::new();
    for (f, text) in [
        ("a.txt", "alpha\n"),
        ("b.txt", "BROKEN\n"),
        ("c.txt", "charlie\n"),
    ] {
        repo.write(f, text);
        oids.push(repo.commit_all(&format!("change {f}")));
    }
    (repo, oids)
}

#[test]
fn every_commit_is_checked_and_the_culprit_is_named() {
    let (repo, oids) = three_commits();
    let out = repo
        .carve()
        .args(["verify", "HEAD~3..HEAD", "--json", "--check", TSC_LIKE])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "a failing commit exits 3");
    let doc = json(&out);
    assert_eq!(passed(&doc), vec![true, false, false]);
    let commits = doc["commits"].as_array().unwrap();
    assert_eq!(commits[1]["oid"], oids[1].as_str());
    assert_eq!(commits[1]["errors"][0], "b.txt(1,1): error TS9999: BROKEN");
    assert!(
        commits[1].get("inherits_from").is_none(),
        "it introduced the error"
    );
    assert_eq!(commits[2]["inherits_from"], oids[1].as_str());

    // The human report says the same thing.
    let out = repo
        .carve()
        .args(["verify", "HEAD~3..HEAD", "--check", TSC_LIKE])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("2 of 3 commits do not pass the check"),
        "{text}"
    );
    assert!(
        text.contains(&format!("same errors as {}", &oids[1][..9])),
        "{text}"
    );
}

#[test]
fn verify_never_touches_the_working_tree_index_or_refs() {
    let (repo, _) = three_commits();
    repo.write("a.txt", "dirty\n");
    repo.write("new.txt", "untracked\n");
    repo.run(&["add", "new.txt"]);
    let (state, files) = (repo.observable(), repo.worktree_digest());
    let out = repo
        .carve()
        .args(["verify", "HEAD~3..HEAD", "--check", TSC_LIKE])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert_eq!(repo.observable(), state, "status, index, HEAD unchanged");
    assert_eq!(
        repo.worktree_digest(),
        files,
        "bytes, modes, mtimes unchanged"
    );
}

#[test]
fn a_second_run_is_served_from_the_cache() {
    let (repo, _) = three_commits();
    let run = || {
        json(
            &repo
                .carve()
                .args(["verify", "HEAD~3..HEAD", "--json", "--check", TSC_LIKE])
                .output()
                .unwrap(),
        )
    };
    let first = run();
    assert!(first["runs"].as_u64().unwrap() >= 3);
    let second = run();
    assert_eq!(second["runs"], 0, "every tree was checked before");
    assert!(
        second["commits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["cached"] == true)
    );
    assert_eq!(passed(&second), passed(&first));
}

#[test]
fn pre_push_checks_only_what_is_being_pushed() {
    let (repo, oids) = three_commits();
    let zero = "0".repeat(oids[0].len());
    // Pushing main from oids[0] (already on the remote) to HEAD: two commits.
    let stdin = format!(
        "refs/heads/main {} refs/heads/main {}\nrefs/heads/gone {zero} refs/heads/gone {}\n",
        oids[2], oids[0], oids[0]
    );
    let mut child = repo
        .carve()
        .args(["verify", "--pre-push", "--json", "--check", TSC_LIKE])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let doc = json(&out);
    let checked: Vec<&str> = doc["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["oid"].as_str().unwrap())
        .collect();
    // Two commits are pushed (the deleted ref adds none); --pre-push fails fast, so the
    // failing first one ends the run.
    assert_eq!(checked, vec![oids[1].as_str()], "{doc}");
    assert_eq!(doc["skipped"], 1, "{doc}");
    assert_eq!(out.status.code(), Some(3), "the push is blocked");
}

/// The review of D48 (2026-10-14): a gate needs a verdict, not a report. `--fail-fast` stops
/// at the first failing commit and skips the D1 probe, which only guards a pass.
#[test]
fn fail_fast_stops_at_the_first_failing_commit() {
    let (repo, oids) = three_commits();
    let out = repo
        .carve()
        .args([
            "verify",
            "HEAD~3..HEAD",
            "--fail-fast",
            "--json",
            "--check",
            TSC_LIKE,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let doc = json(&out);
    assert_eq!(doc["version"], 1);
    assert_eq!(passed(&doc), vec![true, false]);
    assert_eq!(doc["commits"][1]["oid"], oids[1].as_str());
    assert_eq!(doc["skipped"], 1);

    let out = repo
        .carve()
        .args(["verify", "HEAD~3..HEAD", "--fail-fast", "--check", TSC_LIKE])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("git carve verify: 3 commits")
            && text.contains("Stopped there: 1 later commit was not checked.")
            && text.contains("hint: to re-split"),
        "{text}"
    );
}

#[test]
fn fail_fast_still_probes_a_passing_range() {
    let repo = TestRepo::new();
    repo.write("src/a.ts", "export const a = 1;\n");
    repo.commit_all("base");
    repo.write("src/a.ts", "export const a = 2;\n");
    repo.commit_all("change a");
    let doc = json(
        &repo
            .carve()
            .args([
                "verify",
                "HEAD~1..HEAD",
                "--fail-fast",
                "--json",
                "--check",
                "true",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(passed(&doc), vec![true]);
    assert_eq!(doc["skipped"], 0);
    assert_eq!(doc["unverified"], serde_json::json!(["src/a.ts"]));
}

#[test]
fn the_default_range_is_what_the_upstream_lacks() {
    let (repo, oids) = three_commits();
    // No remote at all: nothing safe to default to.
    let out = repo
        .carve()
        .args(["verify", "--check", TSC_LIKE])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("git carve verify main..HEAD"),
        "{}",
        stderr(&out)
    );

    // An upstream that has the first change: only the last two are checked.
    let remote = tempfile::tempdir().unwrap();
    let remote_path = remote.path().to_string_lossy().into_owned();
    repo.run(&["init", "-q", "--bare", &remote_path]);
    repo.run(&["remote", "add", "origin", &remote_path]);
    repo.run(&[
        "push",
        "-q",
        "origin",
        &format!("{}:refs/heads/main", oids[0]),
    ]);
    repo.run(&["branch", "-q", "--set-upstream-to=origin/main"]);
    let doc = json(
        &repo
            .carve()
            .args(["verify", "--json", "--check", TSC_LIKE])
            .output()
            .unwrap(),
    );
    assert_eq!(doc["commits"].as_array().unwrap().len(), 2);
    assert_eq!(doc["commits"][0]["oid"], oids[1].as_str());
}

#[test]
fn no_new_errors_measures_each_commit_against_the_base() {
    let repo = TestRepo::new();
    repo.write("a.txt", "base\n");
    repo.write("b.txt", "base\n");
    repo.write("c.txt", "OLDBAD\n");
    repo.commit_all("base with an old error");
    repo.write("a.txt", "alpha\n");
    let fine = repo.commit_all("adds no new error");
    repo.write("b.txt", "BROKEN\n");
    let broken = repo.commit_all("adds a new error");

    let plain = repo
        .carve()
        .args(["verify", "HEAD~2..HEAD", "--check", TSC_LIKE])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&plain.stdout);
    assert!(text.contains("2 of 2 commits do not pass"), "{text}");
    assert!(
        text.contains("already fails with 1 error"),
        "the hint names the base: {text}"
    );

    let doc = json(
        &repo
            .carve()
            .args([
                "verify",
                "HEAD~2..HEAD",
                "--json",
                "--no-new-errors",
                "--check",
                TSC_LIKE,
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(doc["baseline_errors"], 1);
    assert_eq!(doc["commits"][0]["oid"], fine.as_str());
    assert_eq!(passed(&doc), vec![true, false]);
    assert_eq!(doc["commits"][1]["oid"], broken.as_str());
    assert_eq!(
        doc["commits"][1]["errors"],
        serde_json::json!(["b.txt(1,1): error TS9999: BROKEN"]),
        "only the new error is quoted"
    );
}

#[test]
fn a_check_that_reads_nothing_is_reported() {
    let repo = TestRepo::new();
    repo.write("src/a.ts", "export const a = 1;\n");
    repo.commit_all("base");
    repo.write("src/a.ts", "export const a = 2;\n");
    repo.commit_all("change a");
    let doc = json(
        &repo
            .carve()
            .args(["verify", "HEAD~1..HEAD", "--json", "--check", "true"])
            .output()
            .unwrap(),
    );
    assert_eq!(passed(&doc), vec![true]);
    assert_eq!(
        doc["unverified"],
        serde_json::json!(["src/a.ts"]),
        "the probe (D1) shows `true` never reads the file"
    );
}

#[test]
fn demo_commits_are_verified_with_real_tsc() {
    let repo = TestRepo::demo();
    if !repo.link_typescript() {
        return;
    }
    // Commit the demo's mess as rename, then a feature with a typo, then the fix.
    let rename = [
        "src/format.ts",
        "src/main.ts",
        "src/report.ts",
        "src/table.ts",
        "src/progress.ts",
        "src/summary.ts",
    ];
    repo.run(&[&["add", "--"][..], &rename[..]].concat());
    repo.run(&["commit", "-q", "-m", "rename"]);
    let net = std::fs::read_to_string(repo.path().join("src/net.ts")).unwrap();
    repo.write("src/net.ts", net.replace("throw failure", "throw failur"));
    repo.run(&["add", "src/net.ts", "tests/net.test.ts"]);
    repo.run(&["commit", "-q", "-m", "feature with a typo"]);
    repo.run(&["add", "src/parse.ts"]);
    repo.run(&["commit", "-q", "-m", "fix"]);
    let out = repo
        .carve()
        .args(["verify", "HEAD~3..HEAD", "--json"])
        .output()
        .unwrap();
    let doc = json(&out);
    assert_eq!(doc["check"], "tsc --noEmit -p .");
    assert_eq!(passed(&doc), vec![true, false, false]);
    assert!(
        doc["commits"][1]["errors"][0]
            .as_str()
            .unwrap()
            .contains("TS2552")
    );
    assert!(doc["commits"][2]["inherits_from"].is_string());
}

#[test]
fn one_failing_commit_reads_singular_and_suggests_a_re_carve() {
    let repo = TestRepo::new();
    repo.write("a.txt", "ok\n");
    repo.write("b.txt", "ok\n");
    repo.write("c.txt", "ok\n");
    repo.commit_all("base");
    repo.write("a.txt", "fine\n");
    repo.commit_all("good one");
    repo.write("b.txt", "BROKEN\n");
    repo.commit_all("bad one");
    let out = repo
        .carve()
        .args(["verify", "HEAD~2..HEAD", "--check", TSC_LIKE])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("1 of 2 commits does not pass the check."),
        "{text}"
    );
    assert!(text.contains("git carve @{u}"), "{text}");
}

/// D50 review: git exports GIT_DIR to a pre-push hook run in a linked worktree. The
/// scratch worktree's `read-tree -u` must not follow it into the user's index.
#[test]
fn safety_verify_ignores_an_inherited_git_dir() {
    let (repo, _) = three_commits();
    let wt = tempfile::tempdir().unwrap();
    let wt_path = wt.path().join("wt");
    repo.run(&["worktree", "add", "-q", wt_path.to_str().unwrap(), "HEAD"]);
    let gitdir = repo.run(&["rev-parse", "--absolute-git-dir"]);
    let linked = format!("{}/worktrees/wt", gitdir.trim());
    let out = common::carve_in(&wt_path)
        .env("GIT_DIR", &linked)
        .args(["verify", "HEAD~3..HEAD~2", "--check", "true"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let status = std::process::Command::new("git")
        .current_dir(&wt_path)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        "",
        "the worktree's index is untouched"
    );
}

/// D50 review: a pushed-over remote commit this repository never fetched falls back
/// to "what no remote has", instead of failing the push on a git error.
#[test]
fn pre_push_with_an_unknown_remote_commit_still_checks() {
    let (repo, oids) = three_commits();
    let unknown = "e".repeat(oids[0].len());
    let stdin = format!("refs/heads/main {} refs/heads/main {unknown}\n", oids[2]);
    let mut child = repo
        .carve()
        .args(["verify", "--pre-push", "--json", "--check", TSC_LIKE])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "checked, and blocked: {}",
        stderr(&out)
    );
    assert!(!json(&out)["commits"].as_array().unwrap().is_empty());
}

/// A check without tsc-style diagnostics (cargo, go, `make test`): its failures carry no
/// parsed errors. Under `--no-new-errors` they must still come from the cache on a second run.
#[test]
fn no_new_errors_keeps_cached_failures_of_checks_without_parsed_errors() {
    let (repo, _) = three_commits();
    let plain = "! grep -q BROKEN a.txt b.txt c.txt";
    let run = || {
        json(
            &repo
                .carve()
                .args([
                    "verify",
                    "HEAD~3..HEAD",
                    "--json",
                    "--no-new-errors",
                    "--check",
                    plain,
                ])
                .output()
                .unwrap(),
        )
    };
    let first = run();
    assert!(first["runs"].as_u64().unwrap() >= 3, "{first}");
    let second = run();
    assert_eq!(
        second["runs"], 0,
        "failures without parsed errors stay cached: {second}"
    );
    assert_eq!(passed(&second), passed(&first));
}
