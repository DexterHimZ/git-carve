//! Edge-case fixtures (SPEC §13). Each carve is compared against `git add -A` in a throwaway
//! index, an oracle that shares no code with carve.

mod common;

use common::{TestRepo, one_group, per_file};
use git_carve::atom::{AtomKind, BindReason, Whole};
use git_carve::commit::Hooks;

/// Carves every atom, then checks the new HEAD tree against the oracle and the worktree
/// against its earlier bytes.
fn carve_matches_oracle(repo: &TestRepo) {
    let oracle = repo.oracle_tree();
    let before = repo.worktree_digest();
    repo.carve_lib(one_group, &Hooks::default()).expect("carve");
    assert_eq!(
        repo.head_tree(),
        oracle,
        "carved tree differs from git add -A"
    );
    assert_eq!(repo.worktree_digest(), before, "worktree changed");
}

fn kinds(repo: &TestRepo) -> Vec<AtomKind> {
    repo.changes().atoms.into_iter().map(|a| a.kind).collect()
}

#[test]
fn crlf_with_autocrlf_diffs_in_repository_form() {
    let repo = TestRepo::new();
    repo.run(&["config", "core.autocrlf", "true"]);
    repo.write("win.txt", "a\r\nb\r\nc\r\n");
    repo.commit_all("base");
    repo.write("win.txt", "a\r\nB\r\nc\r\n");
    assert_eq!(kinds(&repo).len(), 1);
    carve_matches_oracle(&repo);
    assert_eq!(repo.run(&["show", "HEAD:win.txt"]), "a\nB\nc");
}

#[test]
fn crlf_without_conversion_is_kept_byte_for_byte() {
    let repo = TestRepo::new();
    repo.write("f.txt", "a\r\nb\r\nc\r\n");
    repo.commit_all("base");
    repo.write("f.txt", "a\r\nb\nc\r\nd\r\n");
    carve_matches_oracle(&repo);
}

#[test]
fn missing_final_newline_is_a_different_last_line() {
    let repo = TestRepo::new();
    repo.write("f.txt", "1\n2\n3\n4\n5\nend");
    repo.commit_all("base");
    repo.write("f.txt", "1\nTWO\n3\n4\n5\nend\n");
    let changes = repo.changes();
    assert_eq!(changes.atoms.len(), 2);
    // Two commits: the final-line fix first, then the earlier edit.
    repo.carve_lib(
        |c| vec![vec![c.atoms[1].id], vec![c.atoms[0].id]],
        &Hooks::default(),
    )
    .unwrap();
    assert_eq!(
        repo.git().run(["show", "HEAD~1:f.txt"]).unwrap(),
        b"1\n2\n3\n4\n5\nend\n"
    );
    assert_eq!(
        repo.git().run(["show", "HEAD:f.txt"]).unwrap(),
        b"1\nTWO\n3\n4\n5\nend\n"
    );
}

#[test]
fn regions_of_one_file_split_across_commits() {
    let repo = TestRepo::new();
    repo.write("f.ts", "a\nb\nc\nd\ne\nf\ng\n");
    repo.commit_all("base");
    repo.write("f.ts", "A\nb\nC\nd\ne\nF\ng\n");
    let changes = repo.changes();
    assert_eq!(changes.atoms.len(), 3);
    let [a, c, f] = [
        changes.atoms[0].id,
        changes.atoms[1].id,
        changes.atoms[2].id,
    ];
    let oracle = repo.oracle_tree();
    repo.carve_lib(|_| vec![vec![a, f], vec![c]], &Hooks::default())
        .unwrap();
    assert_eq!(
        repo.git().run(["show", "HEAD~1:f.ts"]).unwrap(),
        b"A\nb\nc\nd\ne\nF\ng\n"
    );
    assert_eq!(repo.head_tree(), oracle);
}

