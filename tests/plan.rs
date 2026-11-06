//! M4 acceptance (SPEC §12): the plan buffer is a stable interface (snapshots), its rules
//! hold (§9.3–§9.4), and a whole `git carve` run works through a real editor invocation,
//! `--plan-file`, re-verification, and message recovery (DECISIONS.md D6, D17).

mod common;

use std::fs;

use common::{TYPE_MESSAGES, TestRepo, editor, stderr};
use git_carve::plan::{self, Parsed};

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn head(repo: &TestRepo) -> String {
    repo.run(&["rev-parse", "HEAD"])
}

// Snapshots: the buffer is a user interface; changes to it must be deliberate.

#[test]
fn snapshot_demo_buffer() {
    let repo = TestRepo::demo();
    let out = repo
        .carve()
        .args(["--plan-only", "--no-check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    insta::assert_snapshot!("demo_buffer", stdout(&out));
}

#[test]
fn snapshot_demo_buffer_with_diff() {
    let repo = TestRepo::demo();
    let out = repo
        .carve()
        .args(["--plan-only", "--no-check", "-v"])
        .output()
        .unwrap();
    insta::assert_snapshot!("demo_buffer_with_diff", stdout(&out));
}

#[test]
fn snapshot_verified_buffer_with_merge_style_and_leave() {
    let repo = TestRepo::new();
    repo.write("a.txt", "plain\n");
    repo.write("b.txt", "plain\n");
    repo.write(
        "src/s.ts",
        "export function s() {\n  return 1;\n}\n\nexport function t() {\n  return 2;\n}\n",
    );
    repo.commit_all("base");
    repo.write("a.txt", "useHelper\n");
    repo.write("b.txt", "defHelper\n");
    repo.write("src/s.ts", "export function s() {\n    return 1;\n}\n\nexport function t() {\n  debugger;\n  return 2;\n}\n");
    let check = "if grep -q useHelper a.txt; then grep -q defHelper b.txt; fi";
    let out = repo
        .carve()
        .args(["--plan-only", "--check", check])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    insta::assert_snapshot!("verified_buffer", stdout(&out));
}

// Parsing and validation (§9.3–§9.4), on a real plan.

fn demo_parts() -> (
    TestRepo,
    git_carve::atom::Changes,
    git_carve::graph::Plan,
    String,
) {
    let repo = TestRepo::demo();
    let (changes, analysis, plan) = repo.plan();
    let text = plan::render(
        &changes,
        &analysis,
        &plan,
        &plan::RenderOptions {
            verification: None,
            messages: &[],
            notes: &[],
            diff_lines: 50,
            unsure: false,
        },
    );
    (repo, changes, plan, text)
}

fn with_messages(text: &str) -> String {
    let mut n = 0;
    text.lines()
        .map(|l| {
            if l == "commit" {
                n += 1;
                format!("commit\nMessage {n}")
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn untouched_buffer_with_messages_parses_to_the_plan() {
    let (_repo, changes, plan, text) = demo_parts();
    let Parsed::Plan(edited) = plan::parse(&with_messages(&text), &changes, &plan.hard) else {
        panic!()
    };
    let (groups, leave) = edited.structure();
    assert_eq!(groups, plan.groups);
    assert_eq!(leave, plan.leave);
    assert_eq!(edited.groups[1].message, "Message 2\n");
}

#[test]
fn closing_the_editor_without_messages_aborts() {
    let (_repo, changes, plan, text) = demo_parts();
    assert!(
        matches!(plan::parse(&text, &changes, &plan.hard), Parsed::Abort(m) if m.contains("empty message"))
    );
    assert!(
        matches!(plan::parse("# only comments\n\n", &changes, &plan.hard), Parsed::Abort(m) if m.contains("empty"))
    );
}

#[test]
fn deleting_a_commit_line_merges_into_the_block_above() {
    let (_repo, changes, plan, text) = demo_parts();
    let text = with_messages(&text);
    let second = text.find("\ncommit\nMessage 2").unwrap();
    let merged = format!(
        "{}{}",
        &text[..second],
        text[second..].replacen("\ncommit\nMessage 2", "\n", 1)
    );
    let Parsed::Plan(edited) = plan::parse(&merged, &changes, &plan.hard) else {
        panic!()
    };
    assert_eq!(edited.groups.len(), 2);
    assert_eq!(
        edited.groups[0].atoms.len(),
        plan.groups[0].len() + plan.groups[1].len()
    );
}

#[test]
fn invalid_edits_are_errors() {
    let (_repo, changes, plan, text) = demo_parts();
    let text = with_messages(&text);
    let errors = |t: &str| match plan::parse(t, &changes, &plan.hard) {
        Parsed::Errors(e) => e.join("\n"),
        other => panic!("expected errors, got {other:?}"),
    };
    assert!(errors(&text.replacen("  @ a1 ", "  @ a999 ", 1)).contains("unknown atom a999"));
    assert!(errors(&text.replacen("  @ a2 ", "  @ a1 ", 1)).contains("a1 is also on line"));
    assert!(errors(&format!("stray text\n{text}")).contains("before the first"));
    assert!(
        errors(&text.replacen("\nleave\n", "\nleave\nnot an atom\n", 1))
            .contains("only \"@\" lines")
    );
    let with_empty = text.replacen("\nleave\n", "\ncommit\nNothing here\n\nleave\n", 1);
    assert!(errors(&with_empty).contains("has no \"@\" lines"));
    // Below the scissors line, everything is ignored (D16).
    assert!(matches!(
        plan::parse(&format!("{text}\ncommit\nIgnored\n"), &changes, &plan.hard),
        Parsed::Plan(_)
    ));
}

#[test]
fn splitting_a_hard_bind_is_an_error() {
    let repo = TestRepo::new();
    repo.write("old.ts", "export const x = 1;\nexport const y = 2;\n");
    repo.write("other.ts", "export const z = 1;\n");
    repo.commit_all("base");
    repo.remove("old.ts");
    repo.write("new.ts", "export const x = 1;\nexport const y = 2;\n");
    repo.write("other.ts", "export const z = 2;\n");
    let (changes, _, plan) = repo.plan();
    let (a, b, _) = plan.hard[0];
    let text = format!("commit\nOne\n  @ {a}\n  @ a3\ncommit\nTwo\n  @ {b}\n");
    match plan::parse(&text, &changes, &plan.hard) {
        Parsed::Errors(e) => assert!(
            e.join("\n")
                .contains(&format!("{a} and {b} must be in the same commit: rename")),
            "{e:?}"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn missing_atoms_stay_in_the_working_tree() {
    let (_repo, changes, plan, text) = demo_parts();
    let text = with_messages(&text);
    let dropped = text
        .lines()
        .filter(|l| !l.starts_with("  @ a8 "))
        .collect::<Vec<_>>()
        .join("\n");
    let dropped = dropped.replacen("commit\nMessage 3\n\n", "", 1);
    let Parsed::Plan(edited) = plan::parse(&dropped, &changes, &plan.hard) else {
        panic!("{dropped}")
    };
    assert_eq!(edited.missing, vec![git_carve::atom::AtomId(8)]);
}

// Whole runs through a real editor invocation.

#[test]
fn full_run_commits_the_users_messages() {
    let repo = TestRepo::demo();
    let (_dir, ed) = editor(TYPE_MESSAGES);
    let out = repo
        .carve()
        .env("GIT_EDITOR", &ed)
        .arg("--no-check")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with("Created 3 commits on main. They are NOT verified (--no-check):"),
        "{}",
        stdout(&out)
    );
    assert!(stdout(&out).contains("Left in working tree: 2 hunks (src/main.ts:6, src/net.ts:13)"));
    let subjects = repo.run(&["log", "--format=%s", "-3"]);
    assert_eq!(
        subjects,
        "Commit 3 subject\nCommit 2 subject\nCommit 1 subject"
    );
    assert_eq!(
        repo.run(&["status", "--porcelain"]),
        " M src/main.ts\n M src/net.ts"
    );
}

#[test]
fn multi_line_messages_keep_their_body() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit_all("base");
    repo.write("a.txt", "b\n");
    let (_dir, ed) = editor(
        r#"sed -i 's/^commit$/commit\nSubject line\n\nBody paragraph,\nsecond line./' "$1""#,
    );
    let out = repo
        .carve()
        .env("GIT_EDITOR", &ed)
        .arg("--no-check")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        repo.run(&["log", "-1", "--format=%B"]),
        "Subject line\n\nBody paragraph,\nsecond line.\n"
    );
}

#[test]
fn cancelling_changes_nothing() {
    for (name, body, reason) in [
        ("no messages", "true", "empty message"),
        ("editor fails", "exit 1", "editor"),
        ("buffer emptied", ": > \"$1\"", "plan is empty"),
    ] {
        let repo = TestRepo::demo();
        let before = (head(&repo), repo.worktree_digest());
        let (_dir, ed) = editor(body);
        let out = repo
            .carve()
            .env("GIT_EDITOR", &ed)
            .arg("--no-check")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr(&out));
        assert!(stderr(&out).contains(reason), "{name}: {}", stderr(&out));
        assert_eq!((head(&repo), repo.worktree_digest()), before, "{name}");
    }
}

#[test]
fn errors_reopen_the_editor_then_give_up() {
    let repo = TestRepo::demo();
    let body = format!(
        "cp \"$1\" \"$STATE/seen.$(ls $STATE | grep -c seen)\"\n{TYPE_MESSAGES}\nsed -i 's/^  @ a2 /  @ a1 /' \"$1\""
    );
    let (dir, ed) = editor(&body);
    let before = head(&repo);
    let out = repo
        .carve()
        .env("GIT_EDITOR", &ed)
        .arg("--no-check")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("after 3 attempts"));
    assert_eq!(head(&repo), before);
    let second = fs::read_to_string(dir.path().join("seen.1")).unwrap();
    assert!(second.starts_with("# ERROR: line"), "{second}");
    assert!(second.contains("a1 is also on line"));
}

#[test]
fn a_regroup_that_fails_the_check_reopens_then_succeeds() {
    let repo = TestRepo::new();
    repo.write("a.txt", "plain\n");
    repo.write("b.txt", "plain\n");
    repo.commit_all("base");
    repo.write("a.txt", "useHelper\n");
    repo.write("b.txt", "defHelper\n");
    let check = "if grep -q useHelper a.txt; then grep -q defHelper b.txt; fi";
    // First open: split the merged commit in two (a.txt first). Reopened with the failure
    // and the user's split kept (§9.5): follow the hint and delete the added commit line.
    let body = format!(
        "n=$(ls $STATE | grep -c seen); cp \"$1\" \"$STATE/seen.$n\"\n\
         if [ \"$n\" = 0 ]; then\n  sed -i 's/^  @ a2 /commit\\nSplit off\\n  @ a2 /' \"$1\"\n\
         else\n  sed -i '/^commit$/{{N;/\\nSplit off$/d}}' \"$1\"\nfi\n{TYPE_MESSAGES}"
    );
    let (dir, ed) = editor(&body);
    let out = repo
        .carve()
        .env("GIT_EDITOR", &ed)
        .args(["--check", check])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let second = fs::read_to_string(dir.path().join("seen.1")).unwrap();
    assert!(second.starts_with("# CHECK FAILED at commit 1"), "{second}");
    assert!(second.contains("# hint: delete the \"commit\" line after commit 1"));
    assert!(
        second.contains("Split off"),
        "the user's edit is kept on reopen"
    );
    assert_eq!(
        repo.run(&["rev-list", "--count", "HEAD"]),
        "2",
        "base + one merged commit"
    );
}

#[test]
fn plan_file_runs_without_an_editor() {
    let repo = TestRepo::demo();
    // D49: messages in a plan file need the repository's opt-in.
    let plan_out = repo
        .carve()
        .args(["--plan-only", "--no-check"])
        .output()
        .unwrap();
    let file = repo.path().join("../plan.txt");
    fs::write(&file, with_messages(&stdout(&plan_out))).unwrap();
    let out = repo
        .carve()
        .args(["--no-check", "--plan-file"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.run(&["rev-list", "--count", "HEAD"]), "4");
    // Auditable: the undo record, the reflog and the summary say where the messages came from.
    let undo = repo.run(&["rev-parse", "--git-path", "carve/undo.json"]);
    let undo: serde_json::Value =
        serde_json::from_slice(&fs::read(repo.path().join(undo.trim())).unwrap()).unwrap();
    assert_eq!(undo["plan_source"], "plan-file");
    assert!(
        repo.run(&["reflog", "-1", "--format=%gs"])
            .ends_with("(plan file)")
    );
    assert!(stdout(&out).contains("Their messages came from the plan file"));

    let bad = TestRepo::demo();
    fs::write(&file, "commit\nOnly a message\n  @ a999\n").unwrap();
    let out = bad
        .carve()
        .args(["--no-check", "--plan-file"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("unknown atom a999"));
}

/// D58 (D49's opt-in): under `carve.requireHumanMessages`, a plan file's messages are refused
/// (nothing is committed), and `--edit` opens the editor on the plan so the human writes them.
#[test]
fn require_human_messages_refuses_filled_plan_files_without_the_editor() {
    let repo = TestRepo::demo();
    repo.run(&["config", "carve.requireHumanMessages", "true"]);
    let head = repo.run(&["rev-parse", "HEAD"]);
    let plan_out = repo
        .carve()
        .args(["--plan-only", "--no-check"])
        .output()
        .unwrap();
    let file = repo.path().join("../agent-plan.txt");
    fs::write(&file, with_messages(&stdout(&plan_out))).unwrap();
    let out = repo
        .carve()
        .args(["--no-check", "--plan-file"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("--edit"), "{}", stderr(&out));
    assert_eq!(repo.run(&["rev-parse", "HEAD"]), head, "nothing committed");

    // Filled messages seed the editor only as proposals: saving untouched aborts.
    let out = repo
        .carve()
        .env("GIT_EDITOR", "true")
        .args(["--no-check", "--edit", "--plan-file"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(repo.run(&["rev-parse", "HEAD"]), head, "nothing committed");

    // Messages below the atom lines, with CRLF endings, are proposals too (review of
    // D58): saving untouched still aborts.
    // Every commit block gets its message after its atoms; `leave` gets none.
    let mut below = String::new();
    let mut in_commit = false;
    for line in stdout(&plan_out).lines() {
        if (line == "commit" || line == "leave") && in_commit {
            below.push_str("Agent wrote this\r\n");
        }
        if line == "commit" || line == "leave" {
            in_commit = line == "commit";
        }
        below.push_str(line);
        below.push_str("\r\n");
    }
    if in_commit {
        below.push_str("Agent wrote this\r\n");
    }
    fs::write(&file, &below).unwrap();
    let out = repo
        .carve()
        .env("GIT_EDITOR", "true")
        .args(["--no-check", "--edit", "--plan-file"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(repo.run(&["rev-parse", "HEAD"]), head, "nothing committed");

    // The agent's way: proposals as comments; the human's editor writes the messages.
    let proposed = stdout(&plan_out).replace("commit\n", "commit\n# proposed: an agent's idea\n");
    fs::write(&file, proposed).unwrap();
    let (_dir, ed) = editor(TYPE_MESSAGES);
    let out = repo
        .carve()
        .env("GIT_EDITOR", &ed)
        .args(["--no-check", "--edit", "--plan-file"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(repo.run(&["rev-list", "--count", "HEAD"]), "4");
    let subjects = repo.run(&["log", "--format=%s", "-3"]);
    assert!(!subjects.contains("agent"), "{subjects}");
    let undo = repo.run(&["rev-parse", "--git-path", "carve/undo.json"]);
    let undo: serde_json::Value =
        serde_json::from_slice(&fs::read(repo.path().join(undo.trim())).unwrap()).unwrap();
    assert_eq!(undo["plan_source"], "plan-file+edit");
    assert!(
        !repo
            .run(&["reflog", "-1", "--format=%gs"])
            .contains("(plan file)")
    );
}

/// D50: `--plan-only --json` carries the buffer `--plan-only` prints, byte for byte.
#[test]
fn plan_json_carries_the_buffer() {
    let repo = TestRepo::demo();
    let text = repo
        .carve()
        .args(["--plan-only", "--no-check"])
        .output()
        .unwrap();
    let json = repo
        .carve()
        .args(["--plan-only", "--no-check", "--json"])
        .output()
        .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["buffer"].as_str().unwrap(), stdout(&text));
}

#[test]
fn messages_survive_an_aborted_run() {
    let repo = TestRepo::demo();
    // Types messages, then "edits a file" while the editor is open (touch changes its
    // mtime, not its content): the race guard aborts the run with exit 4.
    let (_d1, typing) = editor(&format!("{TYPE_MESSAGES}\ntouch src/format.ts"));
    let out = repo
        .carve()
        .env("GIT_EDITOR", &typing)
        .arg("--no-check")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(repo.path().join(".git/carve/PLAN.saved").exists());

    // Next run: an editor that types nothing. Every message comes back.
    let (_d2, idle) = editor("true");
    let out = repo
        .carve()
        .env("GIT_EDITOR", &idle)
        .arg("--no-check")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        repo.run(&["log", "--format=%s", "-3"]),
        "Commit 3 subject\nCommit 2 subject\nCommit 1 subject"
    );
    assert!(!repo.path().join(".git/carve/PLAN.saved").exists());
}
