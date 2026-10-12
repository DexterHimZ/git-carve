//! M3 acceptance (SPEC §12): the check runs where the spec says, failing prefixes merge
//! forward, `git rebase -x` passes on every commit carve makes, and the check's verdicts
//! are trustworthy (probe, cache fingerprint, syntax pre-check).

mod common;

use std::fs;
use std::time::Instant;

use common::{TestRepo, stderr};
use git_carve::Error;
use git_carve::atom::AtomId;
use git_carve::commit::Hooks;
use git_carve::verify::Dropped;

/// Three unrelated text files with different edits: three groups the graph cannot link.
/// (Identical edits would be a systematic edit and link, DECISIONS.md D26.)
fn three_files() -> TestRepo {
    let repo = TestRepo::new();
    for f in ["a.txt", "b.txt", "c.txt"] {
        repo.write(f, "base\n");
    }
    repo.commit_all("base");
    for (f, text) in [
        ("a.txt", "alpha\n"),
        ("b.txt", "bravo\n"),
        ("c.txt", "charlie\n"),
    ] {
        repo.write(f, text);
    }
    repo
}

#[test]
fn merge_forward_repairs_a_bad_split() {
    let repo = TestRepo::new();
    repo.write("a.txt", "plain\n");
    repo.write("b.txt", "plain\n");
    repo.commit_all("base");
    repo.write("a.txt", "useHelper\n");
    repo.write("b.txt", "defHelper\n");
    let check = "if grep -q useHelper a.txt; then grep -q defHelper b.txt; fi";
    let (_, _, before) = repo.plan();
    assert_eq!(before.groups.len(), 2, "the graph splits them");
    let (_, plan, v) = repo.verified(Some(check)).unwrap();
    assert_eq!(plan.groups.len(), 1, "merge-forward fused them");
    assert!(
        matches!(
            v.dropped.as_slice(),
            [Dropped::CheckFailed { after: 1, .. }]
        ),
        "{:?}",
        v.dropped
    );
    assert_eq!(v.runs, 3, "final, base, one prefix: N+1 for N = 2 (D2)");
}

#[test]
fn rebase_exec_passes_on_every_commit() {
    let repo = three_files();
    let log = tempfile::NamedTempFile::new().unwrap();
    let check = "test -f a.txt && test -f b.txt";
    let (_, plan, v) = repo.verified(Some(check)).unwrap();
    assert_eq!(plan.groups.len(), 3);
    assert!(v.dropped.is_empty());
    let base = repo.run(&["rev-parse", "HEAD"]);
    let groups = plan.groups.clone();
    repo.carve_lib(|_| groups, &Hooks::default()).unwrap();

    let marked = format!("echo ran >> {} && {check}", log.path().display());
    let out = repo
        .git()
        .run(["rebase", "-q", "-x", marked.as_str(), base.as_str()]);
    assert!(out.is_ok(), "rebase -x failed: {out:?}");
    assert_eq!(
        fs::read_to_string(log.path()).unwrap().lines().count(),
        3,
        "once per commit"
    );
}