#[test]
fn empty_files_in_both_directions() {
    let repo = TestRepo::new();
    repo.write("emptied.txt", "x\ny\n");
    repo.write("filled.txt", "");
    repo.commit_all("base");
    repo.write("emptied.txt", "");
    repo.write("filled.txt", "now\n");
    repo.write("new-empty.txt", "");
    carve_matches_oracle(&repo);
}

#[test]
fn binary_file_is_one_whole_atom() {
    let repo = TestRepo::new();
    repo.write("img.bin", b"\x89PNG\0\x01\x02");
    repo.commit_all("base");
    repo.write("img.bin", b"\x89PNG\0\x03\x04\x05");
    assert_eq!(kinds(&repo), vec![AtomKind::Whole(Whole::Binary)]);
    carve_matches_oracle(&repo);
}

#[test]
fn non_utf8_text_is_one_whole_atom() {
    let repo = TestRepo::new();
    repo.write("latin1.txt", b"caf\xe9\n");
    repo.commit_all("base");
    repo.write("latin1.txt", b"caf\xe9\nna\xefve\n");
    assert_eq!(kinds(&repo), vec![AtomKind::Whole(Whole::NonUtf8)]);
    carve_matches_oracle(&repo);
}

#[test]
fn exec_bit_is_its_own_atom_and_can_land_separately() {
    let repo = TestRepo::new();
    repo.write("run.sh", "#!/bin/sh\necho a\n");
    repo.commit_all("base");
    repo.write("run.sh", "#!/bin/sh\necho b\n");
    repo.set_exec("run.sh", true);
    let changes = repo.changes();
    let find = |mode: bool| {
        changes
            .atoms
            .iter()
            .find(|a| matches!(a.kind, AtomKind::Mode { .. }) == mode)
            .unwrap()
            .id
    };
    let (region, mode) = (find(false), find(true));
    let oracle = repo.oracle_tree();
    repo.carve_lib(|_| vec![vec![mode], vec![region]], &Hooks::default())
        .unwrap();
    let first = repo.run(&["ls-tree", "HEAD~1", "run.sh"]);
    assert!(first.starts_with("100755"), "{first}");
    assert_eq!(repo.run(&["show", "HEAD~1:run.sh"]), "#!/bin/sh\necho a");
    assert_eq!(repo.head_tree(), oracle);
}

#[test]
fn symlinks_are_hashed_as_links_not_followed() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    repo.symlink("a.txt", "link");
    repo.commit_all("base");
    repo.symlink("b.txt", "link");
    repo.symlink("a.txt", "new-link");
    carve_matches_oracle(&repo);
    assert_eq!(repo.run(&["cat-file", "-p", "HEAD:link"]), "b.txt");
}

#[test]
fn file_to_symlink_and_back() {
    let repo = TestRepo::new();
    repo.write("x", "x\n");
    repo.write("y.txt", "y\n");
    repo.symlink("y.txt", "was-link");
    repo.commit_all("base");
    repo.remove("x");
    repo.symlink("y.txt", "x");
    repo.remove("was-link");
    repo.write("was-link", "now a file\n");
    assert!(
        kinds(&repo)
            .iter()
            .all(|k| *k == AtomKind::Whole(Whole::TypeChange))
    );
    carve_matches_oracle(&repo);
}

#[test]
fn large_untracked_file_defaults_to_leave() {
    let repo = TestRepo::new();
    repo.write("src.ts", "a\n");
    repo.commit_all("base");
    repo.write("src.ts", "b\n");
    repo.write("dump.log", "x".repeat(2 << 20));
    let changes = repo.changes();
    let plan = git_carve::group::per_file(&changes);
    assert_eq!(plan.leave.len(), 1);
    repo.carve_lib(per_file, &Hooks::default()).unwrap();
    assert_eq!(repo.run(&["status", "--porcelain"]), "?? dump.log");
}

