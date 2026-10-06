mod common;

use std::fs;
use std::time::{Duration, SystemTime};

use common::TestRepo;
use git_carve::git::{GitError, ObjectFormat};

#[test]
fn failure_carries_stderr() {
    let repo = TestRepo::new();
    let err = repo
        .git()
        .run(["rev-parse", "--verify", "refs/heads/nope"])
        .unwrap_err();
    match err {
        GitError::Failed { stderr, .. } => assert!(stderr.starts_with("fatal:"), "{stderr}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn object_format_sha1() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    let head = repo.commit_all("a");
    let format = repo.git().object_format().unwrap();
    assert_eq!(format, ObjectFormat::Sha1);
    assert_eq!(head.len(), format.hex_len());
}

#[test]
fn object_format_sha256() {
    let repo = TestRepo::sha256();
    repo.write("a.txt", "a\n");
    let head = repo.commit_all("a");
    let format = repo.git().object_format().unwrap();
    assert_eq!(format, ObjectFormat::Sha256);
    assert_eq!(head.len(), format.hex_len());
    assert_eq!(format.null_oid().len(), head.len());
}

#[test]
fn exit_code_answers_map_to_bool() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    repo.commit_all("a");
    assert!(repo.git().test(["diff", "--cached", "--quiet"]).unwrap());

    repo.write("a.txt", "2\n");
    repo.git().run(["add", "a.txt"]).unwrap();
    assert!(!repo.git().test(["diff", "--cached", "--quiet"]).unwrap());

    let err = repo
        .git()
        .test(["diff", "--cached", "--bogus"])
        .unwrap_err();
    assert!(matches!(err, GitError::Failed { .. }), "{err:?}");
}

#[test]
fn stdin_round_trips_without_deadlock() {
    let repo = TestRepo::new();
    let oid = repo
        .git()
        .run_with_stdin(["hash-object", "-w", "--stdin"], b"hello\n")
        .unwrap();
    let oid = String::from_utf8(oid).unwrap().trim_end().to_owned();

    // Far more than a pipe buffer in both directions at once.
    let lines = 20_000;
    let input = format!("{oid}\n").repeat(lines);
    let out = repo
        .git()
        .run_with_stdin(["cat-file", "--batch-check"], input.as_bytes())
        .unwrap();
    let expected = format!("{oid} blob 6\n").repeat(lines);
    assert_eq!(String::from_utf8(out).unwrap(), expected);
}

/// carve writes the index once, at the very end (product rule 2), so read-only
/// commands must not refresh it as a side effect.
#[test]
fn safety_status_does_not_rewrite_index() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit_all("a");
    let index = repo.path().join(".git/index");

    // Stale stat data with unchanged content: exactly what `git status` likes to refresh.
    fs::File::options()
        .write(true)
        .open(repo.path().join("a.txt"))
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(60))
        .unwrap();

    let before = fs::read(&index).unwrap();
    repo.git().run(["status", "--porcelain=v2"]).unwrap();
    assert_eq!(
        fs::read(&index).unwrap(),
        before,
        "status rewrote the index"
    );

    // Control: with optional locks allowed, the same command does rewrite it, so the
    // assertion above can fail.
    repo.git()
        .clone()
        .env("GIT_OPTIONAL_LOCKS", "1")
        .run(["status", "--porcelain=v2"])
        .unwrap();
    assert_ne!(
        fs::read(&index).unwrap(),
        before,
        "control: status did not refresh the index"
    );
}
