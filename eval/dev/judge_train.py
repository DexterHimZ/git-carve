"""D28 rung 1: logistic regression over carve's pair features.

Leave-one-repository-out on the TRAIN sources (never eval repos), then the model trained on
all train sources is scored on the eval TUNE split (allowed: tuning uses tune). Every score
sits next to the frozen code arm's own decision on the same pairs (`code_same`).
Usage: judge_train.py pairs-train.jsonl pairs-evaltune.jsonl [weights_out.json]
"""
import json, sys
import numpy as np

FEATURES = ["any_f0", "any_f1", "any_f2", "any_f3", "any_f6", "soft_density", "same_file", "near",
            "same_dir", "dir_overlap", "stem_link", "test_impl", "name_jaccard", "name_shared",
            "new_name_used", "both_new_files", "symbolless", "order_rel", "same_ext", "size_balance"]

def load(path):
    rows = [json.loads(l) for l in open(path)]
    X = np.array([r["x"] for r in rows], dtype=float)
    y = np.array([r["same_commit"] for r in rows], dtype=float)
    code = np.array([r["code_same"] for r in rows], dtype=float)
    src = np.array([r["source"] for r in rows])
    return X, y, code, src

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

def predict(w, X):
    return 1 / (1 + np.exp(-(np.hstack([np.ones((len(X), 1)), X]) @ w)))

def auroc(s, y):
    order = np.argsort(s, kind="mergesort")
    ranks = np.empty(len(s)); ranks[order] = np.arange(1, len(s) + 1)
    # average ranks for ties
    _, inv, counts = np.unique(s, return_inverse=True, return_counts=True)
    sums = np.bincount(inv, weights=ranks); ranks = (sums / counts)[inv]
    pos = y == 1; n1, n0 = pos.sum(), (~pos).sum()
    return (ranks[pos].sum() - n1 * (n1 + 1) / 2) / (n1 * n0) if n1 and n0 else float("nan")

def prf(pred, y):
    tp = float(((pred == 1) & (y == 1)).sum()); fp = float(((pred == 1) & (y == 0)).sum()); fn = float(((pred == 0) & (y == 1)).sum())
    p = tp / (tp + fp) if tp + fp else 1.0; r = tp / (tp + fn) if tp + fn else 1.0
    return p, r, (2 * p * r / (p + r) if p + r else 0.0)

def ece(p, y, bins=10):
    idx = np.minimum((p * bins).astype(int), bins - 1)
    return sum(abs(p[idx == b].mean() - y[idx == b].mean()) * (idx == b).mean() for b in range(bins) if (idx == b).any())

X, y, code, src = load(sys.argv[1])
print(f"train rows {len(y)}  positives {y.mean():.3f}  sources {sorted(set(src))}")
print(f"{'held-out repo':<16} {'n':>6} {'AUROC model':>11} {'code P/R/F1':>18} {'model P/R/F1 @0.5':>20}")
for repo in sorted(set(src)):
    tr, te = src != repo, src == repo
    w = fit(X[tr], y[tr])
    p = predict(w, X[te])
    cp, cr, cf = prf(code[te], y[te]); mp, mr, mf = prf((p >= 0.5).astype(float), y[te])
    print(f"{repo:<16} {te.sum():>6} {auroc(p, y[te]):>11.3f}   {cp:.2f}/{cr:.2f}/{cf:.2f}      {mp:.2f}/{mr:.2f}/{mf:.2f}")

w = fit(X, y)
Xt, yt, ct, _ = load(sys.argv[2])
pt = predict(w, Xt)
print(f"\neval TUNE pairs {len(yt)} (positives {yt.mean():.3f}): AUROC model {auroc(pt, yt):.3f}  ECE {ece(pt, yt):.3f}")
cp, cr, cf = prf(ct, yt)
print(f"  code arm        P {cp:.3f} R {cr:.3f} F1 {cf:.3f}")
for th in (0.3, 0.4, 0.5, 0.6, 0.7):
    mp, mr, mf = prf((pt >= th).astype(float), yt)
    print(f"  model @ {th:.1f}     P {mp:.3f} R {mr:.3f} F1 {mf:.3f}")
print("\nweights (all train sources):")
print(f"  {'bias':<16} {w[0]:+.3f}")
for name, v in sorted(zip(FEATURES, w[1:]), key=lambda t: -abs(t[1])):
    print(f"  {name:<16} {v:+.3f}")
if len(sys.argv) > 3:
    json.dump({"version": 1, "model": "logistic", "features": FEATURES, "bias": w[0], "weights": dict(zip(FEATURES, w[1:].tolist())),
               "trained_on": sorted(set(src)), "rows": int(len(y))}, open(sys.argv[3], "w"), indent=2)
    print("wrote", sys.argv[3])
