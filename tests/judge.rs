//! The learned judge (DECISIONS.md D28): feature symmetry, weights validation, and judge-mode
//! grouping, which keeps every hard bind and the code-only leave decisions.

mod common;

use common::{TestRepo, group_of};
use git_carve::graph;
use git_carve::judge::{FEATURES, Features, LearnedJudge, Linkage};

fn weights(bias: f64, set: &[(&str, f64)]) -> Vec<u8> {
    let weights: serde_json::Map<String, serde_json::Value> = FEATURES
        .iter()
        .map(|f| {
            let v = set.iter().find(|(n, _)| n == f).map_or(0.0, |(_, v)| *v);
            ((*f).to_owned(), v.into())
        })
        .collect();
    serde_json::to_vec(
        &serde_json::json!({ "model": "logistic", "bias": bias, "weights": weights }),
    )
    .expect("json")
}

fn judge(bias: f64, set: &[(&str, f64)], linkage: Linkage) -> LearnedJudge {
    LearnedJudge::from_json(&weights(bias, set), 0.5, linkage, None).expect("weights")
}

#[test]
fn features_are_symmetric() {
    let repo = TestRepo::demo();
    let (changes, analysis, _) = repo.plan();
    let edges = graph::edges(&changes, &analysis);
    let features = Features::new(&changes, &analysis, &edges);
    let n = changes.atoms.len();
    for a in 0..n {
        for b in 0..n {
            assert_eq!(
                features.of(&[a], &[b]),
                features.of(&[b], &[a]),
                "atoms {a} {b}"
            );
        }
    }
    let (left, right): (Vec<usize>, Vec<usize>) = (0..n).partition(|i| i % 2 == 0);
    assert_eq!(features.of(&left, &right), features.of(&right, &left));
}

