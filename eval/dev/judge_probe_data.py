"""Builds same_change probe pairs from TUNE-split windows only (EVAL.md split key).

Each pair is two atoms from one window, labeled same/different commit from `git blame`,
with the judge state SPEC §7.3 describes. Pairs come from carve's own F2 edges (the pairs
DECISIONS.md D11 hands to the judge) plus a balanced sample of other in-window pairs.
Output: JSONL on stdout. Usage: judge_probe_data.py <clone>... > pairs.jsonl
"""
import hashlib, itertools, json, os, random, subprocess, sys

BIN = os.environ.get("GIT_CARVE_BIN", os.path.join(os.path.dirname(__file__), "..", "..", "target", "debug", "git-carve"))
ENV = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", LC_ALL="C")
MAX_DIFF_LINES = 40

def git(*a, cwd, check=True):
    return subprocess.run(["git", *a], cwd=cwd, env=ENV, capture_output=True, text=True, errors="replace", check=check).stdout

def lines_of(text):
    return text.splitlines()

def diff_text(atom, base_lines, new_lines):
    if atom["kind"] != "region":
        return f"({atom['kind']}: {atom['path']})"
    (bs, be), (ns, ne) = atom["base"], atom["new"]
    out = [f"-{l}" for l in base_lines[bs:be]] + [f"+{l}" for l in new_lines[ns:ne]]
    return "\n".join(out[:MAX_DIFF_LINES])

rng = random.Random(20260924)
for repo in sys.argv[1:]:
    root = min(git("rev-list", "--max-parents=0", "origin/HEAD", cwd=repo).split())
    commits = git("rev-list", "--no-merges", "--first-parent", "--reverse", "origin/HEAD", cwd=repo).split()
    for i in range(1, len(commits) - 2):
        window = commits[i:i + 3]
        base = git("rev-parse", window[0] + "^", cwd=repo).strip()
        if int(hashlib.sha256((root + base).encode()).hexdigest(), 16) % 10 >= 3:
            continue  # test split: never looked at
        authors = {git("log", "-1", "--format=%an", c, cwd=repo).strip() for c in window}
        files = [f for f in git("diff", "--name-only", base, window[-1], cwd=repo).split("\n") if f]
        if len(authors) > 1 or len(files) > 15:
            continue
        git("checkout", "-q", "-f", "--detach", base, cwd=repo); git("clean", "-qfdx", cwd=repo)
        git("read-tree", "-u", "--reset", window[-1], cwd=repo); git("reset", "-q", base, cwd=repo)
        out = subprocess.run([BIN, "--plan-only", "--no-check", "--json"], cwd=repo, env=ENV, capture_output=True, text=True)
        if out.returncode != 0:
            continue
        plan = json.loads(out.stdout)
        atoms = {a["id"]: a for a in plan["atoms"]}
        group = {a: k for k, g in enumerate(plan["groups"]) for a in g}
        label, text = {}, {}
        cache = {}
        for a in plan["atoms"]:
            if a["kind"] == "region" and a["new"][1] > a["new"][0]:
                lo, hi = a["new"][0] + 1, a["new"][1]
                blame = git("blame", "--porcelain", "-L", f"{lo},{hi}", window[-1], "--", a["path"], cwd=repo, check=False)
            elif a["kind"] == "new file":
                blame = git("blame", "--porcelain", window[-1], "--", a["path"], cwd=repo, check=False)
            else:
                continue
            shas = {l.split()[0] for l in blame.splitlines() if len(l.split()) >= 3 and len(l.split()[0]) == 40 and l.split()[1].isdigit()}
            if len(shas) == 1 and shas & set(window):
                label[a["id"]] = shas.pop()
                if a["path"] not in cache:
                    old = git("show", f"{base}:{a['path']}", cwd=repo, check=False)
                    new = open(os.path.join(repo, a["path"]), encoding="utf-8", errors="replace").read() if os.path.exists(os.path.join(repo, a["path"])) else ""
                    cache[a["path"]] = (lines_of(old), lines_of(new))
                text[a["id"]] = diff_text(a, *cache[a["path"]])
        if len(set(label.values())) < 2:
            continue
        f2 = {(a, b) for a, b, kinds in plan.get("edges", []) if kinds == ["F2"] and a in label and b in label}
        others = [p for p in itertools.combinations(sorted(label, key=lambda x: int(x[1:])), 2) if p not in f2]
        same = [p for p in others if label[p[0]] == label[p[1]]]
        diff = [p for p in others if label[p[0]] != label[p[1]]]
        k = min(len(same), len(diff), 6)
        chosen = [(p, "F2") for p in sorted(f2)] + [(p, "sample") for p in rng.sample(same, k) + rng.sample(diff, k)]
        for (a, b), source in chosen:
            A, B = atoms[a], atoms[b]
            line_distance = None
            if A["path"] == B["path"] and A["kind"] == "region" and B["kind"] == "region":
                line_distance = abs(A["new"][0] - B["new"][0])
            state = {
                "path_a": A["path"], "path_b": B["path"],
                "enclosing_a": A.get("symbol"), "enclosing_b": B.get("symbol"),
                "line_distance": line_distance,
                "diff_a": text[a], "diff_b": text[b],
            }
            print(json.dumps({
                "repo": os.path.basename(repo), "window": base[:7], "a": a, "b": b,
                "source": source, "same_commit": label[a] == label[b],
                "carve_same_group": group.get(a) == group.get(b), "state": state,
            }))
