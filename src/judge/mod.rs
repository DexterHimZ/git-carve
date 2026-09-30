//! The learned judge (DECISIONS.md D28): P(same commit) for two groups of atoms, from carve's
//! own code facts. One feature function serves training (`git carve eval pairs`) and
//! inference, so the two cannot drift apart.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::atom::{AtomKind, Changes, Whole};
use crate::error::{Error, Result};
use crate::facts::{Analysis, Class, Key};
use crate::graph::Edges;
use crate::group::UnionFind;
use crate::lang::primary;

/// Feature names, in the order of [`Features::of`]'s output. Weights files are keyed by
/// these names, so reordering cannot silently scramble a model.
pub const FEATURES: &[&str] = &[
    "any_f0",
    "any_f1",
    "any_f2",
    "any_f3",
    "any_f6",
    "soft_density",
    "same_file",
    "near",
    "same_dir",
    "dir_overlap",
    "stem_link",
    "test_impl",
    "name_jaccard",
    "name_shared",
    "new_name_used",
    "both_new_files",
    "symbolless",
    "order_rel",
    "same_ext",
    "size_balance",
    "tree_size",
];

/// Features added after rung 1 (DECISIONS.md D38). A weights file may omit them (weight 0),
/// so the frozen rung-1 file keeps loading unchanged; every other feature is required.
const OPTIONAL_FEATURES: &[&str] = &["tree_size"];

/// Indices (into [`FEATURES`]) of the features that *link* two groups: an edge, a shared
/// file, stem, or name, a use of a new name, or a definition-use order. The rest (directory,
/// extension, sizes) describe location or shape, and the evidence floor (D37) never lets them
/// justify a merge on their own.
const LINKING: &[usize] = &[0, 1, 2, 3, 4, 6, 10, 13, 14, 17];

/// Per-atom data the features read, computed once per plan. Everything [`Features::of`]
/// needs is interned or tabulated here, so a query costs about linear time in the two groups'
/// sizes (pooled merging asks many; DECISIONS.md D38).
pub struct Features {
    /// ln(1 + hard-bound components of placeable atoms): the tree's size, so the model can
    /// condition its prior on it instead of on the training corpus's (D38).
    tree_size: f64,
    /// Per atom: interned path, first line, and flags.
    path: Vec<u32>,
    line: Vec<Option<u32>>,
    test: Vec<bool>,
    added: Vec<bool>,
    symbolless: Vec<bool>,
    /// Per atom, sorted and interned: mentioned names (stoplist applied), names of added
    /// definitions, and the four definition/reference key sets.
    names: Vec<Vec<u32>>,
    new_names: Vec<Vec<u32>>,
    adds_def: Vec<Vec<u32>>,
    adds_ref: Vec<Vec<u32>>,
    removes_def: Vec<Vec<u32>>,
    removes_ref: Vec<Vec<u32>>,
    /// Soft edges as adjacency: neighbor and a bitmask of kinds (F0, F1, F2, F3, F6).
    adj: Vec<Vec<(usize, u8)>>,
    /// Path-pair facts, `paths × paths`.
    paths: usize,
    pairs: Vec<PathPair>,
}

#[derive(Clone, Copy, Default)]
struct PathPair {
    same_dir: bool,
    dir_overlap: f64,
    stem_link: bool,
    same_ext: bool,
}

/// Sorted, deduplicated union of the given atoms' sets.
fn union_of(sets: &[Vec<u32>], atoms: &[usize]) -> Vec<u32> {
    let mut v: Vec<u32> = atoms
        .iter()
        .flat_map(|&i| sets[i].iter().copied())
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Size of the intersection of two sorted, deduplicated lists.
fn common(a: &[u32], b: &[u32]) -> usize {
    let (mut i, mut j, mut n) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                n += 1;
                i += 1;
                j += 1;
            }
        }
    }
    n
}

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

