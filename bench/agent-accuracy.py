#!/usr/bin/env python3
"""Agent accuracy benchmark: sem diff (structured JSON) vs git diff (raw line diff).

Sends identical questions to Claude about code changes, using sem diff output vs
git diff output as context. Scores responses against ground truth extracted from
sem diff JSON.

Usage:
    export ANTHROPIC_API_KEY=...
    python bench/agent-accuracy.py

Dependencies: anthropic
"""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

try:
    import anthropic
except ImportError:
    print("Install the Anthropic SDK: pip install anthropic")
    sys.exit(1)

# ── Config ──────────────────────────────────────────────────────────────────

MODEL = "claude-sonnet-4-5-20250929"
TEMPERATURE = 0
MAX_GIT_DIFF_BYTES = 100_000
SEM_BINARY = str(Path(__file__).resolve().parent.parent / "crates" / "target" / "release" / "sem")
REPO_DIR = str(Path(__file__).resolve().parent.parent)

COMMITS = [
    {"sha": "9f7f1c7", "label": "7 new commands (11 files)"},
    {"sha": "fffb38f", "label": "Speed optimization (mixed ops)"},
    {"sha": "ae576ab", "label": "Rust rewrite (large)"},
]

QUESTIONS = [
    {
        "id": "q1_added_functions",
        "text": "List all functions that were ADDED (not modified) in this diff. Return a JSON object: {\"functions\": [\"name1\", \"name2\", ...]}. Only include function names, not methods or other entity types. Return ONLY the JSON, no explanation.",
        "type": "set_f1",
    },
    {
        "id": "q2_files_with_modified",
        "text": "List all files that contain at least one MODIFIED (not added or deleted) entity. Return a JSON object: {\"files\": [\"path/to/file1\", ...]}. Return ONLY the JSON, no explanation.",
        "type": "set_f1",
    },
    {
        "id": "q3_entity_type_counts",
        "text": "Count the number of changed entities grouped by entity type (e.g. function, class, interface, etc). Return a JSON object: {\"counts\": {\"function\": 5, \"class\": 2, ...}}. Return ONLY the JSON, no explanation.",
        "type": "dict_accuracy",
    },
    {
        "id": "q4_change_type_counts",
        "text": "Count the total number of added, modified, and deleted entities in this diff. Return a JSON object: {\"added\": N, \"modified\": N, \"deleted\": N}. Return ONLY the JSON, no explanation.",
        "type": "exact_match",
    },
]

# ── Helpers ──────────────────────────────────────────────────────────────────


def run(cmd: list[str], cwd: str = REPO_DIR) -> str:
    result = subprocess.run(cmd, capture_output=True, text=True, cwd=cwd)
    if result.returncode != 0:
        print(f"  Command failed: {' '.join(cmd)}", file=sys.stderr)
        print(f"  stderr: {result.stderr[:500]}", file=sys.stderr)
    return result.stdout


def get_sem_diff_json(sha: str) -> dict:
    raw = run([SEM_BINARY, "diff", "--commit", sha, "--format", "json"])
    return json.loads(raw)


def get_git_diff(sha: str) -> str:
    diff = run(["git", "diff", f"{sha}~1", sha])
    if len(diff) > MAX_GIT_DIFF_BYTES:
        diff = diff[:MAX_GIT_DIFF_BYTES] + f"\n\n... [truncated at {MAX_GIT_DIFF_BYTES // 1000}KB] ..."
    return diff


def strip_content(sem_json: dict) -> dict:
    """Remove beforeContent/afterContent for a fairer comparison — tests structure, not passthrough."""
    stripped = {"changes": [], "summary": sem_json.get("summary", {})}
    for change in sem_json["changes"]:
        c = {k: v for k, v in change.items() if k not in ("beforeContent", "afterContent")}
        stripped["changes"].append(c)
    return stripped


def extract_ground_truth(sem_json: dict, question_id: str):
    changes = sem_json["changes"]

    if question_id == "q1_added_functions":
        return sorted(set(
            c["entityName"] for c in changes
            if c["changeType"] == "added" and c["entityType"] == "function"
        ))

    if question_id == "q2_files_with_modified":
        return sorted(set(
            c["filePath"] for c in changes
            if c["changeType"] == "modified"
        ))

    if question_id == "q3_entity_type_counts":
        counts: dict[str, int] = {}
        for c in changes:
            t = c["entityType"]
            counts[t] = counts.get(t, 0) + 1
        return counts

    if question_id == "q4_change_type_counts":
        result = {"added": 0, "modified": 0, "deleted": 0}
        for c in changes:
            ct = c["changeType"]
            if ct in result:
                result[ct] += 1
        return result

    return None


# ── Scoring ──────────────────────────────────────────────────────────────────


