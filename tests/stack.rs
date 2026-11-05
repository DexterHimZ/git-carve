//! `git carve <base> --stack` (D46 phase 2, D48, docs/design/stack.md).

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Output;

use common::{TYPE_MESSAGES, TestRepo, editor, per_file, stderr};
use git_carve::Error;
use git_carve::atom::atomize;
use git_carve::commit::{Group, Hooks, UndoRecord, commit_stacked};
use git_carve::snapshot;

/// The SPEC §14.1 demo mess committed as three wip commits on branch `feat`, with `main`
/// left at the base. Returns (repo, base oid).
fn feature_branch() -> (TestRepo, String) {
    let repo = TestRepo::demo();
    let base = repo.run(&["rev-parse", "HEAD"]);
    repo.run(&["branch", "-m", "feat"]);
    repo.run(&["branch", "main", &base]);
    repo.run(&["add", "src/format.ts", "src/main.ts", "src/progress.ts"]);
    repo.run(&["commit", "-q", "-m", "wip"]);
    repo.run(&["add", "src/net.ts", "tests/net.test.ts", "src/parse.ts"]);
    repo.run(&["commit", "-q", "-m", "asdf"]);
    repo.run(&["add", "-A"]);
    repo.run(&["commit", "-q", "-m", "fix stuff"]);
    (repo, base)
}

fn out_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A `gh` stand-in on PATH: `extension list` names gh-stack when `with_stack`; every call is
/// logged to `$dir/gh.log`.
fn fake_gh(with_stack: bool) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("gh");
    let listing = if with_stack {
        "gh stack\tgithub/gh-stack\tv1.2.0"
    } else {
        ""
    };
    fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$*\" >> {log}\nif [ \"$1 $2\" = \"extension list\" ]; then echo \"{listing}\"; fi\nif [ \"$1 $2\" = \"stack submit\" ] && [ \"$GH_STUB_FAIL\" = submit ]; then echo 'not authenticated' >&2; exit 1; fi\nexit 0\n",
            log = dir.path().join("gh.log").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap()
    );
    (dir, path)
}

fn carve_stack(repo: &TestRepo, args: &[&str], path: Option<&str>) -> Output {
    let (_dir, ed) = editor(TYPE_MESSAGES);
    let mut cmd = repo.carve();
    cmd.env("GIT_EDITOR", &ed).args(args);
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    cmd.output().unwrap()
}

fn branch_tip(repo: &TestRepo, name: &str) -> Option<String> {
    repo.git()
        .try_run_str(["rev-parse", "-q", "--verify", &format!("refs/heads/{name}")])
        .unwrap()
}

#[test]
fn stack_creates_one_branch_per_commit_but_the_top() {
    let (repo, base) = feature_branch();
    let digest = repo.worktree_digest();
    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = out_str(&out);
    assert_eq!(repo.run(&["rev-parse", "feat~3"]), base);
    assert_eq!(
        branch_tip(&repo, "feat-1-commit-1-subject").as_deref(),
        Some(repo.run(&["rev-parse", "feat~2"]).as_str())
    );
    assert_eq!(
        branch_tip(&repo, "feat-2-commit-2-subject").as_deref(),
        Some(repo.run(&["rev-parse", "feat~1"]).as_str())
    );
    // Layer k's tree is carved prefix k: one commit over the layer below.
    assert_eq!(
        repo.run(&[
            "rev-list",
            "--count",
            "feat-1-commit-1-subject..feat-2-commit-2-subject"
        ]),
        "1"
    );
    assert!(
        text.contains("Stack on main (3 layers, NOT verified"),
        "{text}"
    );
    assert!(text.contains("(top)"), "{text}");
    assert_eq!(repo.worktree_digest(), digest);
    assert_eq!(repo.run(&["symbolic-ref", "--short", "HEAD"]), "feat");
}

#[test]
fn stack_single_commit_creates_no_branch() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    let base = repo.commit_all("base");
    repo.write("a.txt", "a2\n");
    repo.commit_all("wip");
    let out = carve_stack(&repo, &[&base, "--stack", "--no-check"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(out_str(&out).contains("(1 layers"), "{}", out_str(&out));
    assert_eq!(
        repo.run(&["for-each-ref", "--format=%(refname:short)", "refs/heads"]),
        "main"
    );
}

#[test]
fn stack_refuses_detached_head() {
    let (repo, _) = feature_branch();
    repo.run(&["checkout", "-q", "--detach"]);
    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], None);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("a stack needs a branch"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn stack_requires_base() {
    let (repo, _) = feature_branch();
    let out = repo.carve().args(["--stack"]).output().unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("BASE"), "{}", stderr(&out));
}