impl Features {
    pub fn new<'a>(changes: &'a Changes, analysis: &'a Analysis, edges: &Edges) -> Self {
        let n = changes.atoms.len();
        let facts = &analysis.atoms;
        let stop: BTreeSet<&str> = primary().stoplist().iter().copied().collect();
        let mut sets = UnionFind::new(n);
        for &(a, b, _) in &edges.hard {
            sets.union(a, b);
        }
        let components: BTreeSet<usize> = (0..n)
            .filter(|&i| matches!(facts[i].class, Class::Normal | Class::Symbolless))
            .map(|i| sets.find(i))
            .collect();

        // Interners: paths, names, and definition keys.
        let mut path_ids: BTreeMap<&[u8], u32> = BTreeMap::new();
        let mut path_list: Vec<&[u8]> = Vec::new();
        let path: Vec<u32> = changes
            .atoms
            .iter()
            .map(|a| {
                let p = changes.files[a.file].path.as_slice();
                *path_ids.entry(p).or_insert_with(|| {
                    path_list.push(p);
                    (path_list.len() - 1) as u32
                })
            })
            .collect();
        let mut name_ids: BTreeMap<&str, u32> = BTreeMap::new();
        let mut name = |s: &'a str| -> u32 {
            let next = name_ids.len() as u32;
            *name_ids.entry(s).or_insert(next)
        };
        let sorted = |mut v: Vec<u32>| {
            v.sort_unstable();
            v.dedup();
            v
        };
        let names: Vec<Vec<u32>> = facts
            .iter()
            .map(|f| {
                sorted(
                    f.mentioned
                        .iter()
                        .filter(|m| m.len() >= 3 && !stop.contains(m.as_str()))
                        .map(|m| name(m.as_str()))
                        .collect(),
                )
            })
            .collect();
        let new_names: Vec<Vec<u32>> = facts
            .iter()
            .map(|f| {
                sorted(
                    f.adds_def
                        .iter()
                        .map(|k| match k {
                            Key::Top(_, m) | Key::Local(_, m) => name(m.as_str()),
                        })
                        .collect(),
                )
            })
            .collect();
        let mut key_ids: BTreeMap<&Key, u32> = BTreeMap::new();
        let mut keys = |set: &'a BTreeSet<Key>| -> Vec<u32> {
            let v = set
                .iter()
                .map(|k| {
                    let next = key_ids.len() as u32;
                    *key_ids.entry(k).or_insert(next)
                })
                .collect();
            sorted(v)
        };
        let adds_def = facts.iter().map(|f| keys(&f.adds_def)).collect();
        let adds_ref = facts.iter().map(|f| keys(&f.adds_ref)).collect();
        let removes_def = facts.iter().map(|f| keys(&f.removes_def)).collect();
        let removes_ref = facts.iter().map(|f| keys(&f.removes_ref)).collect();

        let mut adj: Vec<Vec<(usize, u8)>> = vec![Vec::new(); n];
        for (&(x, y), kinds) in &edges.soft {
            // The features know the edge kinds rung 1 was trained on; F7 (D41) is not one of
            // them (`same_file` already carries that evidence), so it is ignored here.
            let mask = kinds.iter().fold(0u8, |m, k| {
                m | match *k {
                    "F0" => 1,
                    "F1" => 2,
                    "F2" => 4,
                    "F3" => 8,
                    "F6" => 16,
                    _ => 0,
                }
            });
            if mask != 0 {
                adj[x].push((y, mask));
                adj[y].push((x, mask));
            }
        }

        // Path-pair facts, exactly as the pairwise definition computes them.
        let paths = path_list.len();
        let mut pairs = vec![PathPair::default(); paths * paths];
        for (p, px) in path_list.iter().enumerate() {
            for (q, py) in path_list.iter().enumerate() {
                let (dx, dy) = (dir_of(px), dir_of(py));
                let (sx, sy): (Vec<&[u8]>, Vec<&[u8]>) = (
                    dx.split(|&c| c == b'/').collect(),
                    dy.split(|&c| c == b'/').collect(),
                );
                let shared = sx.iter().zip(&sy).take_while(|(a, b)| a == b).count();
                let deepest = sx.len().max(sy.len()).max(1);
                pairs[p * paths + q] = PathPair {
                    same_dir: dx == dy,
                    dir_overlap: shared as f64 / deepest as f64,
                    stem_link: stem(px) == stem(py),
                    same_ext: ext(px) == ext(py),
                };
            }
        }

        let line = changes
            .atoms
            .iter()
            .map(|a| match &a.kind {
                AtomKind::Region(r) => Some(if r.new.is_empty() {
                    r.base.start
                } else {
                    r.new.start
                }),
                _ => None,
            })
            .collect();
        Self {
            tree_size: (1.0 + components.len() as f64).ln(),
            test: path
                .iter()
                .map(|&p| is_test(path_list[p as usize]))
                .collect(),
            path,
            line,
            added: changes
                .atoms
                .iter()
                .map(|a| matches!(a.kind, AtomKind::Whole(Whole::Added)))
                .collect(),
            symbolless: facts.iter().map(|f| f.class == Class::Symbolless).collect(),
            names,
            new_names,
            adds_def,
            adds_ref,
            removes_def,
            removes_ref,
            adj,
            paths,
            pairs,
        }
    }

    /// The feature vector for groups `a` and `b` (atom indices), in [`FEATURES`] order.
    ///
    /// Each feature is defined over all atom pairs (x in `a`, y in `b`), as in
    /// `tests::of_reference`; this computes the same values from per-group unions.
    pub fn of(&self, a: &[usize], b: &[usize]) -> Vec<f64> {
        // Soft edges between the groups.
        let mut in_b = vec![false; self.path.len()];
        for &y in b {
            in_b[y] = true;
        }
        let (mut linked, mut kinds) = (0usize, 0u8);
        for &x in a {
            for &(y, mask) in &self.adj[x] {
                if in_b[y] {
                    linked += 1;
                    kinds |= mask;
                }
            }
        }
        // Path facts over distinct path pairs.
        let paths_of = |s: &[usize]| -> Vec<u32> {
            let mut v: Vec<u32> = s.iter().map(|&i| self.path[i]).collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        let (pa, pb) = (paths_of(a), paths_of(b));
        let (mut same_file, mut same_dir, mut stem_link, mut same_ext) =
            (false, false, false, false);
        let mut dir_overlap: f64 = 0.0;
        for &p in &pa {
            for &q in &pb {
                let f = self.pairs[p as usize * self.paths + q as usize];
                same_file |= p == q;
                same_dir |= f.same_dir;
                stem_link |= f.stem_link;
                same_ext |= f.same_ext;
                dir_overlap = dir_overlap.max(f.dir_overlap);
            }
        }
        // Nearest lines within a shared file.
        let mut min_gap = u32::MAX;
        if same_file {
            let mut lines: Vec<(u32, u32)> = b
                .iter()
                .filter_map(|&y| self.line[y].map(|l| (self.path[y], l)))
                .collect();
            lines.sort_unstable();
            for &x in a {
                let Some(lx) = self.line[x] else { continue };
                let px = self.path[x];
                let at = lines.partition_point(|&(p, l)| (p, l) < (px, lx));
                for k in [at.checked_sub(1), Some(at)].into_iter().flatten() {
                    if let Some(&(p, l)) = lines.get(k) {
                        if p == px {
                            min_gap = min_gap.min(lx.abs_diff(l));
                        }
                    }
                }
            }
        }
        let all = |s: &[usize], v: &[bool]| s.iter().all(|&i| v[i]);
        let test_impl = (all(a, &self.test) && b.iter().all(|&i| !self.test[i]))
            || (a.iter().all(|&i| !self.test[i]) && all(b, &self.test));
        let (na, nb) = (union_of(&self.names, a), union_of(&self.names, b));
        let inter = common(&na, &nb);
        let union = na.len() + nb.len() - inter;
        let (da, db) = (union_of(&self.new_names, a), union_of(&self.new_names, b));
        let new_name_used = common(&da, &nb) > 0 || common(&db, &na) > 0;
        let meets = |x: &[Vec<u32>], y: &[Vec<u32>]| {
            common(&union_of(x, a), &union_of(y, b)) > 0
                || common(&union_of(x, b), &union_of(y, a)) > 0
        };
        let order_rel =
            meets(&self.adds_def, &self.adds_ref) || meets(&self.removes_def, &self.removes_ref);
        let flag = |b: bool| if b { 1.0 } else { 0.0 };
        let (small, large) = (a.len().min(b.len()) as f64, a.len().max(b.len()) as f64);
        vec![
            flag(kinds & 1 != 0),
            flag(kinds & 2 != 0),
            flag(kinds & 4 != 0),
            flag(kinds & 8 != 0),
            flag(kinds & 16 != 0),
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
            flag(a.iter().any(|&i| self.added[i]) && b.iter().any(|&i| self.added[i])),
            flag(all(a, &self.symbolless) || all(b, &self.symbolless)),
            flag(order_rel),
            flag(same_ext),
            if large == 0.0 { 0.0 } else { small / large },
            self.tree_size,
        ]
    }
}

