#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"

resolve_repo_path() {
  python3 - "$repo_root" "$1" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[2])
if not path.is_absolute():
    path = pathlib.Path(sys.argv[1]) / path
print(path.resolve(strict=False))
PY
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

sem_bin="$(resolve_repo_path "${SEM_BIN:-crates/target/debug/sem}")"
fixture_root="$(resolve_repo_path "${SEM_FIXTURE_ROOT:-.ota/pressure/sem-stale-index}")"
evidence_dir="$(resolve_repo_path "${SEM_EVIDENCE_DIR:-.ota/evidence/sem-stale-index}")"

: "${SEM_CACHE_DIR:?SEM_CACHE_DIR must name the isolated Ota-owned cache}"
SEM_CACHE_DIR="$(resolve_repo_path "$SEM_CACHE_DIR")"

require_owned_path() {
  case "$1" in
    "$repo_root"/"$2"/*) ;;
    *)
      echo "$3 must stay under the repository's $2 directory" >&2
      exit 1
      ;;
  esac
}

require_owned_path "$SEM_CACHE_DIR" .ota/cache SEM_CACHE_DIR
require_owned_path "$fixture_root" .ota/pressure SEM_FIXTURE_ROOT
require_owned_path "$evidence_dir" .ota/evidence SEM_EVIDENCE_DIR

rm -rf "$fixture_root" "$evidence_dir" "$SEM_CACHE_DIR"
mkdir -p "$fixture_root/src" "$evidence_dir"

git -C "$fixture_root" init --quiet --initial-branch=main
git -C "$fixture_root" config user.name "Ota pressure fixture"
git -C "$fixture_root" config user.email "pressure@ota.run"
git -C "$fixture_root" config commit.gpgsign false

cat > "$fixture_root/src/graph.ts" <<'REVISION_A'
export function legacyDependency(): string {
  return "revision-a";
}

export function selectedEntry(): string {
  return legacyDependency();
}
REVISION_A
touch -t 202601010101 "$fixture_root/src/graph.ts"
git -C "$fixture_root" add src/graph.ts
GIT_AUTHOR_DATE=2026-01-01T01:01:00Z GIT_COMMITTER_DATE=2026-01-01T01:01:00Z \
  git -C "$fixture_root" commit --quiet -m "fixture: revision a"
revision_a="$(git -C "$fixture_root" rev-parse HEAD)"

"$sem_bin" graph "$fixture_root" --json > "$evidence_dir/revision-a-graph.json"
index_path="$(find "$SEM_CACHE_DIR" -type f -name index.sem -print -quit)"
test -n "$index_path"
revision_a_index_sha256="$(sha256_file "$index_path")"
(
  cd "$fixture_root"
  "$sem_bin" find legacyDependency --json
) > "$evidence_dir/revision-a-query.json" 2> "$evidence_dir/revision-a-query.stderr"

cat > "$fixture_root/src/graph.ts" <<'REVISION_B'
export function currentDependency(): string {
  return "revision-b";
}

export function selectedEntry(): string {
  return currentDependency();
}
REVISION_B
touch -t 202601010102 "$fixture_root/src/graph.ts"
git -C "$fixture_root" add src/graph.ts
GIT_AUTHOR_DATE=2026-01-01T01:02:00Z GIT_COMMITTER_DATE=2026-01-01T01:02:00Z \
  git -C "$fixture_root" commit --quiet -m "fixture: revision b"
revision_b="$(git -C "$fixture_root" rev-parse HEAD)"

(
  cd "$fixture_root"
  "$sem_bin" find currentDependency --json
) > "$evidence_dir/revision-b-pre-refresh-query.json" \
  2> "$evidence_dir/revision-b-pre-refresh-query.stderr"
(
  cd "$fixture_root"
  "$sem_bin" find legacyDependency --json
) > "$evidence_dir/revision-b-pre-refresh-stale-query.json" \
  2> "$evidence_dir/revision-b-pre-refresh-stale-query.stderr"
revision_b_pre_refresh_index_sha256="$(sha256_file "$index_path")"
"$sem_bin" graph "$fixture_root" --json > "$evidence_dir/revision-b-graph.json"
revision_b_graph_index_sha256="$(sha256_file "$index_path")"
(
  cd "$fixture_root"
  "$sem_bin" find currentDependency --json
) > "$evidence_dir/revision-b-query.json" 2> "$evidence_dir/revision-b-query.stderr"
(
  cd "$fixture_root"
  "$sem_bin" find legacyDependency --json
) > "$evidence_dir/revision-b-stale-query.json" 2> "$evidence_dir/revision-b-stale-query.stderr"

python3 - "$evidence_dir" "$revision_a" "$revision_b" \
  "$revision_a_index_sha256" "$revision_b_pre_refresh_index_sha256" \
  "$revision_b_graph_index_sha256" <<'PY'
import json
import pathlib
import sys

evidence = pathlib.Path(sys.argv[1])
revision_a, revision_b, index_a, index_b_pre_refresh, index_b_graph = sys.argv[2:]
if revision_a == revision_b:
    raise SystemExit("fixture revisions must differ")
if index_a == index_b_graph:
    raise SystemExit("revision B graph did not replace revision A's warmed index")

graph_a = json.loads((evidence / "revision-a-graph.json").read_text())
graph_b = json.loads((evidence / "revision-b-graph.json").read_text())
query_a = json.loads((evidence / "revision-a-query.json").read_text())
pre_refresh_query_b = json.loads(
    (evidence / "revision-b-pre-refresh-query.json").read_text()
)
pre_refresh_stale_query_b = json.loads(
    (evidence / "revision-b-pre-refresh-stale-query.json").read_text()
)
query_b = json.loads((evidence / "revision-b-query.json").read_text())
stale_query_b = json.loads((evidence / "revision-b-stale-query.json").read_text())

def entity_names(graph):
    return {entity["name"] for entity in graph["entities"]}

def named_edges(graph):
    names = {entity["id"]: entity["name"] for entity in graph["entities"]}
    return {
        (names.get(edge["fromEntity"]), names.get(edge["toEntity"]))
        for edge in graph["edges"]
    }

names_a = entity_names(graph_a)
names_b = entity_names(graph_b)
edges_a = named_edges(graph_a)
edges_b = named_edges(graph_b)

if "legacyDependency" not in names_a or "currentDependency" in names_a:
    raise SystemExit("revision A graph does not match its committed fixture")
if "currentDependency" not in names_b or "legacyDependency" in names_b:
    raise SystemExit("revision B graph retained stale revision A entities")
if ("selectedEntry", "legacyDependency") not in edges_a:
    raise SystemExit("revision A graph is missing its selected dependency edge")
if ("selectedEntry", "currentDependency") not in edges_b:
    raise SystemExit("revision B graph is missing its updated dependency edge")
if ("selectedEntry", "legacyDependency") in edges_b:
    raise SystemExit("revision B graph retained the stale revision A edge")
if [row["name"] for row in query_a] != ["legacyDependency"]:
    raise SystemExit("revision A query did not resolve legacyDependency exactly once")
if pre_refresh_stale_query_b:
    raise SystemExit("revision B pre-refresh query returned the stale revision A entity")
if [row["name"] for row in query_b] != ["currentDependency"]:
    raise SystemExit("revision B post-refresh query did not resolve currentDependency exactly once")
if stale_query_b:
    raise SystemExit("revision B post-refresh query retained the stale revision A entity")

summary = {
    "fixture_revision_a": revision_a,
    "fixture_revision_b": revision_b,
    "revision_a_entity_count": len(graph_a["entities"]),
    "revision_b_entity_count": len(graph_b["entities"]),
    "revision_a_expected_edge": ["selectedEntry", "legacyDependency"],
    "revision_b_expected_edge": ["selectedEntry", "currentDependency"],
    "stale_revision_a_edge_present_in_revision_b": False,
    "stale_revision_a_query_present_before_refresh": False,
    "stale_revision_a_query_present_in_revision_b": False,
    "revision_b_pre_refresh_query_count": len(pre_refresh_query_b),
    "revision_a_index_sha256": index_a,
    "revision_b_pre_refresh_index_sha256": index_b_pre_refresh,
    "revision_b_graph_index_sha256": index_b_graph,
}
(evidence / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, sort_keys=True))
PY
