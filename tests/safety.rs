//! The data-loss guard (SPEC §13): the worktree is never written, undo is exact, and every
//! race aborts without moving anything.

mod common;

use std::collections::BTreeSet;

use common::{TestRepo, carve_in, one_group, per_file, stderr};
use git_carve::Error;
use git_carve::atom::AtomId;
use git_carve::commit::Hooks;
use proptest::prelude::*;

/// Text edits, a new file, a deletion, a symlink retarget, an exec bit, a rename, a
/// large untracked file (default leave), and a debug-looking line.
fn messy() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("src/net.ts", "export function parseHeader(s: string) {\n  return s.slice(0, 10);\n}\n\nexport function fetchAll() {\n  return 1;\n}\n");
    repo.write(
        "src/util.ts",
        "export const fmtBytes = (n: number) => `${n} B`;\n",
    );
    repo.write("run.sh", "#!/bin/sh\necho hi\n");
    repo.write("old.txt", "a\nb\nc\nd\n");
    repo.write("gone.ts", "bye\n");
    repo.symlink("src/net.ts", "link");
    repo.commit_all("base");
    repo.write("src/net.ts", "export function parseHeader(s: string) {\n  return s.slice(0, 11);\n}\n\nexport function fetchAll() {\n  console.log(\"here\");\n  return 1;\n}\n");
    repo.write(
        "src/retry.ts",
        "export async function retryWithBackoff() {}\n",
    );
    repo.remove("gone.ts");
    repo.symlink("src/util.ts", "link");
    repo.set_exec("run.sh", true);
    repo.remove("old.txt");
    repo.write("new.txt", "a\nb\nc\nd\n");
    repo.write("dump.bin", vec![0u8; 3 << 20]);
    repo
}

#[test]
fn safety_worktree_untouched_by_carve_and_undo() {
    let repo = messy();
    let before = repo.worktree_digest();
    let record = repo.carve_lib(per_file, &Hooks::default()).unwrap();
    assert!(record.commits.len() > 1);
    assert_eq!(repo.worktree_digest(), before, "carve changed the worktree");
    let out = repo.carve().arg("--undo").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.worktree_digest(), before, "undo changed the worktree");
}

#[test]
fn safety_undo_restores_exact_state() {
    let repo = messy();
    let before = repo.observable();
    repo.carve_lib(per_file, &Hooks::default()).unwrap();
    assert_ne!(repo.observable(), before);
    let out = repo.carve().arg("--undo").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.observable(), before);
}

#[test]
fn safety_leave_atoms_stay_unstaged_and_nothing_else_does() {
    let repo = messy();
    let record = repo.carve_lib(per_file, &Hooks::default()).unwrap();
    assert_eq!(record.left, vec!["dump.bin"]);
    assert_eq!(repo.run(&["status", "--porcelain"]), "?? dump.bin");
    assert_eq!(repo.run(&["diff", "--cached", "--name-only"]), "");
}

