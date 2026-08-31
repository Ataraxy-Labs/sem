import { McpClient, type McpCallResult } from "../../bridge/mcp-client.ts";
import type { EntityQuery } from "./entities.ts";

/**
 * Owns exactly one weave-mcp McpClient for the live coordination tools
 * (weave_agent_register, weave_claim_entity, weave_update_entity_content,
 * weave_release_entity). Started lazily on first use and kept running for
 * the rest of the session — coordination is best-effort: every method here
 * returns a result union instead of throwing, because a coordination
 * failure must never block the disk edit that already happened.
 */

export interface CoordinatorOptions {
  command: string;
  args?: string[];
  /** Working directory to spawn weave-mcp in — it resolves file_path arguments relative to its own cwd's repo root, not the caller's. */
  cwd?: string;
  /** Extra environment variables for the spawned weave-mcp process (forwarded on top of the parent env by McpClient). */
  env?: Record<string, string>;
  requestTimeoutMs?: number;
  agentId: string;
  onStderr?: (chunk: string) => void;
}

export type CoordinationOutcome = { ok: true; text: string; entityId?: string } | { ok: false; reason: string };

/** One same-entity collision from the merge backstop, as the server reports it (snake_case wire keys kept). */
export interface MergeConflictSummary {
  entity_name?: string;
  entity_type?: string;
  kind?: string;
  kind_display?: string;
  complexity?: string;
}

/**
 * The merge backstop's outcome. `conflict: true` is a BUSINESS outcome in
 * an Ok payload (the AlreadyClaimed convention): the server refused to
 * store anything because base->theirs and base->ours collide on the same
 * entity. `ok: false, conflict: false` is transport/protocol failure --
 * the gate could not run at all, which callers treat as advisory (edit
 * proceeds, status tells the truth), never as a merge.
 */
export type MergeCheckOutcome =
  | {
      ok: true;
      /** undefined = the server accepted the update but gave no drift verdict (a pre-backstop server that ignores the snapshot pair). */
      drift: boolean | undefined;
      merged: boolean;
      mergedContent?: string;
      mergedOver?: string[];
      text: string;
    }
  | { ok: false; conflict: true; conflicts: MergeConflictSummary[]; text: string }
  | { ok: false; conflict: false; reason: string };

function extractText(content: McpCallResult["content"] | undefined): string {
  return (content ?? []).map((c) => c.text).join("\n");
}

/**
 * weave_claim_entity's response is a JSON object; a fixed weave-mcp
 * includes `entity_id` — the claim's own stable identity,
 * independent of the entity's current name. Older servers omit the field
 * entirely, and `undefined` here is exactly the signal `updateAndRelease`
 * needs: only send `entity_id` back to a server that proved (by returning
 * one) it understands the field. An older, `deny_unknown_fields` server
 * would reject the whole call over an unrecognized field otherwise.
 */
function parseEntityId(claimResponseText: string): string | undefined {
  try {
    const parsed = JSON.parse(claimResponseText) as { entity_id?: unknown };
    return typeof parsed.entity_id === "string" ? parsed.entity_id : undefined;
  } catch {
    return undefined;
  }
}

/**
 * weave-mcp reports a LOST claim as an ordinary MCP success whose JSON
 * payload carries the business outcome: `ClaimResult::AlreadyClaimed{by}` is
 * an `Ok` variant on the Rust side (`claim_entity(...).map_err(...)` only
 * converts real errors), so the response arrives with `isError: false` and
 * `{"result":{"AlreadyClaimed":{"by":"agent-A"}}}` (or the same object
 * un-nested) in the text. Trusting `isError` alone therefore reported
 * `{ok:true}` for a claim this agent never won — the misreport propagated
 * straight into performWeaveEdit's caller-visible `coordination.claimed`.
 * Returns the holding agent's id when the payload says the claim was lost,
 * undefined otherwise.
 */
export function parseClaimLoser(claimResponseText: string): string | undefined {
  try {
    const parsed = JSON.parse(claimResponseText) as {
      AlreadyClaimed?: { by?: unknown };
      result?: { AlreadyClaimed?: { by?: unknown } };
    };
    const lost = parsed.AlreadyClaimed ?? parsed.result?.AlreadyClaimed;
    if (lost === undefined) return undefined;
    return typeof lost.by === "string" && lost.by.length > 0 ? lost.by : "another agent";
  } catch {
    return undefined;
  }
}