/// How the merge scores two clusters of hard-bound components (DECISIONS.md D28).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Linkage {
    /// Mean of the component-pair probabilities across the two clusters. Every probability
    /// is asked about a pair of components, the unit the model was trained on.
    Average,
    /// The model asked directly about the two merged clusters (their features pooled).
    Pooled,
}

/// The frozen rung-1 judge (`eval/LOCK`), compiled in so `--judge` needs no files, no network,
/// and no model download (DECISIONS.md D33).
const BUILTIN_WEIGHTS: &[u8] = include_bytes!("../../eval/judge/weights-v1.json");
const BUILTIN_THETA: f64 = 0.7;
const BUILTIN_PLATT: (f64, f64) = (0.821601, 0.480245);

/// A logistic model over [`FEATURES`], loaded from a reviewed weights file.
#[derive(Clone, Debug)]
pub struct LearnedJudge {
    bias: f64,
    weights: Vec<f64>,
    /// Platt scaling fitted on the tune split (EVAL.md §4 step 3): `p = σ(a·z + b)`.
    pub platt: Option<(f64, f64)>,
    /// Merge while the best cluster pair scores at least this.
    pub theta: f64,
    pub linkage: Linkage,
    /// D37: merge only on linking evidence (from the weights file).
    pub floor: bool,
    /// For reports: `learned:<first 12 hex of the weights file's SHA-256>`.
    pub name: String,
    /// The weights file's SHA-256, as frozen in `eval/LOCK`.
    pub sha256: String,
}

