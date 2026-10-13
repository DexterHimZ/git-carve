//! M5: the evaluation harness itself. Ground truth that is wrong would silently corrupt every
//! decision made from it, so labels are checked against histories where each changed line
//! says which commit wrote it.

mod common;

use std::collections::BTreeSet;

use common::{TestRepo, isolated};
use git_carve::eval::corpus::{self, Role, Source, Sources, WindowRules};
use git_carve::eval::run::{self, RunOptions};
use git_carve::git::Git;

/// A history of 24 commits by one author. Commit n rewrites one `return` line of `src/fN.ts`
/// (N = n mod 6) to `  return "commit-n";` and, every third commit, deletes the first line of
/// `notes/gN.txt` (subject "remove gN"). Function bodies have plain statements, so debug
/// lines can be injected after them.
fn history() -> TestRepo {
    let repo = TestRepo::new();
    for f in 0..6 {
        let body: String = (0..8)
            .map(|i| format!("export function f{f}_{i}() {{\n  const a = {i};\n  const b = a + 1;\n  return b;\n}}\n\n"))
            .collect();
        repo.write(&format!("src/f{f}.ts"), body);
        repo.write(
            &format!("notes/g{f}.txt"),
            (0..12).map(|i| format!("note {i}\n")).collect::<String>(),
        );
    }
    repo.commit_all("base");
    for n in 1..=24u32 {
        let f = n % 6;
        let path = format!("src/f{f}.ts");
        let content = std::fs::read_to_string(repo.path().join(&path)).unwrap();
        let target = ((n / 6) % 8) as usize;
        let mut seen = 0;
        let lines: Vec<String> = content
            .lines()
            .map(|l| {
                if l.trim_start().starts_with("return") {
                    seen += 1;
                    if seen == target + 1 {
                        return format!("  return \"commit-{n}\";");
                    }
                }
                l.to_owned()
            })
            .collect();
        repo.write(&path, lines.join("\n") + "\n");
        let mut subject = format!("commit-{n}");
        if n % 3 == 0 {
            let g = format!("notes/g{}.txt", (n + 1) % 6);
            let notes = std::fs::read_to_string(repo.path().join(&g)).unwrap();
            repo.write(
                &g,
                notes
                    .lines()
                    .skip(1)
                    .map(|l| format!("{l}\n"))
                    .collect::<String>(),
            );
            subject = format!("commit-{n} remove g{}", (n + 1) % 6);
        }
        repo.commit_all(&subject);
    }
    repo
}

fn sources(entries: &[(&str, &std::path::Path, Role)]) -> Sources {
    Sources {
        version: 1,
        seed: 20260924,
        window: WindowRules::default(),
        sources: entries
            .iter()
            .map(|(name, path, role)| Source {
                name: (*name).into(),
                location: path.display().to_string(),
                role: *role,
            })
            .collect(),
    }
}

fn env() -> Git {
    isolated(Git::new(std::env::temp_dir()))
}

#[test]
fn windows_are_disjoint_chains_and_labels_name_their_commits() {
    let source = history();
    let cache = tempfile::tempdir().unwrap();
    let manifest = corpus::build(
        &sources(&[("hist", source.path(), Role::EvalUser)]),
        &[Role::EvalUser],
        cache.path(),
        &env(),
    )
    .unwrap();
    assert!(
        manifest.windows.len() >= 4,
        "{} windows",
        manifest.windows.len()
    );

    let mut used = BTreeSet::new();
    for w in &manifest.windows {
        assert!((2..=4).contains(&w.commits.len()));
        for c in &w.commits {
            assert!(used.insert(c.clone()), "windows overlap at {c}");
        }
        let subject = |c: &str| source.run(&["log", "-1", "--format=%s", c]);
        for (path, prov) in &w.provenance {
            let last = source.run(&["show", &format!("{}:{path}", w.last())]);
            let last_lines: Vec<&str> = last.lines().collect();
            for (&line, &idx) in &prov.added {
                let wrote = subject(&w.commits[idx as usize]);
                let number = wrote.split(' ').next().unwrap();
                assert!(
                    last_lines[line as usize].contains(number),
                    "{path}:{line} attributed to {wrote}"
                );
            }
            for &idx in prov.removed.values() {
                let wrote = subject(&w.commits[idx as usize]);
                if let Some(stem) = path.strip_prefix("notes/") {
                    let stem = stem.trim_end_matches(".txt");
                    assert!(
                        wrote.ends_with(&format!("remove {stem}")),
                        "{path} removal attributed to {wrote}"
                    );
                } else {
                    // A rewritten line's old version is removed by the rewriting commit.
                    let n: u32 = wrote
                        .split(' ')
                        .next()
                        .unwrap()
                        .trim_start_matches("commit-")
                        .parse()
                        .unwrap();
                    assert_eq!(
                        format!("src/f{}.ts", n % 6),
                        *path,
                        "{path} removal attributed to {wrote}"
                    );
                }
            }
        }
    }
    // Splits and windows are a pure function of history and seed.
    let again = corpus::build(
        &sources(&[("hist", source.path(), Role::EvalUser)]),
        &[Role::EvalUser],
        cache.path(),
        &env(),
    )
    .unwrap();
    let key = |m: &corpus::Manifest| {
        m.windows
            .iter()
            .map(|w| (w.base.clone(), w.split))
            .collect::<Vec<_>>()
    };
    assert_eq!(key(&manifest), key(&again));
}