#[test]
fn safety_cas_race_aborts_without_moving_the_ref() {
    let repo = messy();
    let before = repo.worktree_digest();
    let racer = repo.run(&["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "racer"]);
    let move_ref = || {
        repo.run(&["update-ref", "refs/heads/main", &racer]);
    };
    let hooks = Hooks {
        before_ref_update: Some(&move_ref),
        ..Hooks::default()
    };
    let err = repo.carve_lib(per_file, &hooks).unwrap_err();
    assert!(matches!(err, Error::Concurrent(_)), "{err}");
    assert_eq!(
        repo.run(&["rev-parse", "main"]),
        racer,
        "carve overwrote the racing ref"
    );
    assert_eq!(repo.worktree_digest(), before);
    assert!(!repo.open().carve_dir.join("undo.json").exists());
}

#[test]
fn safety_worktree_edit_during_carve_aborts() {
    for (name, edit) in [
        ("changed file", "src/net.ts"),
        ("clean file", "src/util.ts"),
        ("new file", "brand-new.ts"),
    ] {
        let repo = messy();
        let head = repo.run(&["rev-parse", "HEAD"]);
        let touch = || repo.write(edit, "edited mid-run\n");
        let hooks = Hooks {
            before_guard: Some(&touch),
            ..Hooks::default()
        };
        let err = repo.carve_lib(per_file, &hooks).unwrap_err();
        assert!(matches!(err, Error::Concurrent(_)), "{name}: {err}");
        assert_eq!(repo.run(&["rev-parse", "HEAD"]), head, "{name}: ref moved");
    }
}

#[test]
fn safety_branch_switch_during_carve_aborts() {
    let repo = messy();
    let main = repo.run(&["rev-parse", "main"]);
    repo.run(&["branch", "other"]);
    let switch = || {
        repo.run(&["symbolic-ref", "HEAD", "refs/heads/other"]);
    };
    let hooks = Hooks {
        before_guard: Some(&switch),
        ..Hooks::default()
    };
    let err = repo.carve_lib(per_file, &hooks).unwrap_err();
    assert!(matches!(err, Error::Concurrent(_)), "{err}");
    assert_eq!(repo.run(&["rev-parse", "main"]), main);
}

#[test]
fn safety_undo_refuses_after_a_new_commit() {
    let repo = messy();
    repo.carve_lib(per_file, &Hooks::default()).unwrap();
    repo.run(&["commit", "-q", "--allow-empty", "-m", "later"]);
    let head = repo.run(&["rev-parse", "HEAD"]);
    let out = repo.carve().arg("--undo").output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("git reset"), "{}", stderr(&out));
    assert_eq!(repo.run(&["rev-parse", "HEAD"]), head);
}

#[test]
fn safety_undo_refuses_with_staged_changes() {
    let repo = messy();
    repo.carve_lib(per_file, &Hooks::default()).unwrap();
    repo.run(&["add", "dump.bin"]);
    let head = repo.run(&["rev-parse", "HEAD"]);
    let out = repo.carve().arg("--undo").output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(repo.run(&["rev-parse", "HEAD"]), head);
    assert_eq!(repo.run(&["diff", "--cached", "--name-only"]), "dump.bin");
}

#[test]
fn safety_undo_is_one_level() {
    let repo = messy();
    repo.carve_lib(per_file, &Hooks::default()).unwrap();
    assert!(
        repo.carve()
            .arg("--undo")
            .output()
            .unwrap()
            .status
            .success()
    );
    let out = repo.carve().arg("--undo").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("nothing to undo"));
}

