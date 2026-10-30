"""Preview of E1 on TUNE-split windows only (EVAL.md split key). Not the M5 harness."""
import hashlib, itertools, json, os, subprocess, sys
BIN = os.environ.get("GIT_CARVE_BIN", os.path.join(os.path.dirname(__file__), "..", "..", "target", "debug", "git-carve"))
ENV = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", LC_ALL="C")
def git(*a, cwd):
    return subprocess.run(["git", *a], cwd=cwd, env=ENV, capture_output=True, text=True, check=True).stdout
def pairs_scores(label, group):
    ids = [a for a in label if a in group]
    tp = fp = fn = 0
    for a, b in itertools.combinations(ids, 2):
        same_c, same_g = label[a] == label[b], group[a] == group[b]
        tp += same_c and same_g; fp += same_g and not same_c; fn += same_c and not same_g
    p = tp / (tp + fp) if tp + fp else 1.0
    r = tp / (tp + fn) if tp + fn else 1.0
    return p, r, (2 * p * r / (p + r) if p + r else 0.0)
def bcubed(label, group):
    ids = [a for a in label if a in group]
    if not ids: return 1.0
    P = sum(sum(1 for b in ids if group[b] == group[a] and label[b] == label[a]) / sum(1 for b in ids if group[b] == group[a]) for a in ids) / len(ids)
    R = sum(sum(1 for b in ids if group[b] == group[a] and label[b] == label[a]) / sum(1 for b in ids if label[b] == label[a]) for a in ids) / len(ids)
    return 2 * P * R / (P + R)
rows = []
edge_stats = {}
base_same = base_total = 0
for repo in sys.argv[1:]:
    root = min(git("rev-list", "--max-parents=0", "origin/HEAD", cwd=repo).split())
    commits = git("rev-list", "--no-merges", "--first-parent", "--reverse", "origin/HEAD", cwd=repo).split()
    taken = 0
    for i in range(1, len(commits) - 2):
        window = commits[i:i + 3]
        base = git("rev-parse", window[0] + "^", cwd=repo).strip()
        if int(hashlib.sha256((root + base).encode()).hexdigest(), 16) % 10 >= 3:
            continue  # test split: never looked at
        authors = {git("log", "-1", "--format=%an", c, cwd=repo).strip() for c in window}
        files = git("diff", "--name-only", base, window[-1], cwd=repo).split("\n")
        if len(authors) > 1 or len([f for f in files if f]) > 15:
            continue
        git("checkout", "-q", "-f", "--detach", base, cwd=repo); git("clean", "-qfdx", cwd=repo)
        git("read-tree", "-u", "--reset", window[-1], cwd=repo); git("reset", "-q", base, cwd=repo)
        out = subprocess.run([BIN, "--plan-only", "--no-check", "--json"], cwd=repo, env=ENV, capture_output=True, text=True)
        if out.returncode != 0:
            continue
        plan = json.loads(out.stdout)
        group = {a: k for k, g in enumerate(plan["groups"]) for a in g}
        label = {}
        for atom in plan["atoms"]:
            if atom["kind"] == "region" and atom["new"][1] > atom["new"][0]:
                lo, hi = atom["new"][0] + 1, atom["new"][1]
            elif atom["kind"] == "new file":
                lo, hi = 1, None
            else:
                continue
            args = ["blame", "--porcelain", window[-1]] + (["-L", f"{lo},{hi}"] if hi else []) + ["--", atom["path"]]
            try:
                blame = git(*args, cwd=repo)
            except subprocess.CalledProcessError:
                continue
            shas = {l.split()[0] for l in blame.splitlines() if len(l.split()) >= 3 and len(l.split()[0]) == 40 and l.split()[1].isdigit()}
            if len(shas) == 1 and shas & set(window):
                label[atom["id"]] = shas.pop()
        if len(set(label.values())) < 2:
            continue  # nothing to separate
        ids = sorted(label)
        for x, y in itertools.combinations(ids, 2):
            base_same += label[x] == label[y]; base_total += 1
        for a, b, kinds in plan.get("edges", []):
            if a in label and b in label:
                for k in kinds:
                    same, total = edge_stats.get(k, (0, 0))
                    edge_stats[k] = (same + (label[a] == label[b]), total + 1)
        one = {a: 0 for a in group}; alone = {a: i for i, a in enumerate(group)}
        rows.append((os.path.basename(repo), base[:7], len(label), len(set(label.values())), len(plan["groups"]),
                     *pairs_scores(label, group), bcubed(label, group), pairs_scores(label, one)[2], pairs_scores(label, alone)[2]))
        taken += 1
        if taken >= 8:
            break
print(f"{'repo':<20} {'window':<8} {'atoms':>5} {'true':>4} {'grp':>4} {'P':>5} {'R':>5} {'F1':>5} {'BC-F1':>6} {'1grp':>5} {'alone':>5}")
for r in rows:
    print(f"{r[0][:20]:<20} {r[1]:<8} {r[2]:>5} {r[3]:>4} {r[4]:>4} {r[5]:>5.2f} {r[6]:>5.2f} {r[7]:>5.2f} {r[8]:>6.2f} {r[9]:>5.2f} {r[10]:>5.2f}")
if rows:
    m = lambda k: sum(r[k] for r in rows) / len(rows)
    print(f"{'MEAN (' + str(len(rows)) + ' tune windows)':<34} {'':>4} {'':>4} {m(5):>5.2f} {m(6):>5.2f} {m(7):>5.2f} {m(8):>6.2f} {m(9):>5.2f} {m(10):>5.2f}")

print()
print("Soft-edge precision on tune windows (share of edges joining atoms of the same real commit):")
for k in sorted(edge_stats):
    same, total = edge_stats[k]
    print(f"  {k}: {same}/{total} = {same / total:.2f}")
print(f"  base rate (any two labeled atoms share a commit): {base_same}/{base_total} = {base_same / base_total:.2f}")