export class Coordinator {
  readonly agentId: string;
  private readonly client: McpClient;
  private started = false;
  private startFailure: Error | undefined;
  private registeredBranch: string | undefined;

  constructor(opts: CoordinatorOptions) {
    this.agentId = opts.agentId;
    this.client = new McpClient({
      id: "weave-edit-coordination",
      command: opts.command,
      args: opts.args ?? [],
      cwd: opts.cwd,
      env: opts.env,
      requestTimeoutMs: opts.requestTimeoutMs ?? 15_000,
      onStderr: opts.onStderr,
    });
  }

  private async ensureRegistered(branch: string): Promise<void> {
    if (this.startFailure) throw this.startFailure;
    if (!this.started) {
      try {
        await this.client.start();
        this.started = true;
      } catch (err) {
        this.startFailure = err instanceof Error ? err : new Error(String(err));
        throw this.startFailure;
      }
    }
    if (this.registeredBranch === branch) return;
    const result = await this.client.callTool("weave_agent_register", { agent_id: this.agentId, branch });
    if (result.isError) throw new Error(extractText(result.content) || "weave_agent_register reported an error");
    this.registeredBranch = branch;
  }

  /**
   * Omits entity_type/parent_name/ordinal entirely when unset, rather than
   * sending them as explicit null. Confirmed against two live weave-mcp
   * builds in this repo that disagree on whether those fields exist on
   * weave_claim_entity/weave_release_entity's schema at all — omitting is
   * compatible with both, where sending an unconditional null broke the
   * older one (`unknown field` under its strict deserializer).
   */
  /**
   * `entityId`, when given, is sent as `entity_id` alongside the usual
   * name-based fields (never in place of them — a still-current entity_name
   * remains useful context, and older weave-mcp servers that ignore
   * entity_id fall through to it unchanged). Only ever pass an `entityId`
   * that came back from THIS session's own `claim()` call against THIS
   * server — see `parseEntityId`'s doc comment for why that's the
   * compatibility guarantee this relies on.
   */
  private queryArgs(filePathRel: string, query: EntityQuery, entityId?: string): Record<string, unknown> {
    const args: Record<string, unknown> = {
      agent_id: this.agentId,
      file_path: filePathRel,
      entity_name: query.name,
    };
    if (query.entity_type !== undefined) args.entity_type = query.entity_type;
    if (query.parent_name !== undefined) args.parent_name = query.parent_name;
    if (query.ordinal !== undefined) args.ordinal = query.ordinal;
    if (entityId !== undefined) args.entity_id = entityId;
    return args;
  }

  async claim(filePathRel: string, branch: string, query: EntityQuery, signal?: AbortSignal): Promise<CoordinationOutcome> {
    try {
      await this.ensureRegistered(branch);
      const result = await this.client.callTool("weave_claim_entity", this.queryArgs(filePathRel, query), signal);
      const text = extractText(result.content);
      if (result.isError) return { ok: false, reason: text || "weave_claim_entity reported an error" };
      // A lost claim rides in as an MCP-level SUCCESS — inspect the payload,
      // never trust isError alone. See parseClaimLoser.
      const holder = parseClaimLoser(text);
      if (holder !== undefined) return { ok: false, reason: `entity already claimed by ${holder}` };
      return { ok: true, text, entityId: parseEntityId(text) };
    } catch (err) {
      return { ok: false, reason: err instanceof Error ? err.message : String(err) };
    }
  }