#[test]
fn check_failing_on_head_exits_3() {
    let repo = TestRepo::new();
    repo.write("bad", "x\n");
    repo.write("a.txt", "a\n");
    repo.commit_all("base");
    repo.remove("bad");
    repo.write("a.txt", "b\n");
    let err = repo.verified(Some("test ! -f bad")).unwrap_err();
    assert!(
        matches!(&err, Error::CheckFailed(m) if m.contains("fails on HEAD")),
        "{err}"
    );
    let out = repo
        .carve()
        .args(["--plan-only", "--check", "test ! -f bad"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
}

#[test]
fn failing_working_tree_exits_3() {
    let repo = three_files();
    // Passes on HEAD only: no rescue (D40) can help, since every change breaks it.
    let out = repo
        .carve()
        .args([
            "--plan-only",
            "--check",
            "test \"$(cat a.txt b.txt c.txt)\" = \"$(printf 'base\\nbase\\nbase')\"",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(
        stderr(&out).contains("does not pass the check"),
        "{}",
        stderr(&out)
    );
    // A check that fails everywhere is reported as failing on HEAD.
    let out = repo
        .carve()
        .args(["--plan-only", "--check", "false"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(stderr(&out).contains("fails on HEAD"), "{}", stderr(&out));
}

#[test]
fn load_bearing_leftover_is_put_back() {
    let repo = TestRepo::new();
    repo.write(
        "src/a.ts",
        "export function a() {\n  return 1;\n}\n\nexport function b() {\n  return 2;\n}\n",
    );
    repo.commit_all("base");
    repo.write("src/a.ts", "export function a() {\n  console.log(\"needed\");\n  return 1;\n}\n\nexport function b() {\n  return 3;\n}\n");
    let (_, _, before) = repo.plan();
    assert_eq!(before.leave.len(), 1, "D8 leaves the console.log");
    // Passes on HEAD; once `return 3` lands, the console.log must be there too.
    let check = "if grep -q 'return 3' src/a.ts; then grep -q console.log src/a.ts; fi";
    let (_, plan, v) = repo.verified(Some(check)).unwrap();
    assert!(plan.leave.is_empty());
    assert_eq!(v.unleft, before.leave);
}

#[test]
fn explicit_check_that_cannot_fail_is_reported() {
    let repo = TestRepo::new();
    repo.write("src/a.ts", "export const a = 1;\n");
    repo.commit_all("base");
    repo.write("src/a.ts", "export const a = 2;\n");
    let (_, _, v) = repo.verified(Some("true")).unwrap();
    assert_eq!(v.unverified, vec!["src/a.ts"]);
    let out = repo
        .carve()
        .args(["--plan-only", "--check", "true"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains("WARNING: the check passed on a deliberately broken src/a.ts")
    );
}

#[test]
fn syntax_break_drops_a_boundary_without_a_run() {
    let repo = TestRepo::new();
    repo.write(
        "src/a.ts",
        "export function a(flag: boolean) {\n  return 1;\n}\n",
    );
    repo.commit_all("base");
    repo.write(
        "src/a.ts",
        "export function a(flag: boolean) {\n  if (flag) {\n  return 1;\n  }\n  return 0;\n}\n",
    );
    let (changes, plan, v) = repo
        .verified_groups("true", |c| c.atoms.iter().map(|a| vec![a.id]).collect())
        .unwrap();
    assert_eq!(changes.atoms.len(), 2);
    assert_eq!(plan.groups.len(), 1);
    assert!(
        matches!(v.dropped.as_slice(), [Dropped::Syntax { after: 1, path }] if path == "src/a.ts"),
        "{:?}",
        v.dropped
    );
    assert_eq!(v.runs, 3, "final, base, probe; the broken prefix never ran");
}

#[test]
fn results_are_cached_until_dependencies_change() {
    let repo = TestRepo::new();
    repo.write(".gitignore", "node_modules/\n");
    for f in ["a.txt", "b.txt", "c.txt"] {
        repo.write(f, "base\n");
    }
    repo.commit_all("base");
    for (f, text) in [
        ("a.txt", "alpha\n"),
        ("b.txt", "bravo\n"),
        ("c.txt", "charlie\n"),
    ] {
        repo.write(f, text);
    }
    fs::create_dir_all(repo.path().join("node_modules/dep")).unwrap();
    let check = "test -d node_modules";
    let (_, _, first) = repo.verified(Some(check)).unwrap();
    assert_eq!((first.runs, first.cached), (4, 0));
    let (_, _, second) = repo.verified(Some(check)).unwrap();
    assert_eq!(
        (second.runs, second.cached),
        (0, 4),
        "same trees, same toolchain"
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(repo.path().join("node_modules/.package-lock.json"), "{}").unwrap();
    let (_, _, third) = repo.verified(Some(check)).unwrap();
    assert_eq!(third.runs, 4, "D13: an install invalidates cached verdicts");
}

#[test]
fn checks_time_out() {
    let repo = three_files();
    repo.run(&["config", "carve.checkTimeout", "1"]);
    let started = Instant::now();
    let err = repo.verified(Some("sleep 30")).unwrap_err();
    assert!(
        matches!(&err, Error::CheckFailed(m) if m.contains("timed out")),
        "{err}"
    );
    assert!(started.elapsed().as_secs() < 10);
}

#[test]
fn checks_never_touch_the_working_tree_and_clean_removes_scratch() {
    let repo = three_files();
    let before = repo.worktree_digest();
    repo.verified(Some("touch made-by-check && test -f a.txt"))
        .unwrap();
    assert_eq!(
        repo.worktree_digest(),
        before,
        "the check ran in the scratch worktree"
    );
    assert_eq!(repo.run(&["worktree", "list"]).lines().count(), 2);
    let out = repo.carve().arg("--clean").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.run(&["worktree", "list"]).lines().count(), 1);
    assert!(!repo.path().join(".git/carve/scratch").exists());
}

#[test]
fn demo_repo_verifies_with_the_default_tsc_check() {
    let repo = TestRepo::demo();
    if !repo.link_typescript() {
        return;
    }
    let (_, plan, v) = repo.verified(None).unwrap();
    assert_eq!(plan.groups.len(), 3);
    assert_eq!(v.check, "tsc --noEmit -p .");
    assert_eq!(
        v.probes.len(),
        2,
        "one probe each for src/ and tests/ (D27)"
    );
    assert_eq!(v.runs, 6, "final, base, 2 probes, and 2 prefixes");
    assert!(v.probes.iter().all(|p| p.caught), "{:?}", v.probes);
    assert!(!repo.path().join("node_modules/.tmp").exists());
}

#[test]
fn default_check_follows_vite_project_references() {
    let repo = TestRepo::new();
    repo.write(
        "tsconfig.json",
        "{\n  \"files\": [],\n  \"references\": [{ \"path\": \"./tsconfig.app.json\" }]\n}\n",
    );
    repo.write(
        "tsconfig.app.json",
        "{\n  \"compilerOptions\": {\n    \"tsBuildInfoFile\": \"./node_modules/.tmp/tsconfig.app.tsbuildinfo\",\n    \"strict\": true,\n    \"noEmit\": true,\n    \"lib\": [\"ES2022\", \"DOM\"]\n  },\n  \"include\": [\"src\"]\n}\n",
    );
    repo.write("src/a.ts", "export const a: number = 1;\n");
    repo.write(".gitignore", "node_modules/\n");
    repo.commit_all("base");
    if !repo.link_typescript() {
        return;
    }
    repo.write("src/a.ts", "export const a: number = 2;\n");
    let (_, _, v) = repo.verified(None).unwrap();
    assert_eq!(v.check, "tsc --noEmit -p tsconfig.app.json");
    assert!(v.probes.iter().all(|p| p.caught), "{:?}", v.probes);
    assert!(
        !repo.path().join("node_modules/.tmp").exists(),
        "no build info through the link (D12)"
    );

    // A type error must fail, so `-p .` (which checks nothing here) was not used.
    repo.write("src/a.ts", "export const a: number = \"two\";\n");
    let err = repo.verified(None).unwrap_err();
    assert!(
        matches!(&err, Error::CheckFailed(m) if m.contains("TS2322")),
        "{err}"
    );
}

#[test]
fn default_check_that_does_not_read_the_changed_file_is_refused() {
    let repo = TestRepo::new();
    repo.write("tsconfig.json", "{\n  \"compilerOptions\": { \"strict\": true, \"noEmit\": true },\n  \"include\": [\"lib\"]\n}\n");
    repo.write("lib/l.ts", "export const l = 1;\n");
    repo.write("src/a.ts", "export const a = 1;\n");
    repo.write(".gitignore", "node_modules/\n");
    repo.commit_all("base");
    if !repo.link_typescript() {
        return;
    }
    repo.write("src/a.ts", "export const a = 2;\n");
    let err = repo.verified(None).unwrap_err();
    assert!(
        matches!(&err, Error::CheckFailed(m) if m.contains("deliberately broken src/a.ts")),
        "{err}"
    );
    let _: Vec<AtomId> = Vec::new();
}

/// Found in M3: a dependency directory holding a file carve commits must not be linked into
/// the scratch worktree. Git replaces such a link while materializing trees; the user's
/// files must stay intact and the check must see a consistent tree.
#[test]
fn safety_scratch_links_never_reach_into_user_files() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit_all("base");
    repo.write("a.txt", "b\n");
    repo.write("node_modules/.package-lock.json", "USER DATA\n");
    repo.write("node_modules/dep/file", "KEEP\n");
    let before = repo.worktree_digest();
    let (_, plan, _) = repo.verified(Some("test -f a.txt")).unwrap();
    assert!(!plan.groups.is_empty());
    assert_eq!(repo.worktree_digest(), before, "user files untouched");
    let scratch_link = repo.path().join(".git/carve/scratch/node_modules");
    assert!(!fs::symlink_metadata(&scratch_link).is_ok_and(|m| m.file_type().is_symlink()));
}

/// D27: a changed file in a sub-project with its own tsconfig makes that project part of the
/// default check, instead of carve refusing (found on the user's repository in M5).
#[test]
fn default_check_covers_a_subproject_that_owns_a_change() {
    let repo = TestRepo::new();
    repo.write("tsconfig.json", "{\n  \"compilerOptions\": { \"strict\": true, \"noEmit\": true },\n  \"include\": [\"src\"]\n}\n");
    repo.write("web/tsconfig.json", "{\n  \"compilerOptions\": { \"strict\": true, \"noEmit\": true },\n  \"include\": [\".\"]\n}\n");
    repo.write("src/a.ts", "export const a: number = 1;\n");
    repo.write("web/page.ts", "export const page: string = \"x\";\n");
    repo.write(".gitignore", "node_modules/\n");
    repo.commit_all("base");
    if !repo.link_typescript() {
        return;
    }
    repo.write("src/a.ts", "export const a: number = 2;\n");
    repo.write("web/page.ts", "export const page: string = \"y\";\n");
    let (_, _, v) = repo.verified(None).unwrap();
    assert_eq!(v.check, "tsc --noEmit -p . && tsc --noEmit -p web");
    assert!(v.probes.iter().all(|p| p.caught), "{:?}", v.probes);
    assert!(v.unverified.is_empty());

    // And the sub-project is really type-checked: its broken change is the one left out,
    // with the compiler's error quoted (D40), while src/a.ts still commits.
    repo.write("web/page.ts", "export const page: string = 3;\n");
    let (changes, plan, v) = repo.verified(None).unwrap();
    assert_eq!(plan.leave, common::atoms_in(&changes, "web/page.ts"));
    assert_eq!(plan.groups, vec![common::atoms_in(&changes, "src/a.ts")]);
    let failing = v.failing.expect("rescued");
    assert!(failing.errors[0].contains("TS2322"), "{:?}", failing.errors);
}

/// D27: a file no project reads is reported as unverified; the rest is still proven.
#[test]
fn files_outside_every_project_are_unverified_not_refused() {
    let repo = TestRepo::new();
    repo.write("tsconfig.json", "{\n  \"compilerOptions\": { \"strict\": true, \"noEmit\": true },\n  \"include\": [\"src\"]\n}\n");
    repo.write("src/a.ts", "export const a = 1;\n");
    repo.write("scripts/tool.ts", "export const tool = 1;\n");
    repo.write(".gitignore", "node_modules/\n");
    repo.commit_all("base");
    if !repo.link_typescript() {
        return;
    }
    repo.write("src/a.ts", "export const a = 2;\n");
    repo.write("scripts/tool.ts", "export const tool = 2;\n");
    let (_, plan, v) = repo.verified(None).unwrap();
    assert!(!plan.groups.is_empty());
    assert_eq!(v.unverified, vec!["scripts/tool.ts"]);
    assert!(
        v.probes.iter().any(|p| p.caught && p.path == "src/a.ts"),
        "{:?}",
        v.probes
    );
}

/// A check like a compiler's: fails when any file says BROKEN, naming each location as
/// `path(line,col): error ...` (DECISIONS.md D40).
const BROKEN_CHECK: &str = "out=$(grep -n BROKEN a.txt b.txt c.txt 2>/dev/null); \
    test -z \"$out\" || { echo \"$out\" | sed 's/^\\([^:]*\\):\\([0-9]*\\):.*/\\1(\\2,1): error: broken/'; exit 1; }";

#[test]
fn a_failing_tree_commits_what_passes_and_leaves_the_rest() {
    let repo = three_files();
    repo.write("b.txt", "BROKEN\n");
    let (changes, plan, v) = repo.verified(Some(BROKEN_CHECK)).unwrap();
    let b = common::atoms_in(&changes, "b.txt");
    assert_eq!(plan.groups.len(), 2, "a and c commit: {:?}", plan.groups);
    assert_eq!(plan.leave, b);
    assert!(plan.tags[&b[0]].contains(&git_carve::graph::Tag::FailsCheck));
    let failing = v.failing.expect("rescued");
    assert_eq!(failing.left, b);
    assert_eq!(failing.errors, vec!["b.txt(1,1): error: broken".to_owned()]);
}

#[test]
fn a_failing_tree_without_locations_is_rescued_by_search() {
    let repo = three_files();
    repo.write("b.txt", "BROKEN\n");
    // Fails with no location in its output: the compiler cannot point, so carve searches.
    let (changes, plan, v) = repo
        .verified(Some("! grep -q BROKEN b.txt 2>/dev/null"))
        .unwrap();
    assert_eq!(plan.leave, common::atoms_in(&changes, "b.txt"));
    assert_eq!(plan.groups.len(), 2);
    assert!(v.failing.is_some());
}

#[test]
fn a_rescued_carve_commits_only_passing_work_and_undoes_exactly() {
    let repo = three_files();
    repo.write("b.txt", "BROKEN\n");
    let (state, files) = (repo.observable(), repo.worktree_digest());
    let plan_out = repo
        .carve()
        .args(["--plan-only", "--check", BROKEN_CHECK])
        .output()
        .unwrap();
    assert!(plan_out.status.success(), "{}", stderr(&plan_out));
    let text = String::from_utf8_lossy(&plan_out.stdout).into_owned();
    assert!(
        text.contains("# Your working tree does not pass the check"),
        "{text}"
    );
    assert!(text.contains("[does not pass the check]"), "{text}");
    let mut n = 0;
    let filled: String = text
        .lines()
        .map(|l| {
            if l == "commit" {
                n += 1;
                format!("commit\nPart {n}\n")
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    let file = repo.path().join("../rescued-plan.txt");
    fs::write(&file, filled).unwrap();
    let out = repo
        .carve()
        .args(["--check", BROKEN_CHECK, "--plan-file"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("does not pass"));
    // Two commits, each passing; b.txt's broken edit is still only in the working tree.
    assert_eq!(repo.run(&["rev-list", "--count", "HEAD"]), "3");
    assert_eq!(repo.run(&["show", "HEAD:b.txt"]), "base");
    assert_eq!(repo.worktree_digest(), files, "working tree untouched");
    for rev in ["HEAD~1", "HEAD"] {
        let tree = repo.run(&["rev-parse", &format!("{rev}^{{tree}}")]);
        assert!(!repo.run(&["ls-tree", "-r", &tree]).is_empty());
        assert!(
            !repo
                .run(&["show", &format!("{rev}:b.txt")])
                .contains("BROKEN")
        );
    }
    let out = repo.carve().arg("--undo").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.observable(), state);
}

#[test]
fn demo_with_a_broken_feature_commits_the_rest_with_real_tsc() {
    let repo = TestRepo::demo();
    if !repo.link_typescript() {
        return;
    }
    // A typo in the feature, below a debug line that `leave` keeps out of the checked tree:
    // the compiler's line numbers are the checked tree's, not the working tree's.
    let net = fs::read_to_string(repo.path().join("src/net.ts")).unwrap();
    repo.write("src/net.ts", net.replace("throw failure", "throw failur"));
    let (changes, plan, v) = repo.verified(None).unwrap();
    let feature: Vec<AtomId> = ["src/net.ts", "tests/net.test.ts"]
        .iter()
        .flat_map(|p| common::atoms_in(&changes, p))
        .filter(|id| !plan.tags[id].contains(&git_carve::graph::Tag::DebugOnly))
        .collect();
    assert_eq!(plan.groups.len(), 2, "the rename and the fix still commit");
    let failing = v.failing.expect("rescued");
    assert_eq!(failing.left, feature);
    assert!(
        failing.errors[0].contains("error TS"),
        "{:?}",
        failing.errors
    );
}

/// A compiler-like check for D43: every OLDBAD or BROKEN line is a tsc-style error, and the
/// check fails if there is any.
const TSC_LIKE: &str = "out=$(grep -n -e OLDBAD -e BROKEN a.txt b.txt c.txt 2>/dev/null); \
    test -z \"$out\" || { echo \"$out\" | sed 's/^\\([^:]*\\):\\([0-9]*\\):\\(.*\\)/\\1(\\2,1): error TS9999: \\3/'; exit 1; }";

fn head_with_an_old_error() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("a.txt", "base\n");
    repo.write("b.txt", "base\n");
    repo.write("c.txt", "OLDBAD\n");
    repo.commit_all("base");
    repo.write("a.txt", "alpha\n");
    repo.write("b.txt", "BROKEN\n");
    repo.write("c.txt", "OLDBAD\ncharlie\n");
    repo
}

#[test]
fn a_head_that_fails_is_refused_with_a_hint() {
    let repo = head_with_an_old_error();
    let out = repo
        .carve()
        .args(["--plan-only", "--check", TSC_LIKE])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("fails on HEAD") && err.contains("HEAD already has 1 error"),
        "{err}"
    );
    assert!(err.contains("git carve --no-new-errors"), "{err}");
}

#[test]
fn no_new_errors_commits_what_adds_none_and_leaves_the_rest() {
    let repo = head_with_an_old_error();
    let out = repo
        .carve()
        .args(["--plan-only", "--no-new-errors", "--check", TSC_LIKE])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("HEAD already fails the check with 1 error; no commit below adds a new one"),
        "{text}"
    );
    // a.txt and c.txt commit (c.txt keeps HEAD's old error, no new one); b.txt's new error is
    // left out, and the buffer quotes only that new error.
    let leave = text.split("\nleave\n").nth(1).unwrap_or_default();
    assert!(
        leave.contains("b.txt") && !leave.contains("a.txt") && !leave.contains("c.txt"),
        "{text}"
    );
    assert!(text.contains("b.txt(1,1): error TS9999: BROKEN"), "{text}");
    assert!(
        !text.contains("#   c.txt"),
        "the old error is not quoted: {text}"
    );
}

#[test]
fn no_new_errors_with_real_tsc_tolerates_errors_already_at_head() {
    let repo = TestRepo::new();
    repo.write("tsconfig.json", "{\n  \"compilerOptions\": { \"strict\": true, \"noEmit\": true },\n  \"include\": [\"src\"]\n}\n");
    // Committed with a type error, as in many real projects.
    repo.write(
        "src/old.ts",
        "export const legacy: number = \"not a number\";\n",
    );
    repo.write(
        "src/a.ts",
        "export function a(): number {\n  return 1;\n}\n",
    );
    repo.write(".gitignore", "node_modules/\n");
    repo.commit_all("base");
    if !repo.link_typescript() {
        return;
    }
    repo.write(
        "src/a.ts",
        "export function a(): number {\n  return 2;\n}\n",
    );
    let refused = repo.carve().args(["--plan-only"]).output().unwrap();
    assert_eq!(refused.status.code(), Some(3), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("--no-new-errors"),
        "{}",
        stderr(&refused)
    );
    let out = repo
        .carve()
        .args(["--plan-only", "--no-new-errors", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["verification"]["baseline_errors"], 1);
    assert_eq!(doc["groups"].as_array().unwrap().len(), 1);
    // The probe still has to catch a deliberately broken file (D1): new errors, not old ones.
    assert!(
        doc["verification"]["probes"][0]["caught"]
            .as_bool()
            .unwrap()
    );
}