#[derive(Deserialize)]
struct WeightsFile {
    model: String,
    bias: f64,
    weights: BTreeMap<String, f64>,
    /// D37: merge only on linking evidence.
    #[serde(default)]
    evidence_floor: bool,
}

/// One merge of the judge's agglomeration, in order.
#[derive(Clone, Debug, PartialEq)]
pub struct Merge {
    /// An atom (index) of each merged cluster.
    pub a: usize,
    pub b: usize,
    /// The cluster pair's score when merged.
    pub p: f64,
    /// Both sides' atoms, the score, and the top features, for `--json`.
    pub why: String,
    /// The smaller side's atoms: the likelier odd one out, for D30's notes.
    pub smaller: Vec<usize>,
    /// The features that raised the score most, strongest first.
    pub top: Vec<&'static str>,
}

impl LearnedJudge {
    pub fn from_json(
        bytes: &[u8],
        theta: f64,
        linkage: Linkage,
        platt: Option<(f64, f64)>,
    ) -> Result<Self> {
        let file: WeightsFile = serde_json::from_slice(bytes)
            .map_err(|e| Error::Invalid(format!("judge weights: {e}")))?;
        if file.model != "logistic" {
            return Err(Error::Invalid(format!(
                "judge weights: unknown model {:?}",
                file.model
            )));
        }
        let weights = FEATURES
            .iter()
            .map(|f| match file.weights.get(*f) {
                Some(w) => Ok(*w),
                None if OPTIONAL_FEATURES.contains(f) => Ok(0.0),
                None => Err(Error::Invalid(format!(
                    "judge weights: missing feature {f}"
                ))),
            })
            .collect::<Result<Vec<f64>>>()?;
        if let Some(unknown) = file
            .weights
            .keys()
            .find(|k| !FEATURES.contains(&k.as_str()))
        {
            return Err(Error::Invalid(format!(
                "judge weights: unknown feature {unknown}"
            )));
        }
        let digest = crate::eval::sha256::hex(&crate::eval::sha256::sha256(bytes));
        Ok(Self {
            bias: file.bias,
            weights,
            platt,
            theta,
            linkage,
            floor: file.evidence_floor,
            name: format!("learned:{}", &digest[..12]),
            sha256: digest,
        })
    }

