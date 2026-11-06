//! `git carve <base> --stack` (D46 phase 2, D48, docs/design/stack.md).

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Output;

use common::{TYPE_MESSAGES, TestRepo, editor, per_file, stderr};
use git_carve::Error;
use git_carve::atom::atomize;
use git_carve::commit::{Group, Hooks, Layer, UndoRecord, commit_stacked};
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
    let layers: Vec<Layer> = (1..groups.len())
        .map(|k| Layer {
            name: format!("feat-{k}"),
            was: None,
        })
        .collect();
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
    for layer in &layers {
        assert!(
            branch_tip(&repo, &layer.name).is_none(),
            "{} left behind",
            layer.name
        );
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

/// A carved stack whose layer branches were renamed, as if their names came from other
/// subjects: a re-carve must move these, not create `feat-1-commit-1-subject`.
fn reviewed_stack() -> (TestRepo, String, String) {
    let (repo, _) = feature_branch();
    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    repo.run(&["branch", "-m", "feat-1-commit-1-subject", "feat-1-reviewed"]);
    repo.run(&["branch", "-m", "feat-2-commit-2-subject", "feat-2-reviewed"]);
    let one = branch_tip(&repo, "feat-1-reviewed").unwrap();
    let two = branch_tip(&repo, "feat-2-reviewed").unwrap();
    (repo, one, two)
}

#[test]
fn stack_recarve_adopts_existing_layers_and_undo_moves_them_back() {
    let (repo, one, two) = reviewed_stack();
    // A review fix, uncommitted.
    let format = fs::read_to_string(repo.path().join("src/format.ts")).unwrap();
    repo.write("src/format.ts", format!("{format}// reviewed\n"));
    let before = repo.observable();
    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = out_str(&out);
    assert_eq!(
        branch_tip(&repo, "feat-1-reviewed").as_deref(),
        Some(repo.run(&["rev-parse", "feat~2"]).as_str())
    );
    assert_eq!(
        branch_tip(&repo, "feat-2-reviewed").as_deref(),
        Some(repo.run(&["rev-parse", "feat~1"]).as_str())
    );
    assert_ne!(branch_tip(&repo, "feat-1-reviewed").unwrap(), one);
    assert!(branch_tip(&repo, "feat-1-commit-1-subject").is_none());
    assert!(text.contains("2 of 3 layers kept their branch"), "{text}");
    assert!(text.contains("--force-with-lease") || text.contains("git push -u <remote>"));

    let out = repo.carve().arg("--undo").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(branch_tip(&repo, "feat-1-reviewed"), Some(one));
    assert_eq!(branch_tip(&repo, "feat-2-reviewed"), Some(two));
    assert_eq!(repo.observable(), before);
}

#[test]
fn stack_recarve_buffer_names_the_adopted_layers() {
    let (repo, _, _) = reviewed_stack();
    let out = repo
        .carve()
        .args(["main", "--stack", "--no-check"])
        .env("GIT_EDITOR", "cat")
        .output()
        .unwrap();
    // `cat` leaves the messages empty, so nothing is committed; the buffer is on stdout.
    let text = out_str(&out);
    assert!(
        text.contains("# stack: commits 1-2 move feat-1-reviewed, feat-2-reviewed"),
        "{text}{}",
        stderr(&out)
    );
}

#[test]
fn stack_recarve_refuses_a_layer_checked_out_elsewhere() {
    let (repo, one, _) = reviewed_stack();
    let other = tempfile::tempdir().unwrap();
    let wt = other.path().join("review");
    repo.run(&[
        "worktree",
        "add",
        "-q",
        wt.to_str().unwrap(),
        "feat-1-reviewed",
    ]);
    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], None);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("feat-1-reviewed is checked out in another worktree"),
        "{}",
        stderr(&out)
    );
    assert_eq!(branch_tip(&repo, "feat-1-reviewed"), Some(one));
}