  /**
   * Writes the entity's new content (skip for a delete, where nothing
   * survives to update) and releases the claim.
   *
   * `renamedQuery`, when given, is the entity's post-edit identity (only
   * relevant for a replace that renamed it via allow_signature_change).
   *
   * Dogfood round 2 finding: the round-1 fix only
   * retried the RELEASE call under the renamed identity when the
   * claim-time name failed — it assumed weave_update_entity_content itself
   * always succeeds under the claim-time name. Confirmed false against a
   * real dogfood run (8/8 identity-changing edits in one batched call):
   * weave-mcp resolves entity_name against the file's OWN CURRENT content,
   * and pi-sem has already written the renamed content to disk (via its
   * own writeFile, independent of weave-mcp) by the time this call goes
   * out — so weave_update_entity_content under the STALE claim-time name
   * can throw "entity '<old name>' not found" too, for the exact same
   * reason release can. That throw used to escape straight past this
   * function's primary/retry release logic entirely (nothing wrapped the
   * update call the way tryRelease wraps release), landing exactly the
   * bare, single-error text the dogfood evidence showed — never the
   * combined "(also failed under the claim-time identity: ...)" message
   * this function produces when release's own primary+retry both actually
   * run. Update now gets the identical defensive treatment as release: try
   * under the claim-time identity, retry under the renamed identity if
   * that fails and one is known, and — critically — never let an update
   * failure (under either identity) skip the release attempt below. A
   * still-failed update is only ever a lost content-sync, never a reason to
   * abandon the claim.
   *
   * `entityId`: the claim's own stable identity, as returned by
   * `claim()`'s `entityId` when the connected weave-mcp supports it. Sent on
   * the FIRST update/release attempt alongside `query` -- a fixed server
   * resolves by entity_id directly and never needs the renamedQuery retry
   * below at all, since a rename can no longer re-key the claim out from
   * under it. Only ever pass an entityId this session's own claim() already
   * captured against this same server (see queryArgs' doc comment); when
   * absent, behavior is byte-for-byte the pre-existing name+renamedQuery
   * retry path.
   */
  /**
   * The concurrent-edit merge backstop: one weave_update_entity_content
   * call carrying the whole-file snapshot pair -- `base_content` (the file
   * as this agent read it) and `ours_content` (the file as this agent
   * intends to write it) -- alongside the entity's new `content`. The
   * server compares base_content against the bytes on disk NOW:
   *
   * - no drift: stores the entity update, responds drift_detected: false.
   * - drift, disjoint entities: merges base->ours with base->theirs and
   *   responds merged: true + merged_content (the reconciled WHOLE FILE,
   *   for THIS caller to write -- weave never writes files) + merged_over
   *   (names of everything it merged across).
   * - drift, same-entity collision: refuses with merge_conflicts in an Ok
   *   payload; nothing is stored anywhere.
   *
   * Contract notes pinned by the live tests: the snapshot is the content
   * PAIR only -- there is no base_hash in this protocol (a cross-language
   * wire hash was rejected server-side as broken by construction), and
   * half a snapshot is an invalid_params protocol error, so this method
   * always sends both or would not be called. Response keys are
   * snake_case: merged, merged_content, merged_over, drift_detected,
   * merge_conflicts.
   *
   * The verdict has a shelf life, and it is the CALLER's job to respect it:
   * "the bytes on disk NOW" means now as of the server's read, which is one
   * round trip before the caller writes. weave-edit.ts brackets this call
   * with its own reads of the same file and re-invokes it (same snapshot
   * pair, unchanged) whenever the disk moved underneath -- see the
   * freshness re-check there. Nothing in this method detects that on its
   * own, and a caller that writes a verdict without re-checking will
   * silently clobber whoever wrote inside the window.
   */
  async mergeCheck(
    filePathRel: string,
    branch: string,
    query: EntityQuery,
    content: string,
    baseContent: string,
    oursContent: string,
    signal?: AbortSignal,
    entityId?: string,
  ): Promise<MergeCheckOutcome> {
    try {
      await this.ensureRegistered(branch);
      const result = await this.client.callTool(
        "weave_update_entity_content",
        { ...this.queryArgs(filePathRel, query, entityId), content, base_content: baseContent, ours_content: oursContent },
        signal,
      );
      const text = extractText(result.content);
      if (result.isError) return { ok: false, conflict: false, reason: text || "weave_update_entity_content reported an error" };
      let payload: {
        drift_detected?: unknown;
        merged?: unknown;
        merged_content?: unknown;
        merged_over?: unknown;
        merge_conflicts?: unknown;
      };
      try {
        payload = JSON.parse(text) as typeof payload;
      } catch {
        // A non-JSON SUCCESS still means the server accepted and stored
        // the update (weave_update_entity_content succeeded; it just isn't
        // a backstop-aware response) -- report "stored, no drift verdict",
        // NOT a transport failure, or the caller would push the same
        // content a second time through the legacy path.
        return { ok: true, drift: undefined, merged: false, text };
      }
      if (Array.isArray(payload.merge_conflicts)) {
        return { ok: false, conflict: true, conflicts: payload.merge_conflicts as MergeConflictSummary[], text };
      }
      if (payload.merged === true) {
        return {
          ok: true,
          drift: payload.drift_detected === true,
          merged: true,
          mergedContent: typeof payload.merged_content === "string" ? payload.merged_content : undefined,
          mergedOver: Array.isArray(payload.merged_over) ? (payload.merged_over as string[]) : [],
          text,
        };
      }
      return {
        ok: true,
        drift: typeof payload.drift_detected === "boolean" ? payload.drift_detected : undefined,
        merged: false,
        text,
      };
    } catch (err) {
      return { ok: false, conflict: false, reason: err instanceof Error ? err.message : String(err) };
    }
  }