#[test]
fn rename_with_modification_is_bound() {
    let repo = TestRepo::new();
    repo.write(
        "old.ts",
        "export const x = 1;\nexport const y = 2;\nexport const z = 3;\n",
    );
    repo.commit_all("base");
    repo.remove("old.ts");
    repo.write(
        "new.ts",
        "export const x = 1;\nexport const y = 2;\nexport const z = 4;\n",
    );
    let changes = repo.changes();
    assert_eq!(changes.binds.len(), 1);
    assert_eq!(changes.binds[0].reason, BindReason::Rename);
    assert_eq!(per_file(&changes).len(), 1, "rename halves land together");
    carve_matches_oracle(&repo);
}

#[test]
fn deleting_a_whole_file() {
    let repo = TestRepo::new();
    repo.write("keep.ts", "k\n");
    repo.write("gone.ts", "g\n");
    repo.commit_all("base");
    repo.remove("gone.ts");
    assert_eq!(kinds(&repo), vec![AtomKind::Whole(Whole::Deleted)]);
    carve_matches_oracle(&repo);
}

#[test]
fn file_replaced_by_directory_is_bound() {
    let repo = TestRepo::new();
    repo.write("x", "file\n");
    repo.write("d/f", "in dir\n");
    repo.commit_all("base");
    repo.remove("x");
    repo.write("x/inner.ts", "now a dir\n");
    repo.remove("d/f");
    std::fs::remove_dir(repo.path().join("d")).unwrap();
    repo.write("d", "now a file\n");
    let changes = repo.changes();
    assert_eq!(
        changes
            .binds
            .iter()
            .filter(|b| b.reason == BindReason::DirectoryFile)
            .count(),
        2
    );
    carve_matches_oracle(&repo);
}

#[test]
fn filtered_path_is_whole_and_uses_the_clean_filter() {
    let repo = TestRepo::new();
    repo.run(&["config", "filter.upper.clean", "tr a-z A-Z"]);
    repo.run(&["config", "filter.upper.smudge", "cat"]);
    repo.write(".gitattributes", "*.dat filter=upper\n");
    repo.write("x.dat", "hello\n");
    repo.commit_all("base");
    repo.write("x.dat", "hello\nworld\n");
    assert_eq!(kinds(&repo), vec![AtomKind::Whole(Whole::Filtered)]);
    carve_matches_oracle(&repo);
    assert_eq!(repo.run(&["show", "HEAD:x.dat"]), "HELLO\nWORLD");
}

#[test]
fn awkward_paths_are_literal() {
    let repo = TestRepo::new();
    repo.write("a b*.ts", "1\n");
    repo.write("a b-other.ts", "2\n");
    repo.write("dir/é [x].ts", "3\n");
    repo.commit_all("base");
    repo.write("a b*.ts", "1!\n");
    repo.write("dir/é [x].ts", "3!\n");
    repo.write("-dash.ts", "new\n");
    carve_matches_oracle(&repo);
}

/// Regression: two files with the same original content must each get their own base.
#[test]
fn files_sharing_a_base_blob_rebuild_independently() {
    let repo = TestRepo::new();
    let same = "one\ntwo\nthree\nfour\nfive\n";
    repo.write("a.ts", same);
    repo.write("b.ts", same);
    repo.commit_all("base");
    repo.write("a.ts", "ONE\ntwo\nthree\nfour\nFIVE\n");
    repo.write("b.ts", "one\ntwo\nTHREE\nfour\nfive\n");
    let changes = repo.changes();
    let a_first = changes.atoms[0].id;
    let rest: Vec<_> = changes.atoms[1..].iter().map(|a| a.id).collect();
    let oracle = repo.oracle_tree();
    repo.carve_lib(|_| vec![vec![a_first], rest], &Hooks::default())
        .unwrap();
    assert_eq!(
        repo.run(&["show", "HEAD~1:a.ts"]),
        "ONE\ntwo\nthree\nfour\nfive"
    );
    assert_eq!(
        repo.run(&["show", "HEAD~1:b.ts"]),
        "one\ntwo\nthree\nfour\nfive"
    );
    assert_eq!(repo.head_tree(), oracle);
}
