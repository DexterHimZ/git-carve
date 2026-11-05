//! Re-carving committed work: `git carve <base>` (D46 phase 1, docs/design/carve-range.md).

mod common;

use std::collections::BTreeSet;

use common::{TYPE_MESSAGES, TestRepo, editor, per_file, stderr};
use git_carve::Error;
use git_carve::atom::{AtomId, AtomKind, Whole, atomize};
use git_carve::commit::{Group, Hooks, commit};
use git_carve::precondition::{ResolvedBase, resolve_base};
use git_carve::snapshot::{self, FileSnap, Snapshot};
use proptest::prelude::*;

/// Plain `git carve` builds on HEAD: the snapshot's base is HEAD itself.
#[test]
fn snapshot_plain_base_equals_head() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.commit_all("base");
    repo.write("a.txt", "two\n");
    let snap = snapshot::take(&repo.open()).unwrap();
    assert_eq!(snap.base.commit, snap.head.commit);
    assert_eq!(snap.base.tree, snap.head.tree);
    assert_eq!(snap.base.tree, repo.head_tree());
}

fn since(repo: &TestRepo, base: &str) -> Result<Snapshot, Error> {
    snapshot::take_since(&repo.open(), Some(base))
}

fn paths(snap: &Snapshot) -> Vec<String> {
    snap.files
        .iter()
        .map(|f| String::from_utf8_lossy(&f.path).into_owned())
        .collect()
}

fn file<'a>(snap: &'a Snapshot, path: &str) -> &'a FileSnap {
    snap.files
        .iter()
        .find(|f| f.path == path.as_bytes())
        .unwrap_or_else(|| panic!("{path} not in the snapshot"))
}

#[test]
fn range_unions_committed_and_dirty_paths() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    let base = repo.commit_all("base");
    repo.write("a.txt", "a2\n");
    repo.commit_all("wip");
    repo.write("b.txt", "b2\n");
    repo.write("c.txt", "c\n");
    let snap = since(&repo, &base).unwrap();
    assert_eq!(paths(&snap), ["a.txt", "b.txt", "c.txt"]);
    assert_eq!(snap.base.commit, base);
    assert_ne!(snap.head.commit, base);
    assert_eq!(
        file(&snap, "a.txt").base.as_ref().unwrap().oid,
        repo.run(&["rev-parse", &format!("{base}:a.txt")]).trim()
    );
    assert_eq!(
        file(&snap, "a.txt").new.as_ref().unwrap().oid,
        repo.run(&["rev-parse", "HEAD:a.txt"]).trim()
    );
    assert!(file(&snap, "c.txt").base.is_none());
    assert!(!file(&snap, "c.txt").in_head);
}

#[test]
fn range_dirty_file_also_changed_in_range_has_base_from_b() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    let base = repo.commit_all("base");
    repo.write("a.txt", "two\n");
    repo.commit_all("wip");
    repo.write("a.txt", "three\n");
    let snap = since(&repo, &base).unwrap();
    let f = file(&snap, "a.txt");
    assert_eq!(
        f.base.as_ref().unwrap().oid,
        repo.run(&["rev-parse", &format!("{base}:a.txt")]).trim()
    );
    assert_eq!(
        f.new.as_ref().unwrap().oid,
        repo.run(&["hash-object", "a.txt"]).trim()
    );
}

#[test]
fn range_added_then_deleted_file_is_absent() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    let base = repo.commit_all("base");
    repo.write("tmp.txt", "scratch\n");
    repo.write("a.txt", "a2\n");
    repo.commit_all("add tmp");
    repo.remove("tmp.txt");
    repo.commit_all("drop tmp");
    let snap = since(&repo, &base).unwrap();
    assert_eq!(paths(&snap), ["a.txt"]);
}

#[test]
fn range_mode_only_change_is_a_mode_atom() {
    let repo = TestRepo::new();
    repo.write("run.sh", "#!/bin/sh\necho hi\n");
    let base = repo.commit_all("base");
    repo.set_exec("run.sh", true);
    repo.commit_all("exec");
    let snap = since(&repo, &base).unwrap();
    let changes = atomize(&repo.open(), &snap).unwrap();
    assert!(
        changes
            .atoms
            .iter()
            .any(|a| matches!(a.kind, AtomKind::Mode { .. })),
        "{:?}",
        changes.atoms
    );
}