def score_set_f1(predicted: list, truth: list) -> dict:
    pred_set = set(predicted)
    truth_set = set(truth)
    if not truth_set:
        return {"precision": 1.0, "recall": 1.0, "f1": 1.0} if not pred_set else {"precision": 0.0, "recall": 1.0, "f1": 0.0}
    tp = len(pred_set & truth_set)
    precision = tp / len(pred_set) if pred_set else 0.0
    recall = tp / len(truth_set) if truth_set else 0.0
    f1 = 2 * precision * recall / (precision + recall) if (precision + recall) > 0 else 0.0
    return {"precision": round(precision, 4), "recall": round(recall, 4), "f1": round(f1, 4)}


def score_dict_accuracy(predicted: dict, truth: dict) -> dict:
    all_keys = set(list(predicted.keys()) + list(truth.keys()))
    if not all_keys:
        return {"accuracy": 1.0, "per_type": {}}
    per_type = {}
    total_acc = 0.0
    for k in all_keys:
        p = predicted.get(k, 0)
        t = truth.get(k, 0)
        max_val = max(abs(p), abs(t), 1)
        acc = 1.0 - abs(p - t) / max_val
        per_type[k] = round(acc, 4)
        total_acc += acc
    avg = total_acc / len(all_keys)
    return {"accuracy": round(avg, 4), "per_type": per_type}


def score_exact_match(predicted: dict, truth: dict) -> dict:
    fields = ["added", "modified", "deleted"]
    matches = sum(1 for f in fields if predicted.get(f) == truth.get(f))
    return {
        "score": round(matches / len(fields), 4),
        "fields": {f: predicted.get(f) == truth.get(f) for f in fields},
        "predicted": {f: predicted.get(f) for f in fields},
        "truth": {f: truth.get(f) for f in fields},
    }


def score(question_type: str, predicted, truth) -> dict:
    if question_type == "set_f1":
        return score_set_f1(predicted, truth)
    if question_type == "dict_accuracy":
        return score_dict_accuracy(predicted, truth)
    if question_type == "exact_match":
        return score_exact_match(predicted, truth)
    return {}


# ── API ──────────────────────────────────────────────────────────────────────


def ask_claude(client: anthropic.Anthropic, context: str, question: str) -> str:
    resp = client.messages.create(
        model=MODEL,
        max_tokens=4096,
        temperature=TEMPERATURE,
        messages=[
            {
                "role": "user",
                "content": f"Here is a diff of code changes:\n\n{context}\n\n{question}",
            }
        ],
    )
    return resp.content[0].text


def parse_response(raw: str, question_id: str):
    """Extract the JSON from Claude's response."""
    # Try to find JSON in the response
    text = raw.strip()
    # Strip markdown code fences
    if text.startswith("```"):
        lines = text.split("\n")
        lines = [l for l in lines if not l.strip().startswith("```")]
        text = "\n".join(lines).strip()

    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        # Try to find JSON object in text
        start = text.find("{")
        end = text.rfind("}") + 1
        if start >= 0 and end > start:
            try:
                data = json.loads(text[start:end])
            except json.JSONDecodeError:
                return None
        else:
            return None

    if question_id == "q1_added_functions":
        return data.get("functions", [])
    if question_id == "q2_files_with_modified":
        return data.get("files", [])
    if question_id == "q3_entity_type_counts":
        return data.get("counts", data)
    if question_id == "q4_change_type_counts":
        return data

    return data


# ── Main ─────────────────────────────────────────────────────────────────────


