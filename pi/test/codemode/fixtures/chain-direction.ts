// Deterministic fixture for the path/why DIRECTION semantics.
//
// Directed call edges:
//   source -> m1 -> m2 -> target        (the real 3-hop call chain)
//   hub    -> source, hub -> target     (hub calls BOTH endpoints)
//
// The trap this reproduces: treated undirected, source-hub-target is a
// 2-hop "connection" — SHORTER than the real chain, and not a call chain
// at all (hub calls source; source does not reach target through hub).
// The old undirected default returned exactly that shape on a real repo,
// which is why "out" is now the default and "any" an explicit opt-in.

export function target(): number {
  return 1;
}

export function m2(): number {
  return target() + 1;
}

export function m1(): number {
  return m2() + 1;
}

export function source(): number {
  return m1() + 1;
}

export function hub(): number {
  return source() + target();
}