#[test]
fn range_symlink_in_range_is_whole_atom() {
    let repo = TestRepo::new();
    repo.write("x.txt", "x\n");
    repo.write("y.txt", "y\n");
    repo.symlink("x.txt", "link");
    let base = repo.commit_all("base");
    repo.remove("link");
    repo.symlink("y.txt", "link");
    repo.commit_all("retarget");
    let snap = since(&repo, &base).unwrap();
    let changes = atomize(&repo.open(), &snap).unwrap();
    assert!(
        changes
            .atoms
            .iter()
            .any(|a| matches!(a.kind, AtomKind::Whole(Whole::Symlink)))
    );
}

#[test]
fn range_refuses_submodule_added_in_range() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    let base = repo.commit_all("base");
    repo.run(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{base},sub"),
    ]);
    repo.run(&["commit", "-q", "-m", "add a submodule"]);
    let err = since(&repo, &base).unwrap_err();
    assert!(
        matches!(&err, Error::Precondition(m) if m.contains("submodule")),
        "{err:?}"
    );
}

#[test]
fn range_net_empty_is_refused() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    let base = repo.commit_all("base");
    repo.write("a.txt", "b\n");
    repo.commit_all("change");
    repo.write("a.txt", "a\n");
    repo.commit_all("change back");
    let err = since(&repo, &base).unwrap_err();
    assert!(
        matches!(&err, Error::Precondition(m) if m.contains("changes nothing net")),
        "{err:?}"
    );
}

#[test]
fn range_committed_large_binary_is_not_left_by_default() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    let base = repo.commit_all("base");
    repo.write("logo.bin", b"\x00\x01\x02 binary asset\x00");
    repo.commit_all("add an asset");
    let snap = since(&repo, &base).unwrap();
    assert!(file(&snap, "logo.bin").in_head);
    let changes = atomize(&repo.open(), &snap).unwrap();
    let asset = changes
        .atoms
        .iter()
        .find(|a| changes.files[a.file].path == b"logo.bin")
        .unwrap();
    assert!(
        !asset.leave_by_default,
        "a committed asset must stay in the history"
    );

    // The same file untracked is still left out by default.
    let plain = TestRepo::new();
    plain.write("a.txt", "a\n");
    plain.commit_all("base");
    plain.write("logo.bin", b"\x00\x01\x02 binary asset\x00");
    let changes = plain.changes();
    assert!(changes.atoms.iter().all(|a| a.leave_by_default));
}

#[test]
fn range_deleted_then_untracked_recreation() {
    let repo = TestRepo::new();
    repo.write("a.txt", "old\n");
    repo.write("b.txt", "b\n");
    let base = repo.commit_all("base");
    repo.remove("a.txt");
    repo.commit_all("delete a");
    repo.write("a.txt", "new\n");
    let snap = since(&repo, &base).unwrap();
    let f = file(&snap, "a.txt");
    assert!(f.base.is_some());
    assert!(!f.in_head);
    assert_eq!(
        f.new.as_ref().unwrap().oid,
        repo.run(&["hash-object", "a.txt"]).trim()
    );
}

fn resolve(repo: &TestRepo, expr: &str, rewrite_pushed: bool) -> Result<ResolvedBase, Error> {
    let before = repo.run(&["rev-parse", "HEAD"]);
    let out = resolve_base(&repo.open(), expr, rewrite_pushed);
    assert_eq!(
        repo.run(&["rev-parse", "HEAD"]),
        before,
        "resolving moved HEAD"
    );
    out
}

fn refused(out: Result<ResolvedBase, Error>, needle: &str) {
    match out {
        Err(Error::Precondition(m)) => assert!(m.contains(needle), "{m}"),
        other => panic!("expected a refusal mentioning {needle:?}, got {other:?}"),
    }
}

/// base, then two commits on top.
fn two_on_top() -> (TestRepo, String) {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    let base = repo.commit_all("base");
    repo.write("a.txt", "a2\n");
    repo.commit_all("wip one");
    repo.write("b.txt", "b\n");
    repo.commit_all("wip two");
    (repo, base)
}

