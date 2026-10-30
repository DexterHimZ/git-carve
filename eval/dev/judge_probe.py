"""Zero-shot probe: can Laya answer carve's same_change question (SPEC §7.3) on real pairs?

Run with a Python that has `laya` installed:  judge_probe.py pairs.jsonl [checkpoint ...]
Pairs come from judge_probe_data.py (TUNE-split windows only). Reports, per checkpoint and
question wording: AUROC on all pairs and on the F2-only pairs (the ones D11 sends to the
judge), mean P(same) by true label, ECE, and seconds per pair.
"""
import json, sys, time
import laya
import torch

DEVICE = "cuda" if torch.cuda.is_available() else "cpu"

INSTRUCTIONS = "Would a careful engineer put these two code edits in the same commit?"
WORDINGS = {
    "plain": {"type": "noul", "instructions": INSTRUCTIONS},
    # README #156 workaround: neutral labels, the meaning stays P(true).
    "A/B labels": {"type": "noul", "instructions": INSTRUCTIONS,
                   "criteria": {"true": "yes, both edits belong to one change",
                                "false": "no, they are separate changes"},
                   "labels": {"true": "A", "false": "B"}},
}
CHECKPOINTS = {"english": None, "typed-decisions": "typed-decisions", "multilingual": "multilingual"}

def auroc(scores, labels):
    pos = [s for s, l in zip(scores, labels) if l]
    neg = [s for s, l in zip(scores, labels) if not l]
    if not pos or not neg:
        return float("nan")
    wins = sum((p > n) + 0.5 * (p == n) for p in pos for n in neg)
    return wins / (len(pos) * len(neg))

def ece(scores, labels, bins=10):
    total, err = len(scores), 0.0
    for b in range(bins):
        idx = [i for i, s in enumerate(scores) if b / bins <= s < (b + 1) / bins or (b == bins - 1 and s == 1.0)]
        if idx:
            conf = sum(scores[i] for i in idx) / len(idx)
            acc = sum(labels[i] for i in idx) / len(idx)
            err += len(idx) / total * abs(conf - acc)
    return err

rows = [json.loads(l) for l in open(sys.argv[1])]
chosen = sys.argv[2:] or list(CHECKPOINTS)
labels = [r["same_commit"] for r in rows]
f2 = [i for i, r in enumerate(rows) if r["source"] == "F2"]
states = [r["state"] for r in rows]
print(f"{len(rows)} pairs ({sum(labels)} same-commit), {len(f2)} F2-only pairs; device {DEVICE}; laya {laya.__version__}")
print(f"{'checkpoint':<16} {'wording':<11} {'AUROC':>6} {'AUROC F2':>9} {'P|same':>7} {'P|diff':>7} {'ECE':>5} {'s/pair':>7}")
for name in chosen:
    sub = CHECKPOINTS[name]
    agent = laya.load("convaiinnovations/laya", subfolder=sub, device=DEVICE)
    for wording, question in WORDINGS.items():
        started = time.time()
        results = agent.predict_batch(states, {"same_change": question}, batch_size=16)
        per_pair = (time.time() - started) / len(rows)
        p = [r["answers"]["same_change"]["noul"] for r in results]
        mean = lambda xs: sum(xs) / max(1, len(xs))
        print(f"{name:<16} {wording:<11} {auroc(p, labels):6.3f} {auroc([p[i] for i in f2], [labels[i] for i in f2]):9.3f} "
              f"{mean([x for x, l in zip(p, labels) if l]):7.3f} {mean([x for x, l in zip(p, labels) if not l]):7.3f} "
              f"{ece(p, labels):5.3f} {per_pair:7.3f}", flush=True)
    del agent
