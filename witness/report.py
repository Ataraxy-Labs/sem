"""Tiered data-flow report: CONFIRMED (with witness), POSSIBLE, UNKNOWN.

    python3 witness/report.py TASKS.json RUNS_DIR [--md]

CONFIRMED only for a task whose RUNS_DIR/<id>/witness/ holds the harness and three passing run results (checked
again here, not trusted from final.json). Tasks never attempted stay POSSIBLE; tasks sem could not turn into a
witness task (`skipped`) are UNKNOWN.
"""
import json
import sys
from pathlib import Path


def witness_ok(wd: Path) -> bool:
    if not wd.is_dir() or not (list(wd.glob("harness.*"))):
        return False
    for i in (1, 2, 3):
        p = wd / f"run-{i}.json"
        if not p.exists() or json.loads(p.read_text()).get("verdict") != "PASS":
            return False
    return True


def tiers(tasks_json: dict, runs: Path):
    out = {"CONFIRMED": [], "POSSIBLE": [], "UNKNOWN": []}
    for t in tasks_json["tasks"]:
        td = runs / t["id"]
        fin = td / "final.json"
        row = {"id": t["id"], "key": t["key"], "source": f"{t['source']['file']}:{t['source']['line']}",
               "sink": f"{t['sink']['file']}:{t['sink']['line']}"}
        if witness_ok(td / "witness"):
            row["witness"] = str(td / "witness")
            out["CONFIRMED"].append(row)
        elif fin.exists() and json.loads(fin.read_text()).get("tier") == "UNKNOWN":
            row["why"] = json.loads(fin.read_text()).get("why")
            out["UNKNOWN"].append(row)
        else:
            row["attempted"] = fin.exists()
            out["POSSIBLE"].append(row)
    for s in tasks_json.get("skipped", []):
        out["UNKNOWN"].append({"key": s["key"], "why": s["why"]})
    return out


def main():
    tj = json.load(open(sys.argv[1]))
    r = tiers(tj, Path(sys.argv[2]))
    if "--md" in sys.argv:
        print(f"CONFIRMED {len(r['CONFIRMED'])} | POSSIBLE {len(r['POSSIBLE'])} | UNKNOWN {len(r['UNKNOWN'])}\n")
        for row in r["CONFIRMED"]:
            print(f"- CONFIRMED {row['key']}\n    witness: {row['witness']}")
    else:
        print(json.dumps({k: v for k, v in r.items()}, indent=1))


if __name__ == "__main__":
    main()
