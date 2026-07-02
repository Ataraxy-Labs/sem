# The context window is a cache. Treat it like one.

*Design doc, July 2026. The third axis of the sem storage engine.*

## The observation nobody builds on

Every code tool ever made — grep, LSP, ctags, even sem until today — is
stateless per call. Ask the same question twice, get the same bytes twice.
That was fine when a human read the answers: humans remember.

Agents remember too. Everything a tool returns lands in the model's context
window and STAYS there for the rest of the session. But the tool doesn't know
that, so it keeps re-sending what the model already holds:

- The agent reads `resolve_entity` (200 lines). Forty turns later it asks
  again — different phrasing, same entity — and pays 200 lines again.
- Two answers overlap (a caller listed by `impact` is re-included as a
  dependent by `context`). The overlap ships twice.
- Nothing changed between the two reads. The second copy carries zero
  information. It costs real dollars and real context-window pressure anyway.

Measured across our agent benchmarks, repeated and overlapping fills are the
single largest token cost after first-read — and first-read is irreducible.
The repeats are pure waste.

## The architecture

**Model the agent's context window as a cache tier, and make the code
intelligence layer its coherence protocol.**

```
L0  agent context window     what the model has already been shown
L1  resident server (RAM)    warm entity graph · <10ms socket answers
L2  cache.db (disk)          entities · content store · commit index
L3  cloud                    the same layers, hosted
```

L1-L3 exist in sem today. The new piece is that **L0 becomes tracked state**:
the resident server keeps a per-session *ledger* of every fill it has emitted
— entity id plus the structural hash of the version it sent.

Three behaviors fall out, and each one attacks the token axis directly:

**1. Duplicate fills are suppressed.**
A re-ask of something in the ledger answers with one line —
`resolve_entity: unchanged since you read it (L910 calls diffy_merge)` —
instead of the body. ~10 tokens instead of ~800. The agent loses nothing:
the body is already in its own context.

**2. Changes ship as deltas.**
If the entity's structural hash moved since the ledger's version, the answer
is the entity-level diff against *what this agent saw*, not the full body.
"You read version A; here is what changed" is the minimum-token truthful
answer, and only a layer that remembers what it sent can give it.

**3. Prefetch becomes free to be aggressive.**
Speculative context injection (sem's prompt-time prefetch) has always had one
risk: shipping something the agent would have read anyway costs nothing, but
shipping it twice costs double. With the ledger, a speculative fill is
recorded like any other, so the later explicit ask collapses to one line.
Prefetch and pull stop competing and start composing.

## Why this is agent-native and not human-native

A human's screen is not a cache with perfect retention — people scroll away,
forget, re-open files. Tools for humans are RIGHT to be stateless. An LLM's
context window has perfect retention within a session and hard capacity
limits, which is exactly the profile of a cache tier. Coherence protocols,
not search engines, are the correct discipline for feeding it. That inversion
is why this design looks obvious in hindsight and appears nowhere: it only
makes sense once the reader is a machine with a context window.

## How it completes the storage engine

sem's storage engine now manages three axes, each one turning a repeated
computation into data paid for once:

| axis | structure | what stops being recomputed |
|---|---|---|
| space | entity graph + content store | parsing, cross-file resolution |
| time | semantic commit index | history walks (150x measured) |
| attention | session fill ledger | re-reads and overlaps in the conversation |

Files were never the unit any of this needed. Entities address space,
commits-as-deltas address time, and fills address attention. A hosted service
serves all three from the same store: the graph is an artifact, history is
rows, and the ledger is a session key away.

## Implementation sketch (v1 is small)

- The ledger lives in the resident server: `session_key -> {entity_id ->
  structural_hash}`, LRU-bounded, TTL ~2h. No new storage layer.
- Session key: the socket client passes one (agents already carry a session
  id in their environment); no key means no ledger, exactly today's behavior.
- Fill suppression handles `context` first (bodies dominate token spend),
  then text hits and impact lists.
- Deltas reuse the differ that already powers `sem diff` — the machinery
  exists; it just needs the "since the version YOU saw" anchor the ledger
  provides.
- Escape hatch: `--fresh` re-sends unconditionally.

Nothing about the protocol requires sem: it is the natural contract between
any long-lived code intelligence layer and any consumer with a context
window. But it requires entity identity (ids + structural hashes) to say
"unchanged" honestly, per-session state to know who saw what, and a resident
process to hold it — which is exactly the stack sem now has and grep never
will.