#[test]
fn a_training_source_sharing_eval_history_is_refused() {
    let source = history();
    let cache = tempfile::tempdir().unwrap();
    let copy = tempfile::tempdir().unwrap();
    env()
        .run([
            "clone",
            "-q",
            source.path().to_str().unwrap(),
            copy.path().join("c").to_str().unwrap(),
        ])
        .unwrap();
    let s = sources(&[
        ("eval", source.path(), Role::EvalUser),
        ("train", &copy.path().join("c"), Role::Train),
    ]);
    let err = corpus::build(&s, &[Role::Train], cache.path(), &env()).unwrap_err();
    assert!(err.to_string().contains("leak"), "{err}");
}

#[test]
fn clones_of_one_repository_count_once() {
    let source = history();
    let cache = tempfile::tempdir().unwrap();
    let s = sources(&[
        ("a", source.path(), Role::EvalUser),
        ("b", source.path(), Role::EvalOss),
    ]);
    let manifest =
        corpus::build(&s, &[Role::EvalUser, Role::EvalOss], cache.path(), &env()).unwrap();
    assert_eq!(manifest.sources.len(), 1);
}

#[test]
fn runs_score_windows_and_find_injected_debug_lines() {
    let source = history();
    let cache = tempfile::tempdir().unwrap();
    let manifest = corpus::build(
        &sources(&[("hist", source.path(), Role::EvalUser)]),
        &[Role::EvalUser],
        cache.path(),
        &env(),
    )
    .unwrap();
    let injections: usize = manifest.windows.iter().map(|w| w.injections.len()).sum();
    assert!(injections > 0, "some windows get injected debug lines");
    let results = run::run(
        &manifest,
        &RunOptions {
            split: None,
            check: false,
            judge: None,
            no_new_errors: false,
        },
        cache.path(),
        &env(),
    )
    .unwrap();
    let a = &results.aggregate;
    let errors: Vec<_> = results
        .windows
        .iter()
        .filter_map(|w| w.skipped.clone())
        .filter(|s| s.starts_with("error"))
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(a.windows_scored > 0);
    let found: usize = results.windows.iter().map(|w| w.injected).sum();
    assert_eq!(
        found, injections,
        "each injection is its own atom, labeled as injected"
    );
    assert!(a.e1_bcubed_f1 > 0.0 && a.e1_bcubed_f1 <= 1.0);
    assert_eq!(a.e5_real_code_left, 0, "only injected lines are left out");
    // The source repository is only read: its working tree is untouched.
    assert_eq!(source.run(&["status", "--porcelain"]), "");
}

/// Every file under `.git`, with its content: the user's repository must not change at all.
fn git_dir_digest(repo: &TestRepo) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            if meta.is_dir() {
                out.insert(rel, b"dir".to_vec());
                walk(root, &path, out);
            } else {
                let mut record = format!("{:?}\n", meta.modified().unwrap()).into_bytes();
                record.extend(std::fs::read(&path).unwrap_or_default());
                out.insert(rel, record);
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    let git_dir = repo.path().join(".git");
    walk(&git_dir, &git_dir, &mut out);
    out
}