def main():
    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if not api_key:
        print("Set ANTHROPIC_API_KEY environment variable")
        sys.exit(1)

    client = anthropic.Anthropic(api_key=api_key)

    print(f"Agent Accuracy Benchmark: sem diff vs git diff")
    print(f"Model: {MODEL}")
    print(f"Commits: {len(COMMITS)} | Questions: {len(QUESTIONS)}")
    print(f"Total API calls: {len(COMMITS) * len(QUESTIONS) * 2}")
    print()

    all_results = []
    sem_scores_by_q: dict[str, list[float]] = {q["id"]: [] for q in QUESTIONS}
    git_scores_by_q: dict[str, list[float]] = {q["id"]: [] for q in QUESTIONS}

    for commit in COMMITS:
        sha = commit["sha"]
        print(f"── {sha}: {commit['label']} ──")

        # Get diffs
        sem_json = get_sem_diff_json(sha)
        git_diff = get_git_diff(sha)
        sem_stripped = strip_content(sem_json)
        sem_context = json.dumps(sem_stripped, indent=2)

        print(f"  sem: {len(sem_json['changes'])} entities | git: {len(git_diff)} chars")

        for q in QUESTIONS:
            truth = extract_ground_truth(sem_json, q["id"])

            # Ask with sem diff
            print(f"  {q['id']} [sem]...", end="", flush=True)
            sem_raw = ask_claude(client, sem_context, q["text"])
            sem_parsed = parse_response(sem_raw, q["id"])
            if sem_parsed is None:
                print(" PARSE FAIL", end="")
                sem_parsed = [] if q["type"] == "set_f1" else {}
            sem_score = score(q["type"], sem_parsed, truth)
            print(f" done", end="", flush=True)

            # Ask with git diff
            print(f" | [git]...", end="", flush=True)
            git_raw = ask_claude(client, git_diff, q["text"])
            git_parsed = parse_response(git_raw, q["id"])
            if git_parsed is None:
                print(" PARSE FAIL", end="")
                git_parsed = [] if q["type"] == "set_f1" else {}
            git_score = score(q["type"], git_parsed, truth)
            print(f" done")

            # Extract the single score number
            sem_num = sem_score.get("f1", sem_score.get("accuracy", sem_score.get("score", 0)))
            git_num = git_score.get("f1", git_score.get("accuracy", git_score.get("score", 0)))
            sem_scores_by_q[q["id"]].append(sem_num)
            git_scores_by_q[q["id"]].append(git_num)

            all_results.append({
                "commit": sha,
                "commit_label": commit["label"],
                "question_id": q["id"],
                "question_text": q["text"],
                "question_type": q["type"],
                "ground_truth": truth if not isinstance(truth, list) or len(truth) < 50 else f"[{len(truth)} items]",
                "sem": {
                    "raw_response": sem_raw,
                    "parsed": sem_parsed if not isinstance(sem_parsed, list) or len(sem_parsed) < 50 else f"[{len(sem_parsed)} items]",
                    "score": sem_score,
                },
                "git": {
                    "raw_response": git_raw,
                    "parsed": git_parsed if not isinstance(git_parsed, list) or len(git_parsed) < 50 else f"[{len(git_parsed)} items]",
                    "score": git_score,
                },
            })

        print()

    # ── Summary ──────────────────────────────────────────────────────────────

    print("=" * 72)
    print(f"{'Question':<30} {'sem':>8} {'git':>8} {'delta':>8}")
    print("-" * 72)

    summary = {}
    for q in QUESTIONS:
        sem_avg = sum(sem_scores_by_q[q["id"]]) / len(sem_scores_by_q[q["id"]])
        git_avg = sum(git_scores_by_q[q["id"]]) / len(git_scores_by_q[q["id"]])
        delta = sem_avg - git_avg
        label = q["id"].replace("_", " ").replace("q1 ", "Q1: ").replace("q2 ", "Q2: ").replace("q3 ", "Q3: ").replace("q4 ", "Q4: ")
        marker = " *" if delta > 0.05 else ""
        print(f"{label:<30} {sem_avg:>7.1%} {git_avg:>7.1%} {delta:>+7.1%}{marker}")
        summary[q["id"]] = {
            "sem_avg": round(sem_avg, 4),
            "git_avg": round(git_avg, 4),
            "delta": round(delta, 4),
            "sem_scores": sem_scores_by_q[q["id"]],
            "git_scores": git_scores_by_q[q["id"]],
        }

    # Overall
    all_sem = [s for scores in sem_scores_by_q.values() for s in scores]
    all_git = [s for scores in git_scores_by_q.values() for s in scores]
    sem_overall = sum(all_sem) / len(all_sem)
    git_overall = sum(all_git) / len(all_git)
    print("-" * 72)
    print(f"{'Overall':<30} {sem_overall:>7.1%} {git_overall:>7.1%} {sem_overall - git_overall:>+7.1%}")
    print()

    summary["overall"] = {
        "sem_avg": round(sem_overall, 4),
        "git_avg": round(git_overall, 4),
        "delta": round(sem_overall - git_overall, 4),
    }

    # ── Write results ────────────────────────────────────────────────────────

    output = {
        "model": MODEL,
        "temperature": TEMPERATURE,
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "commits": COMMITS,
        "summary": summary,
        "results": all_results,
    }

    out_path = Path(__file__).resolve().parent / "agent-accuracy-results.json"
    with open(out_path, "w") as f:
        json.dump(output, f, indent=2, default=str)
    print(f"Results written to {out_path}")

    # ── Update HTML ──────────────────────────────────────────────────────────

    html_path = Path(__file__).resolve().parent.parent / "docs" / "index.html"
    if html_path.exists():
        html = html_path.read_text()
        # Build the JS data block
        js_data = "{\n"
        for q in QUESTIONS:
            s = summary[q["id"]]
            js_data += f'        {q["id"]}:' + '{ sem: ' + f'{s["sem_avg"]:.4f}, git: {s["git_avg"]:.4f}' + ' },\n'
        js_data += "      }"
        # Replace the placeholder data block
        import re
        html = re.sub(
            r"const data = \{.*?q4_change_type_counts:\s*\{[^}]*\}\s*,?\s*\};",
            f"const data = {js_data};",
            html,
            flags=re.DOTALL,
        )
        html_path.write_text(html)
        print(f"Updated {html_path}")


if __name__ == "__main__":
    main()