#[test]
fn stack_recarve_refuses_two_branches_for_one_layer() {
    let (repo, _, two) = reviewed_stack();
    repo.run(&["branch", "feat-2-other", &two]);
    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], None);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("both claim layer 2"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn safety_stack_failed_adoption_moves_earlier_layers_back_never_deletes() {
    let (repo, base) = feature_branch();
    let one = repo.run(&["rev-parse", "feat~2"]);
    let two = repo.run(&["rev-parse", "feat~1"]);
    repo.run(&["branch", "feat-1-pr", &one]);
    repo.run(&["branch", "feat-2-pr", &two]);
    let r = repo.open();
    let snap = snapshot::take_since(&r, Some(&base)).unwrap();
    let changes = atomize(&r, &snap).unwrap();
    let groups: Vec<Group> = per_file(&changes)
        .into_iter()
        .take(3)
        .enumerate()
        .map(|(k, atoms)| Group {
            atoms,
            message: format!("Commit {}\n", k + 1),
        })
        .collect();
    let mut all = per_file(&changes);
    let rest: Vec<_> = all.drain(3..).flatten().collect();
    let mut groups = groups;
    groups.last_mut().unwrap().atoms.extend(rest);
    // Layer 2 moved after carve read it (someone ran `gh stack modify`): the CAS fails.
    let layers = vec![
        Layer {
            name: "feat-1-pr".into(),
            was: Some(one.clone()),
        },
        Layer {
            name: "feat-2-pr".into(),
            was: Some(base.clone()),
        },
    ];
    let err = commit_stacked(
        &r,
        &snap,
        &changes,
        &groups,
        &Hooks::default(),
        &layers,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, Error::Concurrent(_)), "{err}");
    assert_eq!(branch_tip(&repo, "feat-1-pr"), Some(one), "moved back");
    assert_eq!(branch_tip(&repo, "feat-2-pr"), Some(two), "untouched");
    let journal = repo.run(&["rev-parse", "--git-path", "carve/layers.journal"]);
    assert!(!repo.path().join(journal).exists());
}

