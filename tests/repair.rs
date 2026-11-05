//! Error-guided repair (dev experiment `GIT_CARVE_DEV_ERROR_GUIDED`, DECISIONS.md D57):
//! a failing prefix pulls in the later group its check errors point at, instead of merging
//! forward through unrelated groups.

mod common;

use common::{TestRepo, stderr};

/// a.txt starts using X; b.txt changes for an unrelated reason; c.txt adds `def X` on line 2.
/// The check reports the missing definition where it belongs: c.txt line 2.
const CHECK: &str = "if grep -q 'use X' a.txt && ! grep -q 'def X' c.txt; then \
    echo \"c.txt(2,1): error TS2304: Cannot find name 'X'.\"; exit 1; fi";

fn repo() -> TestRepo {
    let repo = TestRepo::new();
    for f in ["a.txt", "b.txt", "c.txt"] {
        repo.write(f, "base\n");
    }
    repo.commit_all("base");
    repo.write("a.txt", "base\nuse X\n");
    repo.write("b.txt", "base\nunrelated\n");
    repo.write("c.txt", "base\ndef X\n");
    repo
}

fn groups(repo: &TestRepo, guided: bool) -> Vec<Vec<String>> {
    let mut cmd = repo.carve();
    cmd.args(["--plan-only", "--json", "--check", CHECK]);
    if guided {
        cmd.env("GIT_CARVE_DEV_ERROR_GUIDED", "1");
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let path_of = |id: &serde_json::Value| {
        doc["atoms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| &a["id"] == id)
            .map(|a| a["path"].as_str().unwrap().to_owned())
            .unwrap()
    };
    doc["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            let mut paths: Vec<String> = g.as_array().unwrap().iter().map(path_of).collect();
            paths.sort();
            paths.dedup();
            paths
        })
        .collect()
}

#[test]
fn a_failing_prefix_pulls_the_group_its_errors_point_at() {
    let repo = repo();
    let plain = groups(&repo, false);
    let guided = groups(&repo, true);
    // Merge-forward folds the unrelated b.txt in on its way to c.txt.
    assert_eq!(plain, vec![vec!["a.txt", "b.txt", "c.txt"]], "{plain:?}");
    // Error-guided repair pulls c.txt into a.txt's commit and keeps b.txt apart.
    assert_eq!(
        guided,
        vec![vec!["a.txt", "c.txt"], vec!["b.txt"]],
        "{guided:?}"
    );
    // Deterministic, with a warm cache too.
    assert_eq!(groups(&repo, true), guided);
}
