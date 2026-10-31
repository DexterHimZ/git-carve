"""DECISIONS.md D38: leave-one-repository-out weights for rung-1.5 variants (train repos only).

For every held-out training repository R and every variant, fits the D28 logistic model on
the other repositories' pairs and writes <out>/<variant>__<R>.json. `git carve eval sweep`
then scores every file on every window of the 2-8 commit training corpus, and
judge_folds_report.py picks theta on the other repositories and reports R's windows.

Variants (the D37 evidence floor trains on linked rows only, since it never asks the model
about unlinked pairs):
  r1       2-4 windows (pairs-train.jsonl), 20 features          (the rung-1 recipe, per fold)
  r1floor  2-4 windows, linked rows, 20 features, floor
  wide     2-8 windows (pairs-train-v2.jsonl), 20 features
  wsize    2-8 windows, 21 features (+ tree_size)
  wsfloor  2-8 windows, linked rows, 21 features, floor
Usage: judge_folds.py pairs-train.jsonl pairs-train-v2.jsonl out_dir
"""
import json, os, sys
import numpy as np

FEATURES = ["any_f0", "any_f1", "any_f2", "any_f3", "any_f6", "soft_density", "same_file", "near",
            "same_dir", "dir_overlap", "stem_link", "test_impl", "name_jaccard", "name_shared",
            "new_name_used", "both_new_files", "symbolless", "order_rel", "same_ext", "size_balance",
            "tree_size"]
LINKING = [0, 1, 2, 3, 4, 6, 10, 13, 14, 17]  # src/judge/mod.rs LINKING


def load(path, width):
    rows = [json.loads(l) for l in open(path)]
    X = np.array([(r["x"] + [0.0] * width)[:width] for r in rows], dtype=float)
    y = np.array([r["same_commit"] for r in rows], dtype=float)
    src = np.array([r["source"] for r in rows])
    return X, y, src


def fit(X, y, l2=1.0, iters=60):
    """Newton / IRLS with an L2 penalty (bias unpenalized). Deterministic."""
    Xb = np.hstack([np.ones((len(X), 1)), X])
    w = np.zeros(Xb.shape[1])
    reg = np.full(Xb.shape[1], l2); reg[0] = 0.0
    for _ in range(iters):
        p = 1 / (1 + np.exp(-(Xb @ w)))
        g = Xb.T @ (p - y) + reg * w
        H = (Xb * (p * (1 - p))[:, None]).T @ Xb + np.diag(reg)
        step = np.linalg.solve(H, g)
        w -= step
        if np.max(np.abs(step)) < 1e-8:
            break
    return w


def auroc(s, y):
    order = np.argsort(s, kind="mergesort")
    ranks = np.empty(len(s)); ranks[order] = np.arange(1, len(s) + 1)
    _, inv, counts = np.unique(s, return_inverse=True, return_counts=True)
    ranks = (np.bincount(inv, weights=ranks) / counts)[inv]
    pos = y == 1; n1, n0 = pos.sum(), (~pos).sum()
    return (ranks[pos].sum() - n1 * (n1 + 1) / 2) / (n1 * n0) if n1 and n0 else float("nan")


narrow = load(sys.argv[1], 21)   # v1 rows carry 20 features; tree_size stays 0 and unused
wide = load(sys.argv[2], 21)
out = sys.argv[3]
os.makedirs(out, exist_ok=True)
variants = {
    "r1": (narrow, 20, False),
    "r1floor": (narrow, 20, True),
    "wide": (wide, 20, False),
    "wsize": (wide, 21, False),
    "wsfloor": (wide, 21, True),
}
repos = sorted(set(wide[2]))
print(f"{'variant':<9} " + " ".join(f"{r:>15}" for r in repos) + "   (pair AUROC on the held-out repo)")
for name, ((X, y, src), width, floor) in variants.items():
    linked = (X[:, LINKING] > 0).any(axis=1) if floor else np.ones(len(y), bool)
    aucs = []
    for held in repos:
        tr = (src != held) & linked
        w = fit(X[tr][:, :width], y[tr])
        te = (src == held) & linked
        p = 1 / (1 + np.exp(-(np.hstack([np.ones((te.sum(), 1)), X[te][:, :width]]) @ w)))
        aucs.append(auroc(p, y[te]))
        json.dump({"version": 1, "model": "logistic", "features": FEATURES[:width], "bias": float(w[0]),
                   "weights": dict(zip(FEATURES[:width], w[1:].tolist())), "evidence_floor": floor,
                   "variant": name, "held_out": held, "trained_on": [r for r in repos if r != held],
                   "rows": int(tr.sum())},
                  open(os.path.join(out, f"{name}__{held}.json"), "w"), indent=2)
    print(f"{name:<9} " + " ".join(f"{a:>15.3f}" for a in aucs))
print("wrote", len(variants) * len(repos), "weights files to", out)
