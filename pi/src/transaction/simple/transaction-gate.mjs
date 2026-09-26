export function editFailed(outcome) {
  const d = outcome.details ?? outcome;
  return Boolean(outcome.isError || d.rolledBack || d.failed > 0 ||
    d.results?.some(r => r.isError || r.outcome?.isError));
}