#[test]
fn range_resolves_commits_oldest_first() {
    let (repo, base) = two_on_top();
    let r = resolve(&repo, &base, false).unwrap();
    assert_eq!(r.oid, base);
    assert!(!r.merge_based);
    let subjects: Vec<&str> = r.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["wip one", "wip two"]);
    for expr in [
        format!("{base}.."),
        format!("{base}..HEAD"),
        "HEAD~2".into(),
    ] {
        assert_eq!(resolve(&repo, &expr, false).unwrap().oid, base, "{expr}");
    }
    assert!(resolve(&repo, "HEAD", false).unwrap().commits.is_empty());
}

#[test]
fn range_refuses_unknown_base() {
    let (repo, _) = two_on_top();
    refused(resolve(&repo, "no-such-rev", false), "unknown revision");
}

#[test]
fn range_refuses_tip_other_than_head() {
    let (repo, base) = two_on_top();
    repo.run(&["branch", "other", "HEAD~1"]);
    refused(
        resolve(&repo, &format!("{base}..other"), false),
        "current branch only",
    );
}

#[test]
fn range_refuses_merge_in_range() {
    let (repo, base) = two_on_top();
    repo.run(&["checkout", "-q", "-b", "side", &base]);
    repo.write("c.txt", "c\n");
    repo.commit_all("side work");
    repo.run(&["checkout", "-q", "main"]);
    repo.run(&["merge", "-q", "--no-edit", "side"]);
    refused(resolve(&repo, &base, false), "merge commit");
}

#[test]
fn range_uses_merge_base_when_base_diverged() {
    let (repo, base) = two_on_top();
    repo.run(&["branch", "-q", "feature"]);
    repo.run(&["reset", "-q", "--hard", &base]);
    repo.write("upstream.txt", "u\n");
    repo.commit_all("main moved on");
    repo.run(&["checkout", "-q", "feature"]);
    let r = resolve(&repo, "main", false).unwrap();
    assert!(r.merge_based);
    assert_eq!(r.oid, base);
    assert_eq!(r.commits.len(), 2);
}