  /**
   * A bare CRDT content push, no release -- the resync path for a rollback
   * that happened AFTER the merge gate already stored content the disk
   * never got (weave-edit.ts's resyncEntityText). Same failure isolation
   * as tryUpdate; callers treat a failed resync as advisory.
   */
  async update(
    filePathRel: string,
    branch: string,
    query: EntityQuery,
    content: string,
    signal?: AbortSignal,
    entityId?: string,
  ): Promise<CoordinationOutcome> {
    try {
      await this.ensureRegistered(branch);
      return await this.tryUpdate(filePathRel, query, content, signal, entityId);
    } catch (err) {
      return { ok: false, reason: err instanceof Error ? err.message : String(err) };
    }
  }

  async updateAndRelease(
    filePathRel: string,
    branch: string,
    query: EntityQuery,
    newContent: string | undefined,
    signal?: AbortSignal,
    renamedQuery?: EntityQuery,
    entityId?: string,
  ): Promise<CoordinationOutcome> {
    try {
      await this.ensureRegistered(branch);

      if (newContent !== undefined) {
        const updatePrimary = await this.tryUpdate(filePathRel, query, newContent, signal, entityId);
        if (!updatePrimary.ok && renamedQuery) {
          await this.tryUpdate(filePathRel, renamedQuery, newContent, signal);
        }
        // A still-failed update (under either identity) is deliberately not
        // returned here — release is the part that actually matters (an
        // unreleased claim blocks every other agent); a stale content push
        // is a lesser, silent loss, not a reason to abandon the claim.
      }

      const primary = await this.tryRelease(filePathRel, query, signal, entityId);
      if (primary.ok) return primary;

      if (renamedQuery) {
        const retry = await this.tryRelease(filePathRel, renamedQuery, signal);
        if (retry.ok) return retry;
        return { ok: false, reason: `${retry.reason} (also failed under the claim-time identity: ${primary.reason})` };
      }

      return primary;
    } catch (err) {
      return { ok: false, reason: err instanceof Error ? err.message : String(err) };
    }
  }

  /**
   * An update attempt's own try/catch, isolated the same way tryRelease's
   * is — weave-mcp can report an unresolvable entity_name for
   * weave_update_entity_content exactly as it does for
   * weave_release_entity (a JSON-RPC-level protocol error, which
   * McpClient.request turns into a REJECTED promise, not an {isError:true}
   * result). Without its own try/catch, that throw would escape
   * updateAndRelease's primary/retry logic and its release attempt below
   * entirely.
   */
  private async tryUpdate(
    filePathRel: string,
    query: EntityQuery,
    content: string,
    signal?: AbortSignal,
    entityId?: string,
  ): Promise<CoordinationOutcome> {
    try {
      const result = await this.client.callTool(
        "weave_update_entity_content",
        { ...this.queryArgs(filePathRel, query, entityId), content },
        signal,
      );
      if (result.isError) return { ok: false, reason: extractText(result.content) || "weave_update_entity_content reported an error" };
      return { ok: true, text: extractText(result.content) };
    } catch (err) {
      return { ok: false, reason: err instanceof Error ? err.message : String(err) };
    }
  }

  /**
   * A release attempt's own try/catch, isolated from updateAndRelease's
   * outer one — weave-mcp reports an unresolvable entity_name as a
   * JSON-RPC-level protocol error (confirmed against a real dogfood run:
   * "entity '<name>' not found ... (code -32602)"), which McpClient.request
   * turns into a REJECTED promise, not an {isError: true} result. Without
   * its own try/catch here, that throw would escape past the
   * primary-then-retry logic above entirely and skip the retry.
   */
  private async tryRelease(filePathRel: string, query: EntityQuery, signal?: AbortSignal, entityId?: string): Promise<CoordinationOutcome> {
    try {
      const result = await this.client.callTool("weave_release_entity", this.queryArgs(filePathRel, query, entityId), signal);
      if (result.isError) return { ok: false, reason: extractText(result.content) || "weave_release_entity reported an error" };
      return { ok: true, text: extractText(result.content) };
    } catch (err) {
      return { ok: false, reason: err instanceof Error ? err.message : String(err) };
    }
  }

  async stop(): Promise<void> {
    if (this.started) await this.client.stop();
  }
}
