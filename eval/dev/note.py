import json, sys
d = json.load(sys.stdin)
g = d["diagnostics"]
sizes = sorted((len(x) for x in d["groups"]), reverse=True)
print(f"soft={g['soft_edges']} order={g['order_edges']} cyc={g['cycle_merges']} bridge={len(g['bridging_import_atoms'])} orphan={g['orphan_groups']} perr={len(g['parse_errors'])} sizes={sizes[:6]}")