#[test]
fn stack_refuses_existing_branch_name_before_editor() {
    let (repo, _) = feature_branch();
    repo.run(&["branch", "feat-1-old-layer", "main"]);
    let (dir, ed) = editor(&format!("touch \"$STATE/editor-ran\"\n{TYPE_MESSAGES}"));
    let out = repo
        .carve()
        .env("GIT_EDITOR", &ed)
        .args(["main", "--stack", "--no-check"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("feat-1-old-layer"),
        "{}",
        stderr(&out)
    );
    assert!(
        !dir.path().join("editor-ran").exists(),
        "refused before the editor"
    );
}

#[test]
fn safety_stack_undo_deletes_only_unmoved_layers() {
    let (repo, _) = feature_branch();
    let before = repo.observable();
    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    repo.run(&["branch", "-f", "feat-1-commit-1-subject", "main"]);
    let out = repo.carve().arg("--undo").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        out_str(&out).contains("Kept feat-1-commit-1-subject: it moved since the carve."),
        "{}",
        out_str(&out)
    );
    assert!(branch_tip(&repo, "feat-2-commit-2-subject").is_none());
    assert!(branch_tip(&repo, "feat-1-commit-1-subject").is_some());
    repo.run(&["branch", "-D", "feat-1-commit-1-subject"]);
    assert_eq!(repo.observable(), before);
}

#[test]
fn safety_stack_cas_race_leaves_no_layer_branch() {
    let (repo, base) = feature_branch();
    let r = repo.open();
    let snap = snapshot::take_since(&r, Some(&base)).unwrap();
    let changes = atomize(&r, &snap).unwrap();
    let groups: Vec<Group> = per_file(&changes)
        .into_iter()
        .enumerate()
        .map(|(k, atoms)| Group {
            atoms,
            message: format!("Commit {}\n", k + 1),
        })
        .collect();
    let layers: Vec<String> = (1..groups.len()).map(|k| format!("feat-{k}")).collect();
    let racer = repo.run(&["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "racer"]);
    let move_ref = || {
        repo.run(&["update-ref", "refs/heads/feat", &racer]);
    };
    let hooks = Hooks {
        before_ref_update: Some(&move_ref),
        ..Hooks::default()
    };
    let err = commit_stacked(&r, &snap, &changes, &groups, &hooks, &layers, None).unwrap_err();
    assert!(matches!(err, Error::Concurrent(_)), "{err}");
    for name in &layers {
        assert!(branch_tip(&repo, name).is_none(), "{name} left behind");
    }
    assert_eq!(repo.run(&["rev-parse", "feat"]), racer);
}

#[test]
fn stack_undo_record_without_stack_still_parses() {
    let json = r#"{"version":1,"ref":"refs/heads/main","detached":false,"old":"a","new":"b",
        "commits":["b"],"left":[],"created_at":"2026-10-13T00:00:00Z"}"#;
    let record: UndoRecord = serde_json::from_str(json).unwrap();
    assert!(record.stack.is_empty());
}

#[test]
fn stack_prints_plain_commands_without_extension() {
    let (repo, _) = feature_branch();
    let remote = tempfile::tempdir().unwrap();
    repo.run(&["init", "-q", "--bare", remote.path().to_str().unwrap()]);
    repo.run(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    let (_gh, path) = fake_gh(false);
    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], Some(&path));
    assert!(out.status.success(), "{}", stderr(&out));
    let text = out_str(&out);
    assert!(
        text.contains("git push -u origin feat-1-commit-1-subject feat-2-commit-2-subject feat"),
        "{text}"
    );
    assert!(
        text.contains("gh pr create --base main --head feat-1-commit-1-subject --fill"),
        "{text}"
    );
    assert!(
        text.contains("gh pr create --base feat-2-commit-2-subject --head feat --fill"),
        "{text}"
    );
    // Printing pushes nothing.
    assert_eq!(
        repo.git()
            .with_dir(remote.path())
            .run_str(["for-each-ref"])
            .unwrap(),
        ""
    );
}

#[test]
fn stack_prints_gh_stack_commands_when_extension_installed() {
    let (repo, _) = feature_branch();
    let (_gh, path) = fake_gh(true);
    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], Some(&path));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        out_str(&out).contains(
            "gh stack init --base main feat-1-commit-1-subject feat-2-commit-2-subject feat && gh stack submit --auto"
        ),
        "{}",
        out_str(&out)
    );
}