#[test]
fn capture_saves_the_messy_tree_without_touching_the_repository() {
    use git_carve::eval::real;

    let repo = TestRepo::new();
    repo.write("src/a.ts", "export const a = 1;\n");
    repo.write("src/gone.ts", "export const gone = 1;\n");
    repo.write(".gitignore", "*.log\n");
    repo.commit_all("base");
    repo.write("src/a.ts", "export const a = 2;\n");
    repo.remove("src/gone.ts");
    repo.write("src/new.ts", "export const fresh = true;\n");
    repo.write("debug.log", "ignored by .gitignore\n");
    std::fs::write(repo.path().join(".git/info/exclude"), "secret.txt\n").unwrap();
    repo.write("secret.txt", "ignored by info/exclude\n");

    let (state, files, git_dir) = (
        repo.observable(),
        repo.worktree_digest(),
        git_dir_digest(&repo),
    );
    let base = tempfile::tempdir().unwrap();
    let env = isolated(Git::new(repo.path()));
    let record = real::capture(&env, repo.path(), base.path(), "a note").unwrap();

    // Nothing in the user's repository changed: not the work tree, not one byte under .git.
    assert_eq!(repo.observable(), state);
    assert_eq!(repo.worktree_digest(), files);
    assert_eq!(git_dir_digest(&repo), git_dir);

    // The store holds exactly what `git add -A` would commit, untracked file included and
    // ignored files excluded, pinned by a ref, with the user's HEAD as parent.
    let store = isolated(Git::new(real::store_dir(base.path(), &record.root)));
    let tree = store
        .run_str(["rev-parse", &format!("refs/real/{}^{{tree}}", record.id)])
        .unwrap();
    let head = repo.run(&["rev-parse", "HEAD"]);
    assert_eq!(
        store
            .run_str(["rev-parse", &format!("{}^", record.id)])
            .unwrap(),
        head
    );
    assert_eq!(record.head, head);
    assert_eq!(tree, repo.oracle_tree());
    let listed = store
        .run_str(["ls-tree", "-r", "--name-only", &tree])
        .unwrap();
    assert!(listed.contains("src/new.ts") && !listed.contains("gone") && !listed.contains(".log"));
    assert!(!listed.contains("secret"));

    // A second capture appends to the same store and index.
    repo.write("src/a.ts", "export const a = 3;\n");
    real::capture(&env, repo.path(), base.path(), "").unwrap();
    assert_eq!(real::records(base.path()).unwrap().len(), 2);
    assert_eq!(real::records(base.path()).unwrap()[0], record);
}

#[test]
fn real_trees_render_blinded_and_unblind_consistently() {
    use git_carve::eval::real::{self, Scores};

    let repo = TestRepo::demo();
    let (state, files) = (repo.observable(), repo.worktree_digest());
    let base = tempfile::tempdir().unwrap();
    let env = isolated(Git::new(repo.path()));
    let record = real::capture(&env, repo.path(), base.path(), "demo").unwrap();
    let out = base.path().join("blind");
    let skipped = real::render(&env, base.path(), &out, 7).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(repo.observable(), state);
    assert_eq!(repo.worktree_digest(), files);

    let (a, b) = (
        std::fs::read_to_string(out.join("tree-01/A.txt")).unwrap(),
        std::fs::read_to_string(out.join("tree-01/B.txt")).unwrap(),
    );
    assert!(a.starts_with("# git carve:") && b.starts_with("# git carve:"));
    let mut scores: Scores =
        serde_json::from_slice(&std::fs::read(out.join("scores.json")).unwrap()).unwrap();
    assert_eq!((scores.seed, scores.trees.len()), (7, 1));
    assert!(
        real::unblind(&scores).is_err(),
        "unscored trees are refused"
    );

    // The user prefers A and would make 1 cut in A, 3 in B.
    let t = &mut scores.trees[0];
    (t.cuts_to_change.a, t.cuts_to_change.b) = (Some(1.0), Some(3.0));
    (t.leave_disagreements.a, t.leave_disagreements.b) = (Some(0.0), Some(2.0));
    t.preferred = Some("A".into());
    let r = real::unblind(&scores).unwrap();
    if real::judge_is_a(7, &record.id) {
        assert_eq!(
            (r.cuts_judge[0], r.cuts_code[0], r.preferred_judge),
            (1.0, 3.0, 1)
        );
    } else {
        assert_eq!(
            (r.cuts_code[0], r.cuts_judge[0], r.preferred_code),
            (1.0, 3.0, 1)
        );
    }
}