#[test]
fn range_refuses_pushed_commits_without_flag() {
    let (repo, base) = two_on_top();
    let remote = tempfile::tempdir().unwrap();
    repo.run(&["init", "-q", "--bare", remote.path().to_str().unwrap()]);
    repo.run(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    repo.run(&["push", "-q", "origin", "main"]);
    refused(resolve(&repo, &base, false), "origin/main");
    let r = resolve(&repo, &base, true).unwrap();
    assert_eq!(r.commits.len(), 2);
    // Only what is not pushed yet needs no flag.
    repo.write("c.txt", "c\n");
    repo.commit_all("local only");
    assert_eq!(
        resolve(&repo, "origin/main", false).unwrap().commits.len(),
        1
    );
}

// Whole runs (step 4): the CLI and the library, on committed work.

/// The SPEC §14.1 demo mess committed as three wip commits; returns (repo, base).
fn wip_history() -> (TestRepo, String) {
    let repo = TestRepo::demo();
    let base = repo.run(&["rev-parse", "HEAD"]);
    repo.run(&["add", "src/format.ts", "src/main.ts", "src/progress.ts"]);
    repo.run(&["commit", "-q", "-m", "wip"]);
    repo.run(&["add", "src/net.ts", "tests/net.test.ts", "src/parse.ts"]);
    repo.run(&["commit", "-q", "-m", "asdf"]);
    repo.run(&["add", "-A"]);
    repo.run(&["commit", "-q", "-m", "fix stuff"]);
    assert_eq!(repo.run(&["status", "--porcelain"]), "");
    (repo, base)
}

fn carve_range(repo: &TestRepo, args: &[&str]) -> std::process::Output {
    let (_dir, ed) = editor(TYPE_MESSAGES);
    repo.carve()
        .env("GIT_EDITOR", &ed)
        .args(args)
        .output()
        .unwrap()
}

fn out_str(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn range_three_wip_commits_become_carved_commits() {
    let (repo, base) = wip_history();
    let old_tree = repo.head_tree();
    let digest = repo.worktree_digest();
    let out = carve_range(&repo, &["HEAD~3", "--no-check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = out_str(&out);
    assert!(
        stdout.starts_with(&format!(
            "Re-carved 3 commits since {} into 3.\nCreated 3 commits on main.",
            &base[..7]
        )),
        "{stdout}"
    );
    assert_eq!(repo.run(&["rev-parse", "HEAD~3"]), base);
    // The agent's committed debug lines left history and stayed in the working tree.
    assert_eq!(
        repo.run(&["status", "--porcelain"]),
        " M src/main.ts\n M src/net.ts"
    );
    // Byte-identical: nothing on disk changed, and the tree a `git add -A` would commit is
    // the old tip's tree.
    assert_eq!(repo.worktree_digest(), digest);
    assert_eq!(repo.oracle_tree(), old_tree);
    let reflog = repo.run(&["reflog", "-1", "--format=%gs", "main"]);
    assert!(
        reflog.starts_with(&format!("carve: re-carved since {}", &base[..7])),
        "{reflog}"
    );
}

#[test]
fn range_commits_each_pass_tsc() {
    let (repo, _) = wip_history();
    if !repo.link_typescript() {
        eprintln!("skipped: no TypeScript compiler");
        return;
    }
    let out = carve_range(&repo, &["HEAD~3"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        out_str(&out).contains("Created 3 commits on main, each passed `tsc"),
        "{}",
        out_str(&out)
    );
}

#[test]
fn range_undo_restores_old_tip_index_and_status() {
    let (repo, _) = wip_history();
    let before = repo.observable();
    let tip = repo.run(&["rev-parse", "HEAD"]);
    let out = carve_range(&repo, &["HEAD~3", "--no-check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = repo.carve().arg("--undo").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.run(&["rev-parse", "HEAD"]), tip);
    assert_eq!(repo.observable(), before);
}

#[test]
fn range_plus_dirty_changes() {
    let repo = TestRepo::demo();
    let base = repo.run(&["rev-parse", "HEAD"]);
    repo.run(&["add", "src/format.ts", "src/net.ts"]);
    repo.run(&["commit", "-q", "-m", "wip"]);
    let dirty_before = repo.run(&["status", "--porcelain"]);
    assert!(!dirty_before.is_empty());
    let out = carve_range(&repo, &["HEAD~1", "--no-check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        out_str(&out).starts_with(&format!("Re-carved 1 commit since {}", &base[..7])),
        "{}",
        out_str(&out)
    );
    assert_eq!(repo.run(&["rev-parse", "HEAD~3"]), base);
    assert_eq!(
        repo.run(&["status", "--porcelain"]),
        " M src/main.ts\n M src/net.ts"
    );
}

#[test]
fn range_deleted_file_in_range() {
    let repo = TestRepo::new();
    repo.write("keep.txt", "keep\n");
    repo.write("gone.txt", "bye\n");
    let base = repo.commit_all("base");
    repo.remove("gone.txt");
    repo.write("keep.txt", "keep 2\n");
    repo.commit_all("wip");
    let out = carve_range(&repo, &[&base, "--no-check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.run(&["ls-tree", "--name-only", "HEAD"]), "keep.txt");
    assert_eq!(repo.run(&["status", "--porcelain"]), "");
}

#[test]
fn range_rename_across_range_is_bound() {
    let repo = TestRepo::new();
    let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
    repo.write("old_name.txt", &body);
    repo.write("other.txt", "x\n");
    let base = repo.commit_all("base");
    repo.run(&["mv", "old_name.txt", "new_name.txt"]);
    repo.write("new_name.txt", format!("{body}one more\n"));
    repo.commit_all("rename");
    repo.write("other.txt", "y\n");
    repo.commit_all("unrelated");
    let snap = since(&repo, &base).unwrap();
    let changes = atomize(&repo.open(), &snap).unwrap();
    assert!(
        changes
            .binds
            .iter()
            .any(|b| matches!(b.reason, git_carve::atom::BindReason::Rename)),
        "{:?}",
        changes.binds
    );
}

#[test]
fn range_detached_head() {
    let (repo, base) = wip_history();
    repo.run(&["checkout", "-q", "--detach"]);
    let out = carve_range(&repo, &["HEAD~3", "--no-check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        out_str(&out).contains("on detached HEAD"),
        "{}",
        out_str(&out)
    );
    assert_eq!(repo.run(&["rev-parse", "HEAD~3"]), base);
}

#[test]
fn range_merge_base_notice_is_printed() {
    let (repo, base) = wip_history();
    repo.run(&["branch", "-q", "feature"]);
    repo.run(&["reset", "-q", "--hard", &base]);
    repo.write("upstream.txt", "u\n");
    repo.commit_all("main moved on");
    repo.run(&["checkout", "-q", "feature"]);
    let out = carve_range(&repo, &["main", "--plan-only"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("main is not an ancestor of HEAD"),
        "{}",
        stderr(&out)
    );
}

/// Carves `base..HEAD` through the library with hooks, as `carve_lib` does for plain runs.
fn carve_lib_since(
    repo: &TestRepo,
    base: &str,
    hooks: &Hooks,
) -> git_carve::Result<git_carve::commit::UndoRecord> {
    let r = repo.open();
    let snap = snapshot::take_since(&r, Some(base))?;
    let changes = atomize(&r, &snap)?;
    let groups: Vec<Group> = per_file(&changes)
        .into_iter()
        .enumerate()
        .map(|(k, atoms)| Group {
            atoms,
            message: format!("Commit {}\n", k + 1),
        })
        .collect();
    commit(&r, &snap, &changes, &groups, hooks)
}

#[test]
fn safety_range_cas_race_aborts_without_moving_the_ref() {
    let (repo, base) = wip_history();
    let digest = repo.worktree_digest();
    let racer = repo.run(&["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "racer"]);
    let move_ref = || {
        repo.run(&["update-ref", "refs/heads/main", &racer]);
    };
    let hooks = Hooks {
        before_ref_update: Some(&move_ref),
        ..Hooks::default()
    };
    let err = carve_lib_since(&repo, &base, &hooks).unwrap_err();
    assert!(matches!(err, Error::Concurrent(_)), "{err}");
    assert_eq!(repo.run(&["rev-parse", "main"]), racer);
    assert_eq!(repo.worktree_digest(), digest);
}

#[test]
fn safety_range_worktree_edit_during_carve_aborts() {
    for edit in ["src/format.ts", "README-new.md"] {
        let (repo, base) = wip_history();
        let head = repo.run(&["rev-parse", "HEAD"]);
        let touch = || repo.write(edit, "edited mid-run\n");
        let hooks = Hooks {
            before_guard: Some(&touch),
            ..Hooks::default()
        };
        let err = carve_lib_since(&repo, &base, &hooks).unwrap_err();
        assert!(matches!(err, Error::Concurrent(_)), "{edit}: {err}");
        assert_eq!(repo.run(&["rev-parse", "HEAD"]), head, "{edit}: ref moved");
    }
}

#[test]
fn safety_range_undo_is_exact() {
    let (repo, base) = wip_history();
    let before = repo.observable();
    let digest = repo.worktree_digest();
    carve_lib_since(&repo, &base, &Hooks::default()).unwrap();
    assert_ne!(repo.observable(), before);
    let out = repo.carve().arg("--undo").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.observable(), before);
    assert_eq!(repo.worktree_digest(), digest);
}

// The plan buffer header and JSON (step 5).

#[test]
fn range_buffer() {
    let (repo, _) = wip_history();
    repo.write("CHANGELOG.md", "- retry failed fetches\n");
    let out = repo
        .carve()
        .args(["HEAD~3", "--plan-only", "--no-check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    insta::assert_snapshot!("range_buffer", out_str(&out));
}

#[test]
fn range_header_is_ignored_by_the_parser() {
    let (repo, base) = wip_history();
    let (_dir, ed) = editor(&format!(
        "grep -q '^# Re-carving 3 commits since {}' \"$1\" || exit 1\n{TYPE_MESSAGES}",
        &base[..7]
    ));
    let out = repo
        .carve()
        .env("GIT_EDITOR", &ed)
        .args(["HEAD~3", "--no-check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    // The old subjects were shown, but only the typed messages were committed.
    assert_eq!(
        repo.run(&["log", "--format=%s", &format!("{base}..HEAD")]),
        "Commit 3 subject\nCommit 2 subject\nCommit 1 subject"
    );
}

#[test]
fn range_plan_only_json_has_range_block() {
    let (repo, base) = wip_history();
    let out = repo
        .carve()
        .args(["HEAD~3", "--plan-only", "--json", "--no-check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["range"]["base"], base.as_str());
    assert_eq!(doc["range"]["merge_based"], false);
    let subjects: Vec<&str> = doc["range"]["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert_eq!(subjects, ["wip", "asdf", "fix stuff"]);

    // A plain carve has no range block.
    let plain = TestRepo::demo();
    let out = plain
        .carve()
        .args(["--plan-only", "--json", "--no-check"])
        .output()
        .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc.get("range").is_none());
}

// Step 6: random histories. Random files through a base commit, one or two range commits,
// and an optional dirty layer; a random base in the chain; a random ordered partition with
// a random leave set. Checks the worktree is untouched, the new commits sit on the base, the
// result reconstructs the working tree exactly (modulo left files), and undo is exact.

fn content() -> impl Strategy<Value = Option<String>> {
    prop::option::weighted(
        0.8,
        prop::collection::vec(
            prop::sample::select(vec!["a\n", "b\n", "c\n", "d\n", "e\r\n", "\n", "x"]),
            0..10,
        )
        .prop_map(|lines| lines.concat()),
    )
}

fn apply(repo: &TestRepo, i: usize, state: &Option<String>) {
    let path = format!("f{i}.ts");
    match state {
        Some(text) => repo.write(&path, text),
        None => {
            if repo.path().join(&path).exists() {
                repo.remove(&path);
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    #[test]
    fn safety_range_random_histories_reconstruct_exactly(
        files in prop::collection::vec((content(), content(), content(), content()), 1..5),
        second_commit in any::<bool>(),
        dirty_layer in any::<bool>(),
        base_after_first in any::<bool>(),
        seed in any::<u64>(),
        leave_every in 0usize..4,
    ) {
        let repo = TestRepo::new();
        repo.write("anchor.txt", "keep HEAD non-empty\n");
        for (i, f) in files.iter().enumerate() { apply(&repo, i, &f.0); }
        let b0 = repo.commit_all("base");
        for (i, f) in files.iter().enumerate() { apply(&repo, i, &f.1); }
        repo.run(&["add", "-A"]);
        repo.run(&["commit", "-q", "--allow-empty", "-m", "wip one"]);
        let b1 = repo.run(&["rev-parse", "HEAD"]);
        if second_commit {
            for (i, f) in files.iter().enumerate() { apply(&repo, i, &f.2); }
            repo.run(&["add", "-A"]);
            repo.run(&["commit", "-q", "--allow-empty", "-m", "wip two"]);
        }
        if dirty_layer {
            for (i, f) in files.iter().enumerate() { apply(&repo, i, &f.3); }
        }
        let base = if base_after_first && second_commit { b1 } else { b0 };

        let r = repo.open();
        let snap = match snapshot::take_since(&r, Some(&base)) {
            Ok(snap) => snap,
            Err(Error::Precondition(m)) if m.contains("nothing net") => {
                prop_assume!(false);
                unreachable!()
            }
            Err(e) => return Err(TestCaseError::fail(format!("{e}"))),
        };
        let changes = atomize(&r, &snap).unwrap();
        prop_assume!(!changes.atoms.is_empty());

        let mut groups: Vec<Vec<AtomId>> = vec![Vec::new(); 3];
        for (i, atom) in changes.atoms.iter().enumerate() {
            let h = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(i as u32 * 7) as usize;
            if !(leave_every > 0 && h % 5 < leave_every) { groups[h % 3].push(atom.id); }
        }
        for bind in &changes.binds {
            for g in &mut groups { g.retain(|&id| id != bind.b); }
            if let Some(g) = groups.iter_mut().find(|g| g.contains(&bind.a)) { g.push(bind.b); }
        }
        groups.retain(|g| !g.is_empty());
        prop_assume!(!groups.is_empty());
        let placed: BTreeSet<AtomId> = groups.iter().flatten().copied().collect();
        let left_files: BTreeSet<Vec<u8>> = changes.atoms.iter()
            .filter(|a| !placed.contains(&a.id))
            .map(|a| changes.files[a.file].path.clone()).collect();

        let oracle = repo.oracle_tree();
        let before_tree = repo.worktree_digest();
        let before_state = repo.observable();
        let n = groups.len();
        let groups: Vec<Group> = groups.into_iter().enumerate()
            .map(|(k, atoms)| Group { atoms, message: format!("Commit {}\n", k + 1) })
            .collect();
        let record = commit(&r, &snap, &changes, &groups, &Hooks::default()).unwrap();
        prop_assert_eq!(record.commits.len(), n);
        prop_assert_eq!(repo.worktree_digest(), before_tree.clone());
        prop_assert_eq!(repo.run(&["rev-parse", &format!("HEAD~{n}")]), base);

        let differing = repo.git().run(["diff-tree", "-r", "--name-only", "-z", &repo.head_tree(), &oracle]).unwrap();
        let differing: BTreeSet<Vec<u8>> = differing.split(|&b| b == 0).filter(|p| !p.is_empty()).map(<[u8]>::to_vec).collect();
        prop_assert!(differing.is_subset(&left_files), "{:?} not within {:?}", differing, left_files);
        if left_files.is_empty() { prop_assert_eq!(repo.head_tree(), oracle); }

        prop_assert!(repo.carve().arg("--undo").output().unwrap().status.success());
        prop_assert_eq!(repo.observable(), before_state);
        prop_assert_eq!(repo.worktree_digest(), before_tree);
    }
}

// Step 7: each block names the old commits its lines came from.

#[test]
fn range_attribution_names_the_old_commit_per_block() {
    let (repo, _) = wip_history();
    let short = |rev: &str| repo.run(&["rev-parse", "--short=7", rev]);
    let (wip, asdf, fix) = (short("HEAD~2"), short("HEAD~1"), short("HEAD"));
    repo.write("CHANGELOG.md", "- retry failed fetches\n");
    let out = repo
        .carve()
        .args(["HEAD~3", "--plan-only", "--no-check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = out_str(&out);
    let froms: Vec<&str> = text.lines().filter(|l| l.starts_with("# from: ")).collect();
    // The rename touched files from "wip" and "fix stuff"; the retry feature and the
    // parseHeader fix came from "asdf"; CHANGELOG.md is uncommitted.
    assert!(
        froms
            .iter()
            .any(|l| l.contains(&format!("{wip} wip")) && l.contains(&format!("{fix} fix stuff"))),
        "{text}"
    );
    assert!(
        froms
            .iter()
            .filter(|l| l.contains(&format!("{asdf} asdf")))
            .count()
            >= 2,
        "{text}"
    );
    assert!(
        froms.iter().any(|l| l.contains("uncommitted (file)")),
        "{text}"
    );

    let out = repo
        .carve()
        .args(["HEAD~3", "--plan-only", "--json", "--no-check"])
        .output()
        .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let sources = doc["range"]["sources"].as_object().unwrap();
    assert!(!sources.is_empty());
}

// Step 8: the product path plans exactly like the evaluated path.

#[test]
fn range_cli_equals_eval_materialization() {
    // Path A, as the eval harness: the window's net change as a dirty tree on its base.
    let (a, base) = wip_history();
    a.run(&["reset", "-q", &base]);
    let eval = a
        .carve()
        .args(["--plan-only", "--json", "--no-check"])
        .output()
        .unwrap();
    assert!(eval.status.success(), "{}", stderr(&eval));
    // Path B, the product: the same history, committed, re-carved from its base.
    let (b, base_b) = wip_history();
    assert_eq!(base, base_b, "deterministic fixtures");
    let range = b
        .carve()
        .args([&base, "--plan-only", "--json", "--no-check"])
        .output()
        .unwrap();
    assert!(range.status.success(), "{}", stderr(&range));
    let strip = |out: &std::process::Output| {
        let mut doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        doc.as_object_mut().unwrap().remove("range");
        // The buffer (D50) adds the range header and `# from:` lines in range mode.
        doc.as_object_mut().unwrap().remove("buffer");
        doc
    };
    assert_eq!(strip(&eval), strip(&range));
}

#[test]
fn range_header_lists_old_trailers_to_copy() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    let base = repo.commit_all("base");
    repo.write("a.txt", "a2\n");
    repo.run(&[
        "commit",
        "-q",
        "-am",
        "wip\n\nCo-authored-by: Agent <agent@example.com>",
    ]);
    let out = repo
        .carve()
        .args([&base, "--plan-only", "--no-check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = out_str(&out);
    assert!(
        text.contains("# Trailers from those messages (copy any you want to keep):\n#   Co-authored-by: Agent <agent@example.com>"),
        "{text}"
    );
}
