"""EVAL.md §4 step 3 for the learned judge (DECISIONS.md D28), on the TUNE split only.

Measures expected calibration error (10 equal-width bins) of a frozen weights file on eval
tune pairs and, if it exceeds 0.1, fits Platt scaling p = σ(a·z + b) on those pairs.
Order sensitivity needs no measurement: every feature is symmetric in its two arguments
(unit test `features_are_symmetric`), so both orders give the same probability.
Usage: judge_calibrate.py weights.json pairs-evaltune.jsonl
"""
import json, sys
import numpy as np

w = json.load(open(sys.argv[1]))
rows = [json.loads(l) for l in open(sys.argv[2])]
X = np.array([r["x"] for r in rows], dtype=float)
y = np.array([r["same_commit"] for r in rows], dtype=float)
z = w["bias"] + X @ np.array([w["weights"][f] for f in w["features"]])

def ece(p, bins=10):
    idx = np.minimum((p * bins).astype(int), bins - 1)
    return sum(abs(p[idx == b].mean() - y[idx == b].mean()) * (idx == b).mean() for b in range(bins) if (idx == b).any())

def sig(v):
    return 1 / (1 + np.exp(-v))

# Step 3's per-kind precision, recorded for completeness: the learned model weighs each edge
# kind itself, so these are not used as priors.
for i, kind in enumerate(["F0", "F1", "F2", "F3", "F6"]):
    on = X[:, i] > 0
    print(f"edge {kind}: {int(on.sum()):>6} tune pairs, precision {y[on].mean() if on.any() else float('nan'):.3f}")
before = ece(sig(z))
print(f"tune pairs {len(y)}  positives {y.mean():.3f}  ECE before {before:.3f}")
if before <= 0.1:
    print("ECE <= 0.1: no Platt scaling (EVAL.md §4 step 3)")
    sys.exit(0)
# Two-parameter logistic fit of y on z (Newton, no penalty).
a, b = 1.0, 0.0
for _ in range(100):
    p = sig(a * z + b)
    g = np.array([((p - y) * z).sum(), (p - y).sum()])
    s = p * (1 - p)
    H = np.array([[(s * z * z).sum(), (s * z).sum()], [(s * z).sum(), s.sum()]])
    step = np.linalg.solve(H, g)
    a, b = a - step[0], b - step[1]
    if np.abs(step).max() < 1e-10:
        break
print(f"Platt a {a:.6f} b {b:.6f}  ECE after {ece(sig(a * z + b)):.3f}")
print(f"--platt {a:.6f},{b:.6f}")
