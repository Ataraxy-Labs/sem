import path from "node:path";
import { checkDependents } from "../../tools/internal/impact.ts";

const MAX_CHECKED_EDITS = 8;
const MAX_REPORTED = 20;

// Edits that can strand callers: deletions and intentional signature changes.
export function signatureEdits(edits) {
  return (edits ?? []).filter((edit) =>
    edit.entity?.name && (edit.op === "delete" || edit.allow_signature_change === true),
  );
}

// Dependents not themselves edited in the same batch. Matching is by file and
// entity name on sem's syntax-level graph, so it is a review list, not a proof.
export function uncoveredDependents(dependents, edits) {
  const edited = new Set((edits ?? []).map((edit) => `${edit.file}\0${edit.entity?.name}`));
  const seen = new Set();
  return (dependents ?? []).filter((dependent) => {
    const key = `${dependent.file}\0${dependent.name}`;
    if (edited.has(key) || seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

// Captures dependents before the batch runs, because a rename or deletion
// removes the name the graph would be queried by afterwards.
export async function dependentsBeforeEdit(edits, cwd, semBin = "sem", check = checkDependents) {
  const targets = signatureEdits(edits);
  const found = [];
  let checked = 0, failed = 0;
  for (const edit of targets.slice(0, MAX_CHECKED_EDITS)) {
    const result = await check(semBin, cwd, path.resolve(cwd, edit.file), edit.entity.name);
    if (!result.ok) { failed++; continue; }
    checked++;
    found.push(...result.dependents);
  }
  return { targets: targets.length, checked, failed, skipped: Math.max(0, targets.length - MAX_CHECKED_EDITS), dependents: found };
}

export function callerReview(before, edits) {
  if (!before || before.targets === 0) return null;
  const uncovered = uncoveredDependents(before.dependents, edits);
  return {
    signature_edits: before.targets,
    checked: before.checked,
    unavailable: before.failed + before.skipped,
    unedited_dependents: uncovered.slice(0, MAX_REPORTED).map(({ file, name, type }) => ({ file, name, type })),
    total_unedited: uncovered.length,
    scope: "Syntax-level graph references to renamed, re-signatured or deleted entities that this batch did not edit. Review or update them before validating; this is not a compile check.",
  };
}