#[test]
fn safety_stack_interrupted_adoption_is_recovered_from_the_journal() {
    let (repo, one, _) = reviewed_stack();
    // As if carve died after moving the layer: the branch is elsewhere, the journal says where
    // it was.
    let moved = repo.run(&["rev-parse", "main"]);
    repo.run(&["update-ref", "refs/heads/feat-1-reviewed", &moved]);
    let journal = serde_json::json!([
        {"ref": "refs/heads/feat-1-reviewed", "oid": moved, "was": one},
    ]);
    let path = repo.run(&["rev-parse", "--git-path", "carve/layers.journal"]);
    fs::write(repo.path().join(&path), journal.to_string()).unwrap();
    let format = fs::read_to_string(repo.path().join("src/format.ts")).unwrap();
    repo.write("src/format.ts", format!("{format}// reviewed\n"));
    let out = carve_stack(&repo, &["main", "--stack", "--no-check"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!repo.path().join(&path).exists());
    assert_eq!(
        branch_tip(&repo, "feat-1-reviewed").as_deref(),
        Some(repo.run(&["rev-parse", "feat~2"]).as_str()),
        "recovered, then adopted"
    );
}

#[test]
fn stack_recarve_of_pushed_layers_prints_leased_pushes_and_never_submits() {
    let (repo, one, _) = reviewed_stack();
    let _remote = with_remote(&repo);
    repo.run(&[
        "push",
        "-q",
        "origin",
        "feat-1-reviewed",
        "feat-2-reviewed",
        "feat",
    ]);
    let format = fs::read_to_string(repo.path().join("src/format.ts")).unwrap();
    repo.write("src/format.ts", format!("{format}// reviewed\n"));
    let (_gh, path) = fake_gh(true);
    let out = carve_stack(
        &repo,
        &[
            "main",
            "--stack",
            "--check",
            "true",
            "--rewrite-pushed",
            "--submit",
        ],
        Some(&path),
    );
    assert_eq!(out.status.code(), Some(6), "{}", stderr(&out));
    let text = out_str(&out);
    assert!(
        text.contains(&format!("--force-with-lease=feat-1-reviewed:{one}")),
        "{text}"
    );
    assert!(stderr(&out).contains("does not resubmit an adopted stack"));
    let log = fs::read_to_string(_gh.path().join("gh.log")).unwrap_or_default();
    assert!(!log.contains("stack submit"), "{log}");
    assert_eq!(
        branch_tip(&repo, "feat-1-reviewed").as_deref(),
        Some(repo.run(&["rev-parse", "feat~2"]).as_str())
    );
}

/// D58: the unattended agent flow. An agent takes the plan buffer from `--plan-only --json`,
/// fills the messages, and hands it back with `--plan-file`: no editor, verified layers, and
/// the run says where the messages came from.
#[test]
fn an_agent_fills_the_plan_and_carve_builds_the_stack_without_an_editor() {
    let (repo, _) = feature_branch();
    let out = repo
        .carve()
        .args(["main", "--stack", "--plan-only", "--json", "--no-check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut n = 0;
    let filled: Vec<String> = doc["buffer"]
        .as_str()
        .unwrap()
        .lines()
        .map(|l| {
            if l == "commit" {
                n += 1;
                format!("commit\nAgent layer {n}")
            } else {
                l.to_owned()
            }
        })
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("plan.txt");
    fs::write(&file, filled.join("\n") + "\n").unwrap();
    let out = repo
        .carve()
        .env("GIT_EDITOR", "false")
        .args(["main", "--stack", "--no-check", "--json", "--plan-file"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["plan_source"], "plan-file", "{report}");
    assert!(branch_tip(&repo, "feat-1-agent-layer-1").is_some());
    assert!(branch_tip(&repo, "feat-2-agent-layer-2").is_some());
    assert_eq!(repo.run(&["log", "--format=%s", "-1"]), "Agent layer 3");
}

/// D59: `--layers` caps the stack by merging the smallest neighbors; the default (5) leaves a
/// small stack alone, and the buffer says what merged.
#[test]
fn layers_caps_the_stack_by_merging_the_smallest_neighbors() {
    let (repo, _) = feature_branch();
    let layers = |extra: &[&str]| -> (usize, String) {
        let out = repo
            .carve()
            .args([
                "main",
                "--stack",
                "--plan-only",
                "--json",
                "--check",
                "true",
            ])
            .args(extra)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        (
            doc["groups"].as_array().unwrap().len(),
            doc["buffer"].as_str().unwrap().to_owned(),
        )
    };
    let (default, buffer) = layers(&[]);
    assert_eq!(default, 3, "three layers fit under the default of 5");
    assert!(!buffer.contains("--layers"), "{buffer}");
    let (two, buffer) = layers(&["--layers", "2"]);
    assert_eq!(two, 2);
    assert!(
        buffer.contains("# (--layers: merged the smallest neighboring commits, 3 into 2"),
        "{buffer}"
    );
    assert_eq!(layers(&["--layers", "0"]).0, 3, "0 means no limit");
    let out = repo.carve().args(["--layers", "2"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(129),
        "--layers needs --stack (a usage error)"
    );
}

/// Seven independent changes, one commit each, on `feat` over `main`.
fn seven_file_branch() -> TestRepo {
    let repo = TestRepo::new();
    for k in 1..=7 {
        repo.write(&format!("f{k}.txt"), "base\n");
    }
    repo.commit_all("base");
    repo.run(&["branch", "-m", "feat"]);
    repo.run(&["branch", "main"]);
    for k in 1..=7 {
        repo.write(&format!("f{k}.txt"), format!("change {k}\n"));
        repo.commit_all(&format!("change {k}"));
    }
    repo
}

/// D59 review: an existing stack's shape wins over the default `--layers` cap (a
/// 7-layer stack is not squeezed to 5, so no PR is silently orphaned); an explicit cap is
/// honored, and the buffer names the layers that stay behind.
#[test]
fn recarving_a_bigger_stack_keeps_its_layers_under_the_default_cap() {
    let repo = seven_file_branch();
    let out = carve_stack(
        &repo,
        &["main", "--stack", "--layers", "0", "--check", "true"],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(branch_tip(&repo, "feat-6-commit-6-subject").is_some());
    let buffer = |extra: &[&str]| -> String {
        let out = repo
            .carve()
            .env("GIT_EDITOR", "cat")
            .args(["main", "--stack", "--check", "true"])
            .args(extra)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let default = buffer(&[]);
    assert!(
        default.contains("# stack: commits 1-6 move feat-1-commit-1-subject"),
        "{default}"
    );
    assert!(!default.contains("stay where they are"), "{default}");
    let plan = repo
        .carve()
        .args([
            "main",
            "--stack",
            "--plan-only",
            "--json",
            "--check",
            "true",
        ])
        .output()
        .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(
        doc["groups"].as_array().unwrap().len(),
        7,
        "plan-only agrees"
    );
    let capped = buffer(&["--layers", "3"]);
    assert!(
        capped.contains("commits 1-2 move feat-1-commit-1-subject, feat-2-commit-2-subject"),
        "{capped}"
    );
    assert!(
        capped.contains("stay where they are (close their PRs)"),
        "{capped}"
    );
}

/// D53 review: under default tolerance the stack summary says "no new error", never
/// "passed" (non-negotiable 3: the summary says which).
#[test]
fn a_stack_over_a_failing_base_says_no_new_error_not_passed() {
    let repo = TestRepo::new();
    for f in ["a.txt", "b.txt"] {
        repo.write(f, "base\n");
    }
    repo.write("c.txt", "OLDBAD\n");
    repo.commit_all("base with an old error");
    repo.run(&["branch", "-m", "feat"]);
    repo.run(&["branch", "main"]);
    repo.write("a.txt", "alpha\n");
    repo.commit_all("a");
    repo.write("b.txt", "beta\n");
    repo.commit_all("b");
    let check = "out=$(grep -n OLDBAD a.txt b.txt c.txt 2>/dev/null); test -z \"$out\" || { echo \"$out\" | sed 's/^\\([^:]*\\):\\([0-9]*\\):\\(.*\\)/\\1(\\2,1): error TS9999: \\3/'; exit 1; }";
    let out = carve_stack(&repo, &["main", "--stack", "--check", check], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = out_str(&out);
    assert!(
        text.contains("each adds no error beyond the base's 1"),
        "{text}"
    );
    assert!(!text.contains("each passed"), "{text}");
}
