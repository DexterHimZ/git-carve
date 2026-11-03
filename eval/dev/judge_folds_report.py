"""DECISIONS.md D38: the pre-registered rung-1.5 comparison, from an `eval sweep` file.

For each variant and held-out repository R, theta is chosen on the other repositories' windows
(maximum mean BCubed F1, using the weights trained without R), then R's windows are scored at
that theta. Windows are split into 2-4 and 5-8 commits. The adoption rule is applied
mechanically, relative to r1.
Usage: judge_folds_report.py sweep.jsonl
"""
import collections, json, sys

rows = [json.loads(l) for l in open(sys.argv[1])]
windows = {}
scores = collections.defaultdict(dict)  # (judge, theta) -> window -> f1
for r in rows:
    windows[r["window"]] = (r["source"], r["commits"])
    scores[(r["judge"], r["theta"])][r["window"]] = r["f1"]
repos = sorted({s for s, _ in windows.values()})
variants = sorted({j.split("__")[0] for j, _ in scores if "__" in j})
thetas = sorted({t for j, t in scores if t is not None})
stratum = lambda w: "2-4" if windows[w][1] <= 4 else "5-8"
mean = lambda v: sum(v) / len(v) if v else float("nan")

# held[variant][repo][stratum] -> list of held-out f1 at the fold's chosen theta
held = collections.defaultdict(lambda: collections.defaultdict(lambda: collections.defaultdict(list)))
chosen = collections.defaultdict(dict)
for v in variants:
    for R in repos:
        judge = f"{v}__{R}"
        others = [w for w in windows if windows[w][0] != R]
        best = max(thetas, key=lambda t: (mean([scores[(judge, t)][w] for w in others if w in scores[(judge, t)]]), -t))
        chosen[v][R] = best
        for w, f in scores[(judge, best)].items():
            if windows[w][0] == R:
                held[v][R][stratum(w)].append(f)
for w, f in scores[("code", None)].items():
    held["code"][windows[w][0]][stratum(w)].append(f)

counts = collections.Counter(stratum(w) for w in scores[("code", None)])
print(f"windows scored: {dict(counts)} across {len(repos)} held-out repositories\n")
print(f"{'arm':<9} {'2-4':>7} {'5-8':>7} {'all':>7}   theta per fold")
pooled = {}
for v in ["code"] + variants:
    s24 = [f for R in repos for f in held[v][R]["2-4"]]
    s58 = [f for R in repos for f in held[v][R]["5-8"]]
    pooled[v] = (mean(s24), mean(s58))
    th = " ".join(f"{chosen[v][R]:.2f}" for R in repos) if v != "code" else ""
    print(f"{v:<9} {mean(s24):>7.3f} {mean(s58):>7.3f} {mean(s24 + s58):>7.3f}   {th}")

print(f"\nper held-out repository, 5-8 windows (and 2-4):")
print(f"{'arm':<9} " + " ".join(f"{R:>22}" for R in repos))
for v in ["code"] + variants:
    print(f"{v:<9} " + " ".join(f"{mean(held[v][R]['5-8']):>10.3f} ({mean(held[v][R]['2-4']):.3f})" for R in repos))

print("\nadoption rule (relative to r1; D38):")
base24, base58 = pooled["r1"]
adopted = []
for v in variants:
    if v == "r1":
        continue
    g58 = pooled[v][1] - base58
    wins = sum(mean(held[v][R]["5-8"]) >= mean(held["r1"][R]["5-8"]) for R in repos if held[v][R]["5-8"])
    loss24 = pooled[v][0] - base24
    ok = g58 >= 0.01 and wins >= 4 and loss24 >= -0.005
    print(f"  {v:<9} 5-8 gain {g58:+.3f}, not worse on {wins}/{len(repos)} repos, 2-4 change {loss24:+.3f} -> {'ADOPT' if ok else 'reject'}")
    if ok:
        adopted.append(v)
if adopted:
    best = max(adopted, key=lambda v: pooled[v][1])
    floors = [v for v in adopted if v.endswith("floor") and pooled[v][1] >= pooled[best][1] - 0.005]
    pick = max(floors, key=lambda v: pooled[v][1]) if floors else best
    print(f"candidate: {pick}")
else:
    print("candidate: none; rung 1 stands")