#[test]
fn safety_detached_head_carve_and_undo() {
    let repo = messy();
    repo.run(&["checkout", "-q", "--detach"]);
    let before = repo.observable();
    let record = repo.carve_lib(per_file, &Hooks::default()).unwrap();
    assert!(record.detached);
    assert_eq!(repo.run(&["rev-parse", "HEAD"]), record.new);
    assert!(
        carve_in(repo.path())
            .arg("--undo")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(repo.observable(), before);
}

#[test]
fn safety_sha256_repository_end_to_end() {
    let repo = TestRepo::sha256();
    repo.write("a.ts", "1\n2\n3\n4\n5\n");
    repo.write("gone.ts", "g\n");
    repo.commit_all("base");
    repo.write("a.ts", "1\nTWO\n3\n4\nFIVE\n");
    repo.remove("gone.ts");
    repo.write("new.ts", "n\n");
    let before = repo.observable();
    let oracle = repo.oracle_tree();
    assert_eq!(oracle.len(), 64);
    let record = repo.carve_lib(per_file, &Hooks::default()).unwrap();
    assert!(record.commits.iter().all(|c| c.len() == 64));
    assert_eq!(repo.head_tree(), oracle);
    assert!(
        repo.carve()
            .arg("--undo")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(repo.observable(), before);
}

#[test]
fn safety_invalid_plans_change_nothing() {
    let repo = messy();
    let head = repo.run(&["rev-parse", "HEAD"]);
    let changes = repo.changes();
    let rename = changes.binds[0].clone();
    let cases: Vec<(&str, Vec<Vec<AtomId>>)> = vec![
        ("duplicate", vec![vec![AtomId(1)], vec![AtomId(1)]]),
        ("unknown", vec![vec![AtomId(999)]]),
        ("empty group", vec![vec![]]),
        ("split bind", vec![vec![rename.a], vec![rename.b]]),
    ];
    for (name, groups) in cases {
        let err = repo
            .carve_lib(move |_| groups, &Hooks::default())
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{name}: {err}");
        assert_eq!(repo.run(&["rev-parse", "HEAD"]), head, "{name}");
    }
}

#[test]
fn safety_empty_message_is_refused() {
    let repo = messy();
    let r = repo.open();
    let snap = git_carve::snapshot::take(&r).unwrap();
    let changes = git_carve::atom::atomize(&r, &snap).unwrap();
    let groups = vec![git_carve::commit::Group {
        atoms: one_group(&changes).remove(0),
        message: "  \n\n".into(),
    }];
    let err =
        git_carve::commit::commit(&r, &snap, &changes, &groups, &Hooks::default()).unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err}");
}

// Random repositories, random edits, random ordered partitions with a random leave set.
// Checks R3 against the git-add oracle, R4 via the atom bookkeeping, and R5 plus exact
// undo on every case. Real git processes, so fewer cases than the pure tests.

fn content() -> impl Strategy<Value = Option<String>> {
    prop::option::weighted(
        0.85,
        prop::collection::vec(
            prop::sample::select(vec!["a\n", "b\n", "c\n", "d\n", "e\r\n", "\n", "x"]),
            0..12,
        )
        .prop_map(|lines| lines.concat()),
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn safety_random_partitions_reconstruct_exactly(
        files in prop::collection::vec((content(), content()), 1..5),
        seed in any::<u64>(),
        leave_every in 0usize..4,
    ) {
        let repo = TestRepo::new();
        repo.write("anchor.txt", "keep HEAD non-empty\n");
        for (i, (base, _)) in files.iter().enumerate() {
            if let Some(base) = base { repo.write(&format!("f{i}.ts"), base); }
        }
        repo.commit_all("base");
        let mut dirty = false;
        for (i, (base, new)) in files.iter().enumerate() {
            let path = format!("f{i}.ts");
            match (base, new) {
                (Some(_), None) => { repo.remove(&path); dirty = true; }
                (_, Some(new)) => { repo.write(&path, new); dirty |= base.as_deref() != Some(new.as_str()); }
                (None, None) => {}
            }
        }
        prop_assume!(dirty);
        let changes = repo.changes();
        prop_assume!(!changes.atoms.is_empty());

        // Random ordered partition: atom i goes to group (hash % 3), or to leave.
        let mut groups: Vec<Vec<AtomId>> = vec![Vec::new(); 3];
        let mut left = BTreeSet::new();
        for (i, atom) in changes.atoms.iter().enumerate() {
            let h = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(i as u32 * 7) as usize;
            if leave_every > 0 && h % 5 < leave_every { left.insert(atom.file); }
            else { groups[h % 3].push(atom.id); }
        }
        // Keep hard binds together by moving any bound atom into its partner's group.
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
        let record = repo.carve_lib(|_| groups.clone(), &Hooks::default()).unwrap();
        prop_assert_eq!(record.commits.len(), n);
        prop_assert_eq!(repo.worktree_digest(), before_tree.clone());

        // R3: the carved tree differs from the oracle only in files with left-out atoms.
        let differing = repo.git().run(["diff-tree", "-r", "--name-only", "-z", &repo.head_tree(), &oracle]).unwrap();
        let differing: BTreeSet<Vec<u8>> = differing.split(|&b| b == 0).filter(|p| !p.is_empty()).map(<[u8]>::to_vec).collect();
        prop_assert!(differing.is_subset(&left_files), "{:?} not within {:?}", differing, left_files);
        if left_files.is_empty() { prop_assert_eq!(repo.head_tree(), oracle); }

        prop_assert!(repo.carve().arg("--undo").output().unwrap().status.success());
        prop_assert_eq!(repo.observable(), before_state);
        prop_assert_eq!(repo.worktree_digest(), before_tree);
    }
}