#[test]
fn bad_weights_are_refused() {
    let bad = |bytes: &[u8]| LearnedJudge::from_json(bytes, 0.5, Linkage::Average, None).is_err();
    assert!(bad(b"{}"));
    assert!(bad(br#"{"model": "tree", "bias": 0, "weights": {}}"#));
    let mut missing: serde_json::Value = serde_json::from_slice(&weights(0.0, &[])).expect("json");
    missing["weights"]
        .as_object_mut()
        .expect("map")
        .remove("same_file");
    assert!(bad(&serde_json::to_vec(&missing).expect("json")));
    let mut extra = missing.clone();
    extra["weights"]["same_file"] = 0.0.into();
    extra["weights"]["mystery"] = 1.0.into();
    assert!(bad(&serde_json::to_vec(&extra).expect("json")));
}

#[test]
fn a_judge_that_never_merges_keeps_hard_binds_and_leave() {
    let repo = TestRepo::demo();
    let (changes, analysis, code) = repo.plan();
    for linkage in [Linkage::Average, Linkage::Pooled] {
        let plan = graph::plan_with(&changes, &analysis, Some(&judge(-50.0, &[], linkage)));
        assert_eq!(plan.leave, code.leave);
        assert!(plan.groups.len() >= code.groups.len());
        for &(a, b, kind) in &graph::edges(&changes, &analysis).hard {
            let (a, b) = (changes.atoms[a].id, changes.atoms[b].id);
            assert_eq!(group_of(&plan, a), group_of(&plan, b), "{kind} {a} {b}");
        }
    }
}

#[test]
fn a_judge_that_always_merges_makes_one_group() {
    let repo = TestRepo::demo();
    let (changes, analysis, code) = repo.plan();
    for linkage in [Linkage::Average, Linkage::Pooled] {
        let plan = graph::plan_with(&changes, &analysis, Some(&judge(50.0, &[], linkage)));
        assert_eq!(plan.groups.len(), 1, "{linkage:?}");
        assert_eq!(plan.leave, code.leave);
    }
}

#[test]
fn judge_plans_are_deterministic() {
    let set = [("same_file", 4.0), ("order_rel", 3.0), ("same_dir", 1.0)];
    let (a, b) = (TestRepo::demo(), TestRepo::demo());
    for linkage in [Linkage::Average, Linkage::Pooled] {
        let j = judge(-2.5, &set, linkage);
        let (ca, aa, _) = a.plan();
        let (cb, ab, _) = b.plan();
        let (pa, pb) = (
            graph::plan_with(&ca, &aa, Some(&j)),
            graph::plan_with(&cb, &ab, Some(&j)),
        );
        assert_eq!(pa.groups, pb.groups);
        assert_eq!(pa, graph::plan_with(&ca, &aa, Some(&j)));
    }
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
fn the_judge_flag_carves_end_to_end_and_undoes() {
    let repo = TestRepo::demo();
    let (state, files) = (repo.observable(), repo.worktree_digest());
    let json = |extra: &[&str]| -> serde_json::Value {
        let out = repo
            .carve()
            .args(["--plan-only", "--no-check", "--json"])
            .args(extra)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", common::stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    };
    // Code-only output does not mention a judge; judge mode names the built-in one.
    assert!(json(&[])["diagnostics"].get("judge").is_none());
    let judged = json(&["--judge"]);
    assert_eq!(
        judged["diagnostics"]["judge"],
        LearnedJudge::builtin().name.as_str()
    );

    let buffer = repo
        .carve()
        .args(["--plan-only", "--no-check", "--judge"])
        .output()
        .unwrap();
    let file = repo.path().join("../judge-plan.txt");
    std::fs::write(
        &file,
        with_messages(&String::from_utf8_lossy(&buffer.stdout)),
    )
    .unwrap();
    repo.run(&["config", "carve.agentMessages", "true"]); // D49: a scripted plan file
    let out = repo
        .carve()
        .args(["--no-check", "--judge", "--plan-file"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", common::stderr(&out));
    let groups = judged["groups"].as_array().unwrap().len();
    assert_eq!(
        repo.run(&["rev-list", "--count", "HEAD"]),
        (1 + groups).to_string()
    );
    assert_eq!(repo.worktree_digest(), files, "working tree untouched");

    let out = repo.carve().arg("--undo").output().unwrap();
    assert!(out.status.success(), "{}", common::stderr(&out));
    assert_eq!(repo.observable(), state);
    assert_eq!(repo.worktree_digest(), files);
}

#[test]
fn one_sequence_cut_at_theta_equals_a_run_at_theta() {
    let repo = TestRepo::demo();
    let (changes, analysis, _) = repo.plan();
    let set = [
        ("same_file", 4.0),
        ("order_rel", 3.0),
        ("same_dir", 1.5),
        ("tree_size", -0.3),
    ];
    let merges = graph::judge_sequence(
        &changes,
        &analysis,
        &judge(-2.0, &set, Linkage::Pooled),
        0.0,
    );
    assert!(merges.len() > 3, "a full sequence: {merges:?}");
    for theta in [0.05, 0.3, 0.5, 0.7, 0.9, 0.99] {
        let mut j = judge(-2.0, &set, Linkage::Pooled);
        j.theta = theta;
        let direct = graph::plan_with(&changes, &analysis, Some(&j));
        let replayed = graph::plan_soft(
            &changes,
            &analysis,
            graph::Soft::Replay {
                judge: &j.name,
                merges: &merges,
                theta,
            },
        );
        assert_eq!(direct, replayed, "theta {theta}");
    }
}

#[test]
fn the_evidence_floor_keeps_unlinked_changes_apart() {
    let repo = TestRepo::demo();
    let (changes, analysis, code) = repo.plan();
    let mut file: serde_json::Value = serde_json::from_slice(&weights(50.0, &[])).unwrap();
    file["evidence_floor"] = true.into();
    let floor = LearnedJudge::from_json(
        &serde_json::to_vec(&file).unwrap(),
        0.5,
        Linkage::Pooled,
        None,
    )
    .unwrap();
    assert!(floor.config().ends_with(" floor=on"));
    let plan = graph::plan_with(&changes, &analysis, Some(&floor));
    // Without the floor this judge makes one group; with it, the drive-by fix (linked to
    // nothing) stays alone, as in code-only mode.
    let fix = common::atoms_in(&changes, "src/parse.ts");
    let g = group_of(&plan, fix[0]).unwrap();
    assert_eq!(plan.groups[g], fix);
    assert_eq!(plan.groups.len(), code.groups.len());
}

#[test]
fn features_added_later_are_optional_but_names_are_checked() {
    let mut file: serde_json::Value = serde_json::from_slice(&weights(0.0, &[])).unwrap();
    file["weights"].as_object_mut().unwrap().remove("tree_size");
    assert!(
        LearnedJudge::from_json(
            &serde_json::to_vec(&file).unwrap(),
            0.5,
            Linkage::Pooled,
            None
        )
        .is_ok()
    );
    file["weights"]["tree_sise"] = 1.0.into();
    assert!(
        LearnedJudge::from_json(
            &serde_json::to_vec(&file).unwrap(),
            0.5,
            Linkage::Pooled,
            None
        )
        .is_err()
    );
}

#[test]
fn unsure_notes_flag_the_merges_closest_to_theta() {
    let repo = TestRepo::demo();
    let (changes, analysis, code) = repo.plan();
    // The frozen judge joins the drive-by fix at exactly θ (DECISIONS.md D37).
    let j = LearnedJudge::builtin();
    let plan = graph::plan_with(&changes, &analysis, Some(&j));
    assert!(!plan.unsure.is_empty() && plan.unsure.len() <= graph::UNSURE_MAX);
    let fix = common::atoms_in(&changes, "src/parse.ts");
    let g = group_of(&plan, fix[0]).unwrap();
    let note = plan
        .unsure
        .iter()
        .find(|(k, _)| *k == g)
        .map(|(_, n)| n.as_str())
        .expect("a note on the fix's commit");
    assert!(
        note.starts_with("# unsure: a8 joined this commit at 0.70"),
        "{note}"
    );
    assert!(note.contains("same_dir"), "{note}");
    // Notes never change the grouping, and code-only plans have none.
    assert!(code.unsure.is_empty());
    let mut far = LearnedJudge::builtin();
    far.theta = 0.95;
    assert!(
        graph::plan_with(&changes, &analysis, Some(&far))
            .unsure
            .len()
            <= graph::UNSURE_MAX
    );

    // Shown in the user's buffer, hidden in blinded renders.
    let render = |unsure| {
        git_carve::plan::render(
            &changes,
            &analysis,
            &plan,
            &git_carve::plan::RenderOptions {
                verification: None,
                messages: &[],
                notes: &[],
                diff_lines: 0,
                unsure,
            },
        )
    };
    assert!(render(true).contains(note) && !render(false).contains("# unsure"));
    // The parser ignores them: the buffer with notes parses back to the same plan.
    let with_messages = render(true).replace("commit\n\n", "commit\nA message\n");
    let git_carve::plan::Parsed::Plan(parsed) =
        git_carve::plan::parse(&with_messages, &changes, &plan.hard)
    else {
        panic!("not a plan");
    };
    assert_eq!(parsed.structure().0, plan.groups);
}

/// The original pairwise definition of every feature (DECISIONS.md D28), kept as the oracle
/// for the fast `Features::of` (D38): same values, bit for bit.
mod reference {
    use std::collections::BTreeSet;

    use git_carve::atom::{AtomKind, Changes, Whole};
    use git_carve::facts::{Analysis, Class, Key};
    use git_carve::graph::Edges;

    fn dir_of(path: &[u8]) -> &[u8] {
        match path.iter().rposition(|&b| b == b'/') {
            Some(i) => &path[..i],
            None => b"",
        }
    }

    fn is_test(path: &[u8]) -> bool {
        let p = String::from_utf8_lossy(path);
        p.contains(".test.")
            || p.contains(".spec.")
            || p.contains("__tests__/")
            || p.starts_with("test/")
            || p.starts_with("tests/")
            || p.contains("/test/")
            || p.contains("/tests/")
    }

    fn stem(path: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut dir = dir_of(path).to_vec();
        for special in [&b"/__tests__"[..], b"/__snapshots__", b"/__mocks__"] {
            if let Some(stripped) = dir.strip_suffix(special) {
                dir = stripped.to_vec();
            }
        }
        let name = &path[dir_of(path).len() + usize::from(!dir_of(path).is_empty())..];
        (
            dir,
            name.split(|&b| b == b'.').next().unwrap_or(name).to_vec(),
        )
    }

    fn ext(path: &[u8]) -> &[u8] {
        match path.iter().rposition(|&b| b == b'.') {
            Some(i) => &path[i..],
            None => b"",
        }
    }

    pub fn of(
        changes: &Changes,
        analysis: &Analysis,
        edges: &Edges,
        tree_size: f64,
        a: &[usize],
        b: &[usize],
    ) -> Vec<f64> {
        let stop: BTreeSet<&str> = git_carve::lang::primary()
            .stoplist()
            .iter()
            .copied()
            .collect();
        let mentioned: Vec<BTreeSet<String>> = analysis
            .atoms
            .iter()
            .map(|f| {
                f.mentioned
                    .iter()
                    .filter(|n| n.len() >= 3 && !stop.contains(n.as_str()))
                    .cloned()
                    .collect()
            })
            .collect();
        let path = |i: usize| changes.files[changes.atoms[i].file].path.as_slice();
        let line = |i: usize| match &changes.atoms[i].kind {
            AtomKind::Region(r) => Some(if r.new.is_empty() {
                r.base.start
            } else {
                r.new.start
            }),
            _ => None,
        };
        let facts = &analysis.atoms;
        let mut any = [false; 5];
        let mut linked = 0usize;
        let (mut same_file, mut same_dir, mut stem_link, mut same_ext) =
            (false, false, false, false);
        let mut min_gap = u32::MAX;
        let mut dir_overlap: f64 = 0.0;
        for &x in a {
            for &y in b {
                // Edge kinds rung 1 knows: F0-F3 and F6; F7 (D41) is not a feature.
                if let Some(kinds) = edges.soft.get(&(x.min(y), x.max(y))) {
                    let known: Vec<usize> = kinds
                        .iter()
                        .filter_map(|k| match *k {
                            "F0" => Some(0),
                            "F1" => Some(1),
                            "F2" => Some(2),
                            "F3" => Some(3),
                            "F6" => Some(4),
                            _ => None,
                        })
                        .collect();
                    if !known.is_empty() {
                        linked += 1;
                    }
                    for slot in known {
                        any[slot] = true;
                    }
                }
                let (px, py) = (path(x), path(y));
                if px == py {
                    same_file = true;
                    if let (Some(lx), Some(ly)) = (line(x), line(y)) {
                        min_gap = min_gap.min(lx.abs_diff(ly));
                    }
                }
                let (dx, dy) = (dir_of(px), dir_of(py));
                same_dir |= dx == dy;
                let (sx, sy): (Vec<&[u8]>, Vec<&[u8]>) = (
                    dx.split(|&c| c == b'/').collect(),
                    dy.split(|&c| c == b'/').collect(),
                );
                let shared = sx.iter().zip(&sy).take_while(|(p, q)| p == q).count();
                let deepest = sx.len().max(sy.len()).max(1);
                dir_overlap = dir_overlap.max(shared as f64 / deepest as f64);
                stem_link |= stem(px) == stem(py);
                same_ext |= ext(px) == ext(py);
            }
        }
        let tests = |s: &[usize]| s.iter().all(|&i| is_test(path(i)));
        let impls = |s: &[usize]| s.iter().all(|&i| !is_test(path(i)));
        let test_impl = (tests(a) && impls(b)) || (impls(a) && tests(b));
        let names = |s: &[usize]| -> BTreeSet<&String> {
            s.iter().flat_map(|&i| mentioned[i].iter()).collect()
        };
        let (na, nb) = (names(a), names(b));
        let inter = na.intersection(&nb).count();
        let union = na.union(&nb).count();
        let new_names = |s: &[usize]| -> BTreeSet<String> {
            s.iter()
                .flat_map(|&i| facts[i].adds_def.iter())
                .map(|k| match k {
                    Key::Top(_, n) | Key::Local(_, n) => n.clone(),
                })
                .collect()
        };
        let (da, db) = (new_names(a), new_names(b));
        let new_name_used = da.iter().any(|n| nb.contains(n)) || db.iter().any(|n| na.contains(n));
        let added = |s: &[usize]| {
            s.iter()
                .any(|&i| matches!(changes.atoms[i].kind, AtomKind::Whole(Whole::Added)))
        };
        let symbolless = |s: &[usize]| s.iter().all(|&i| facts[i].class == Class::Symbolless);
        let order_rel = a.iter().any(|&x| {
            b.iter().any(|&y| {
                let (fx, fy) = (&facts[x], &facts[y]);
                fx.adds_def.intersection(&fy.adds_ref).next().is_some()
                    || fy.adds_def.intersection(&fx.adds_ref).next().is_some()
                    || fx
                        .removes_def
                        .intersection(&fy.removes_ref)
                        .next()
                        .is_some()
                    || fy
                        .removes_def
                        .intersection(&fx.removes_ref)
                        .next()
                        .is_some()
            })
        });
        let flag = |b: bool| if b { 1.0 } else { 0.0 };
        let (small, large) = (a.len().min(b.len()) as f64, a.len().max(b.len()) as f64);
        vec![
            flag(any[0]),
            flag(any[1]),
            flag(any[2]),
            flag(any[3]),
            flag(any[4]),
            linked as f64 / (a.len() * b.len()).max(1) as f64,
            flag(same_file),
            if min_gap == u32::MAX {
                0.0
            } else {
                1.0 / (1.0 + f64::from(min_gap) / 10.0)
            },
            flag(same_dir),
            dir_overlap,
            flag(stem_link),
            flag(test_impl),
            if union == 0 {
                0.0
            } else {
                inter as f64 / union as f64
            },
            flag(inter > 0),
            flag(new_name_used),
            flag(added(a) && added(b)),
            flag(symbolless(a) || symbolless(b)),
            flag(order_rel),
            flag(same_ext),
            if large == 0.0 { 0.0 } else { small / large },
            tree_size,
        ]
    }
}

#[test]
fn fast_features_equal_the_pairwise_definition() {
    let repo = TestRepo::demo();
    let (changes, analysis, _) = repo.plan();
    let edges = graph::edges(&changes, &analysis);
    let features = Features::new(&changes, &analysis, &edges);
    let n = changes.atoms.len();
    let tree_size = features.of(&[0], &[1])[20];
    // Deterministic pseudo-random disjoint groups, sizes 1..=6, plus every singleton pair.
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = |m: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % m as u64) as usize
    };
    let mut cases: Vec<(Vec<usize>, Vec<usize>)> = (0..n)
        .flat_map(|a| {
            (0..n)
                .filter(move |&b| b != a)
                .map(move |b| (vec![a], vec![b]))
        })
        .collect();
    for _ in 0..400 {
        let mut order: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            order.swap(i, next(i + 1));
        }
        let (ka, kb) = (1 + next(6), 1 + next(6));
        cases.push((order[..ka].to_vec(), order[ka..ka + kb].to_vec()));
    }
    for (a, b) in &cases {
        let fast = features.of(a, b);
        let slow = reference::of(&changes, &analysis, &edges, tree_size, a, b);
        assert_eq!(
            fast.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            slow.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            "groups {a:?} {b:?}: fast {fast:?} slow {slow:?}"
        );
    }
}