    /// Every inference setting besides the weights, as frozen in `eval/LOCK`.
    pub fn config(&self) -> String {
        let linkage = match self.linkage {
            Linkage::Average => "average",
            Linkage::Pooled => "pooled",
        };
        let platt = self
            .platt
            .map_or("none".into(), |(a, b)| format!("{a},{b}"));
        let floor = if self.floor { " floor=on" } else { "" };
        format!(
            "theta={} linkage={linkage} platt={platt}{floor}",
            self.theta
        )
    }

    /// Refuses unless `eval/LOCK` freezes exactly this judge (EVAL.md §2: the trained judge
    /// is frozen before it runs on the test split).
    pub fn check_frozen(&self, lock: &str) -> Result<()> {
        let field = |name: &str| {
            lock.lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(':').map(str::trim))
        };
        let (sha, config) = (field("judge_weights_sha256"), field("judge_config"));
        if sha == Some(self.sha256.as_str()) && config == Some(self.config().as_str()) {
            Ok(())
        } else {
            Err(Error::Invalid(format!(
                "eval/LOCK does not freeze this judge (weights {}, {}); the judge arm runs on tune only until it does",
                self.sha256,
                self.config()
            )))
        }
    }

    /// The built-in judge behind `--judge`: exactly the one frozen in `eval/LOCK`.
    pub fn builtin() -> Self {
        Self::from_json(
            BUILTIN_WEIGHTS,
            BUILTIN_THETA,
            Linkage::Pooled,
            Some(BUILTIN_PLATT),
        )
        .expect("the built-in weights are valid")
    }

    pub fn probability(&self, x: &[f64]) -> f64 {
        let z = self.bias + self.weights.iter().zip(x).map(|(w, v)| w * v).sum::<f64>();
        let z = self.platt.map_or(z, |(a, b)| a * z + b);
        1.0 / (1.0 + (-z).exp())
    }

    /// A pair's score: the probability, or 0 when the evidence floor applies and nothing
    /// links the two groups.
    fn score(&self, x: &[f64]) -> f64 {
        if self.floor && !LINKING.iter().any(|&i| x[i] > 0.0) {
            0.0
        } else {
            self.probability(x)
        }
    }

    /// Judge-mode grouping (DECISIONS.md D4, D28): starting from the hard-bound components
    /// of placeable atoms in `sets`, repeatedly merge the best-scoring cluster pair while its
    /// score is at least `min_theta`. Ties go to the lowest cluster indices.
    ///
    /// The merges are the same whatever the threshold; a threshold only decides where the
    /// sequence stops. So one sequence, cut at θ, gives the grouping for every θ ≥
    /// `min_theta` (`graph::plan_soft`).
    pub fn sequence(
        &self,
        changes: &Changes,
        analysis: &Analysis,
        edges: &Edges,
        mut sets: UnionFind,
        min_theta: f64,
    ) -> Vec<Merge> {
        let features = Features::new(changes, analysis, edges);
        let placeable =
            |i: usize| matches!(analysis.atoms[i].class, Class::Normal | Class::Symbolless);
        let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for i in (0..changes.atoms.len()).filter(|&i| placeable(i)) {
            groups.entry(sets.find(i)).or_default().push(i);
        }
        let comps: Vec<Vec<usize>> = groups.into_values().collect();
        let k = comps.len();
        // Component-pair scores, asked once.
        let mut prob = vec![vec![0.0; k]; k];
        for i in 0..k {
            for j in i + 1..k {
                let p = self.score(&features.of(&comps[i], &comps[j]));
                prob[i][j] = p;
                prob[j][i] = p;
            }
        }
        // Live clusters: their components, their atoms, and (Average) cross-pair sums.
        let mut members: Vec<Option<Vec<usize>>> = (0..k).map(|i| Some(vec![i])).collect();
        let mut atoms: Vec<Vec<usize>> = comps.clone();
        let mut sum = prob.clone();
        let mut score = prob.clone();
        let mut merges = Vec::new();
        loop {
            let mut best: Option<(usize, usize, f64)> = None;
            for i in 0..k {
                if members[i].is_none() {
                    continue;
                }
                for j in i + 1..k {
                    if members[j].is_some()
                        && score[i][j] >= min_theta
                        && best.is_none_or(|(_, _, s)| score[i][j] > s)
                    {
                        best = Some((i, j, score[i][j]));
                    }
                }
            }
            let Some((i, j, p)) = best else { break };
            let (why, top) = self.explain(changes, &features, &atoms[i], &atoms[j], p);
            let smaller = if atoms[j].len() < atoms[i].len() {
                atoms[j].clone()
            } else {
                atoms[i].clone()
            };
            merges.push(Merge {
                a: atoms[i][0],
                b: atoms[j][0],
                p,
                why,
                smaller,
                top,
            });
            let absorbed = members[j].take().expect("live cluster");
            let moved = std::mem::take(&mut atoms[j]);
            atoms[i].extend(moved);
            members[i].as_mut().expect("live cluster").extend(absorbed);
            for m in 0..k {
                if m == i || members[m].is_none() {
                    continue;
                }
                let s = match self.linkage {
                    Linkage::Average => {
                        sum[i][m] += sum[j][m];
                        sum[m][i] = sum[i][m];
                        let n = members[i].as_ref().expect("live").len()
                            * members[m].as_ref().expect("live").len();
                        sum[i][m] / n as f64
                    }
                    Linkage::Pooled => self.score(&features.of(&atoms[i], &atoms[m])),
                };
                score[i][m] = s;
                score[m][i] = s;
            }
        }
        merges
    }

    /// One merge, for `--json` (DECISIONS.md D33): the atoms on each side, the score, and the
    /// features that pushed the pooled pair's log-odds up the most.
    fn explain(
        &self,
        changes: &Changes,
        features: &Features,
        a: &[usize],
        b: &[usize],
        p: f64,
    ) -> (String, Vec<&'static str>) {
        let ids = |s: &[usize]| {
            let mut v: Vec<String> = s
                .iter()
                .take(4)
                .map(|&i| changes.atoms[i].id.to_string())
                .collect();
            if s.len() > 4 {
                v.push(format!("+{}", s.len() - 4));
            }
            v.join(",")
        };
        let scale = self.platt.map_or(1.0, |(a, _)| a);
        let x = features.of(a, b);
        let mut parts: Vec<(f64, &'static str)> = FEATURES
            .iter()
            .zip(self.weights.iter().zip(&x))
            .map(|(name, (w, v))| (w * v * scale, *name))
            .filter(|(c, _)| *c > 0.0)
            .collect();
        parts.sort_by(|l, r| r.0.total_cmp(&l.0).then(l.1.cmp(r.1)));
        let top: Vec<String> = parts
            .iter()
            .take(3)
            .map(|(c, n)| format!("{n} +{c:.2}"))
            .collect();
        (
            format!("{} + {}: {p:.2} ({})", ids(a), ids(b), top.join(", ")),
            parts.iter().take(3).map(|(_, n)| *n).collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_and_tests_are_recognized() {
        assert_eq!(
            stem(b"src/Button.tsx"),
            stem(b"src/__tests__/Button.test.tsx")
        );
        assert!(is_test(b"src/a.test.ts") && is_test(b"tests/a.ts") && !is_test(b"src/latest.ts"));
        assert_eq!(FEATURES.len(), 21);
    }

    #[test]
    fn the_builtin_judge_is_the_frozen_one() {
        let lock = include_str!("../../eval/LOCK");
        LearnedJudge::builtin()
            .check_frozen(lock)
            .expect("built-in judge matches eval/LOCK");
    }
}