#[test]
fn stack_trunk_from_remote_tracking_base() {
    let (repo, _) = feature_branch();
    let remote = tempfile::tempdir().unwrap();
    repo.run(&["init", "-q", "--bare", remote.path().to_str().unwrap()]);
    repo.run(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    repo.run(&["push", "-q", "origin", "main"]);
    repo.run(&["branch", "-D", "main"]);
    let out = carve_stack(&repo, &["origin/main", "--stack", "--no-check"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        out_str(&out).contains("Stack on main ("),
        "{}",
        out_str(&out)
    );
}

/// D48 review: `git carve @{u} --stack` on `feat` tracking `origin/feat` must not
/// stack `feat` on itself (`gh pr create --base feat --head feat-1-…`).
#[test]
fn stack_upstream_of_the_same_branch_is_not_the_trunk() {
    let (repo, _) = feature_branch();
    repo.run(&["branch", "-D", "main"]);
    let remote = tempfile::tempdir().unwrap();
    repo.run(&["init", "-q", "--bare", remote.path().to_str().unwrap()]);
    repo.run(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    repo.run(&["push", "-q", "origin", "HEAD~2:refs/heads/feat"]);
    repo.run(&["branch", "-q", "--set-upstream-to=origin/feat"]);
    let out = carve_stack(&repo, &["@{u}", "--stack", "--no-check"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = out_str(&out);
    assert!(text.contains("Stack on <trunk> ("), "{text}");
    assert!(!text.contains("--base feat "), "{text}");
    repo.carve().arg("--undo").output().unwrap();

    let (gh, path) = fake_gh(true);
    let out = carve_stack(&repo, &["@{u}", "--stack", "--submit"], Some(&path));
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("name the base branch"),
        "{}",
        stderr(&out)
    );
    let log = fs::read_to_string(gh.path().join("gh.log")).unwrap_or_default();
    assert!(!log.contains("stack init"), "{log}");
}

/// D48 review: carve never touches remotes, so an undo after the layers were pushed
/// says they are still there, and a re-carve that reuses their names says it replaces them.
#[test]
fn stack_notes_remote_copies_on_undo_and_re_carve() {
    let (repo, _) = feature_branch();
    let remote = tempfile::tempdir().unwrap();
    repo.run(&["init", "-q", "--bare", remote.path().to_str().unwrap()]);
    repo.run(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let layers: Vec<String> = repo
        .run(&[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/feat-*",
        ])
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(!layers.is_empty());
    let mut push = vec!["push", "-q", "origin"];
    push.extend(layers.iter().map(String::as_str));
    repo.run(&push);

    let undo = repo.carve().arg("--undo").output().unwrap();
    let text = out_str(&undo);
    assert!(
        text.contains(&format!("note: origin/{}", layers[0]))
            && text.contains("still exist on the remote"),
        "{text}"
    );

    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = out_str(&out);
    assert!(
        text.contains(&format!("note: origin/{}", layers[0]))
            && text.contains("pushing the new layers replaces them"),
        "{text}"
    );
}

#[test]
fn stack_unknown_trunk_prints_placeholder() {
    let (repo, _) = feature_branch();
    repo.run(&["branch", "-D", "main"]);
    let out = carve_stack(&repo, &["HEAD~3", "--stack", "--no-check"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = out_str(&out);
    assert!(text.contains("Stack on <trunk> ("), "{text}");
    assert!(text.contains("# replace <trunk>"), "{text}");
}

#[test]
fn stack_warns_when_top_diverged_from_upstream() {
    let (repo, _) = feature_branch();
    let remote = tempfile::tempdir().unwrap();
    repo.run(&["init", "-q", "--bare", remote.path().to_str().unwrap()]);
    repo.run(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    repo.run(&["push", "-q", "-u", "origin", "feat"]);
    let out = carve_stack(
        &repo,
        &["main", "--stack", "--no-check", "--rewrite-pushed"],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        out_str(&out).contains("feat diverged from origin/feat"),
        "{}",
        out_str(&out)
    );
}

// --submit (step 5): gh is a stub that logs its arguments; nothing reaches GitHub.

fn with_remote(repo: &TestRepo) -> tempfile::TempDir {
    let remote = tempfile::tempdir().unwrap();
    repo.run(&["init", "-q", "--bare", remote.path().to_str().unwrap()]);
    repo.run(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    remote
}

#[test]
fn stack_submit_runs_init_then_submit_with_layers_bottom_to_top() {
    let (repo, _) = feature_branch();
    let remote = with_remote(&repo);
    let (gh, path) = fake_gh(true);
    let out = carve_stack(
        &repo,
        &["main", "--stack", "--submit", "--check", "true"],
        Some(&path),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let log = fs::read_to_string(gh.path().join("gh.log")).unwrap();
    let calls: Vec<&str> = log.lines().filter(|l| l.starts_with("stack ")).collect();
    assert_eq!(
        calls,
        [
            "stack init --base main feat-1-commit-1-subject feat-2-commit-2-subject feat",
            "stack submit --auto"
        ]
    );
    // carve itself pushed nothing; the (stub) gh would have.
    assert_eq!(
        repo.git()
            .with_dir(remote.path())
            .run_str(["for-each-ref"])
            .unwrap(),
        ""
    );
}

#[test]
fn stack_submit_failure_exits_6_with_retry_commands_and_keeps_commits() {
    let (repo, _) = feature_branch();
    let _remote = with_remote(&repo);
    let (_gh, path) = fake_gh(true);
    let (_dir, ed) = editor(TYPE_MESSAGES);
    let out = repo
        .carve()
        .env("GIT_EDITOR", &ed)
        .env("PATH", &path)
        .env("GH_STUB_FAIL", "submit")
        .args(["main", "--stack", "--submit", "--check", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(6), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("retry (safe to repeat)"),
        "{}",
        stderr(&out)
    );
    assert!(branch_tip(&repo, "feat-1-commit-1-subject").is_some());
    assert_eq!(repo.run(&["log", "-1", "--format=%s"]), "Commit 3 subject");
}

#[test]
fn stack_submit_preflight_refuses_without_remote_before_editor() {
    let (repo, _) = feature_branch();
    let (_gh, path) = fake_gh(true);
    let (dir, ed) = editor(&format!("touch \"$STATE/editor-ran\"\n{TYPE_MESSAGES}"));
    let out = repo
        .carve()
        .env("GIT_EDITOR", &ed)
        .env("PATH", &path)
        .args(["main", "--stack", "--submit", "--check", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("needs a remote"), "{}", stderr(&out));
    assert!(!dir.path().join("editor-ran").exists());
}

#[test]
fn stack_submit_preflight_refuses_without_the_extension() {
    let (repo, _) = feature_branch();
    let _remote = with_remote(&repo);
    let (_gh, path) = fake_gh(false);
    let out = carve_stack(
        &repo,
        &["main", "--stack", "--submit", "--check", "true"],
        Some(&path),
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("gh-stack extension"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn stack_submit_conflicts_with_no_check() {
    let (repo, _) = feature_branch();
    let out = repo
        .carve()
        .args(["main", "--stack", "--submit", "--no-check"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--no-check"), "{}", stderr(&out));
}

// JSON (step 6): the planned stack and the run report.

#[test]
fn stack_plan_only_json_lists_planned_layers() {
    let (repo, _) = feature_branch();
    let out = repo
        .carve()
        .args(["main", "--stack", "--plan-only", "--json", "--no-check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["stack"]["trunk"], "main");
    let layers = doc["stack"]["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 3);
    assert_eq!(layers[0]["prefix"], "feat-1");
    assert_eq!(layers[2]["branch"], "feat");
    assert_eq!(layers[2]["top"], true);
}

#[test]
fn stack_run_json_reports_layers_and_commands() {
    let (repo, _) = feature_branch();
    let out = carve_stack(&repo, &["main", "--stack", "--no-check", "--json"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", out_str(&out)));
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["branch"], "feat");
    assert_eq!(doc["commits"].as_array().unwrap().len(), 3);
    let layers = doc["stack"]["layers"].as_array().unwrap();
    assert_eq!(layers[0]["branch"], "feat-1-commit-1-subject");
    assert_eq!(layers[0]["subject"], "Commit 1 subject");
    assert_eq!(layers[2]["top"], true);
    assert_eq!(doc["stack"]["submit"]["ran"], false);
    assert!(doc["stack"]["submit"]["commands"].as_array().unwrap().len() >= 2);
}

#[test]
fn stack_submit_json_stays_valid_and_reports_failure() {
    let (repo, _) = feature_branch();
    let _remote = with_remote(&repo);
    let (_gh, path) = fake_gh(true);
    let (_dir, ed) = editor(TYPE_MESSAGES);
    let out = repo
        .carve()
        .env("GIT_EDITOR", &ed)
        .env("PATH", &path)
        .env("GH_STUB_FAIL", "submit")
        .args(["main", "--stack", "--submit", "--check", "true", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(6), "{}", stderr(&out));
    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", out_str(&out)));
    assert_eq!(doc["stack"]["submit"]["ran"], true);
    assert_eq!(doc["stack"]["submit"]["ok"], false);
    assert!(
        stderr(&out).contains("not authenticated"),
        "{}",
        stderr(&out)
    );
}
