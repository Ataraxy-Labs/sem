import fs from "node:fs/promises";
import path from "node:path";
import {selectEntity} from "./repair-contract.mjs";
function uniqueFlexibleText(source, requested) {
  const first = source.indexOf(requested);
  if (first >= 0) {
    return source.indexOf(requested, first + 1) < 0
      ? { index: first, text: requested }
      : null;
  }
  const pieces = requested.split(/\s+/).filter(Boolean);
  if (pieces.length < 2) return null;
  const pattern = pieces
    .map((piece) => piece.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
    .join("\\s*");
  const matches = [...source.matchAll(new RegExp(pattern, "g"))];
  if (matches.length !== 1) return null;
  let index = matches[0].index;
  let text = matches[0][0];
  // The tokenized fallback starts at the first non-whitespace token. When
  // the requested anchor includes indentation, consume the actual
  // whitespace-only prefix too; otherwise inserting an indented replacement
  // after the retained prefix doubles the first line's indentation.
  if (/^[ \t]+/.test(requested)) {
    const lineStart = source.lastIndexOf("\n", index - 1) + 1;
    const prefix = source.slice(lineStart, index);
    if (/^[ \t]*$/.test(prefix)) {
      index = lineStart;
      text = prefix + text;
    }
  }
  return { index, text };
}

export async function normalizeEdits(rawEdits, cwd, api) {
  const creates = [];
  const edits = [];
  const textEdits = [];
  const composed = new Map();
  const changes = new Map();
  const outlines = new Map();
  const sources = new Map();
  const outlineFor = async file => {
    if(!outlines.has(file)) outlines.set(file,await api.outline(file));
    return outlines.get(file);
  };
  for (let raw of rawEdits ?? []) {
    if (raw.op === "delete" && typeof raw.old === "string" && typeof raw.new !== "string") {
      raw = { ...raw, new: "" };
    }
    let file = raw.file ?? raw.path ?? raw.entity?.file;
    if (!file) throw new Error("each edit needs file or path");
    const absolutePath=path.resolve(cwd,file);
    if(!absolutePath.startsWith(path.resolve(cwd)+path.sep)) throw new Error('edit path escapes repository');
    file=path.relative(cwd,absolutePath);
    if (raw.operation === "create") {
      creates.push({ file, content: raw.content });
      continue;
    }
    if (raw.entity?.name && typeof raw.old !== "string" && !raw.range) {
      // Canonicalize model-facing aliases/casing against the file outline.
      // Python compatibility aliases such as MCPTool/McpTool otherwise make
      // a correct member edit fail the identity guard before any write.
      let normalizedEntity = raw.entity;
      {
        const outline = await outlineFor(file);
        const canonical = selectEntity(outline.entities ?? [],raw.entity);
        if (canonical) {
          normalizedEntity = {
            ...raw.entity,
            name: canonical.name,
            entity_type: canonical.type,
            ...(canonical.parent_name ? { parent_name: canonical.parent_name } : {}),
          };
        }
      }
      edits.push({ ...raw, entity: normalizedEntity, file, op: raw.op ?? raw.operation ?? "replace", claim: false });
      continue;
    }
    let absolute = path.resolve(cwd, file);
    const root = `${path.resolve(cwd)}${path.sep}`;
    if (!absolute.startsWith(root)) throw new Error(`edit path escapes repository: ${file}`);
    if(!sources.has(file)) sources.set(file,await fs.readFile(absolute,"utf8"));
    const source=sources.get(file);
    // Explicit file targets are never silently redirected to another file.
    const outline = await outlineFor(file);
    let startLine;
    let endLine;
    let replacement;
    let anchorIndex;
    if (raw.range) {
      startLine = raw.range.start_line;
      endLine = raw.range.end_line;
      replacement = raw.content;
    } else if (typeof raw.old === "string" && typeof raw.new === "string") {
      for (const match of raw.new.matchAll(/self\.(\_[A-Za-z_]\w*)\.[A-Za-z_]\w*/g)) {
        const field = match[1].replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
        const guardedInSource = new RegExp(`(?:not\\s+self\\.${field}|self\\.${field}\\s+is\\s+None)`).test(source);
        const guardedInReplacement = new RegExp(`(?:if\\s+self\\.${field}|not\\s+self\\.${field}|hasattr\\s*\\(\\s*self[^)]*["']${field}["']|getattr\\s*\\(\\s*self\\s*,\\s*["']${field}["'])`).test(raw.new);
        if (guardedInSource && !guardedInReplacement) {
          throw new Error(`replacement dereferences nullable self.${match[1]} without preserving its indexed null guard`);
        }
      }
      let resolved;
      if(raw.entity?.name) {
        const selected=selectEntity(outline.entities??[],raw.entity);
        const lines=source.split('\n');
        const body=lines.slice(selected.start_line-1,selected.end_line).join('\n');
        const local=uniqueFlexibleText(body,raw.old);
        const prefix=lines.slice(0,selected.start_line-1).join('\n');
        resolved=local?{...local,index:local.index+prefix.length+(selected.start_line>1?1:0)}:null;
      } else resolved=uniqueFlexibleText(source,raw.old);
      if (!resolved) {
        throw new Error(`old text must occur exactly once in ${file}`);
      }
      raw = { ...raw, old: resolved.text };
      anchorIndex = resolved.index;
      startLine = source.slice(0, resolved.index).split("\n").length;
      endLine = startLine + resolved.text.split("\n").length - 1;
      replacement = raw.new;
    } else {
      throw new Error(`edit for ${file} needs entity, range+content, or exact old+new`);
    }
    const enclosing = (outline.entities ?? [])
      .filter((item) => item.start_line <= startLine && item.end_line >= endLine)
      // Compose textual changes against one outer source image. Independent
      // parent/child replacements from the original image lose each other's
      // changes when applied sequentially (Sphinx process_doc/build_toc).
      .sort((a, b) => (b.end_line - b.start_line) - (a.end_line - a.start_line))[0];
    if (!enclosing) {
      const selected = raw.range
        ? source.split("\n").slice(startLine - 1, endLine).join("\n")
        : raw.old;
      if (!selected || !uniqueFlexibleText(source, selected)) {
        throw new Error(`parser-orphan text must occur exactly once in ${file}:${startLine}-${endLine}`);
      }
      textEdits.push({
        file,
        old: selected,
        new: raw.op === "insert_before"
          ? replacement + selected
          : raw.op === "insert_after"
            ? selected + replacement
            : replacement,
      });
      continue;
    }
    const lines = source.split("\n");
    const entitySource = lines.slice(enclosing.start_line - 1, enclosing.end_line).join("\n");
    const entityKey = [file, enclosing.name, enclosing.type, enclosing.parent_name ?? "",enclosing.start_line,enclosing.end_line].join("\0");
    const prior = composed.get(entityKey);
    let content = prior?.content ?? entitySource;
    if (raw.range) {
      const selected = lines.slice(startLine - 1, endLine).join("\n");
      const first = content.indexOf(selected);
      if (first < 0 || content.indexOf(selected, first + 1) >= 0) {
        throw new Error(`range source must occur exactly once in ${file} entity ${enclosing.name}`);
      }
      content = content.slice(0, first) + replacement + content.slice(first + selected.length);
    } else {
      // Preserve the location resolved in the original entity scope. Searching
      // again inside its parent makes distinct member anchors ambiguous.
      const prefix = lines.slice(0, enclosing.start_line - 1).join('\n');
      const start = anchorIndex - prefix.length - (enclosing.start_line > 1 ? 1 : 0);
      const end = start + raw.old.length;
      const previous = changes.get(entityKey) ?? [];
      if (previous.some(p => start < p.end && p.start < end))
        throw new Error('CONFLICTING_TEXT_EDITS: overlapping original source anchors');
      const shifted = start + previous.filter(p => p.end <= start).reduce((sum,p)=>sum+p.delta,0);
      if (content.slice(shifted, shifted + raw.old.length) !== raw.old)
        throw new Error('CONFLICTING_TEXT_EDITS: original source changed');
      const resolved = {index:shifted,text:raw.old};
      const inserted = raw.op === "insert_before"
        ? replacement + resolved.text
        : raw.op === "insert_after"
          ? resolved.text + replacement
          : replacement;
      content = content.slice(0, resolved.index) + inserted + content.slice(resolved.index + resolved.text.length);
      previous.push({start,end,delta:inserted.length-raw.old.length});
      changes.set(entityKey,previous);
    }
    composed.set(entityKey, {
      file,
      entity: {
        name: enclosing.name,
        entity_type: enclosing.type,
        ...(enclosing.parent_name ? { parent_name: enclosing.parent_name } : {}),
      },
      op: "replace",
      content,
      claim: false,
      allow_signature_change: (raw.allow_signature_change ?? false) || (prior?.allow_signature_change ?? false),
    });
  }
  edits.push(...composed.values());
  // Insertion at a replaced entity's boundary is not a conflicting rewrite.
  // Fuse it with that replacement, preserving the requested insertion order.
  for (let i = edits.length - 1; i >= 0; i--) {
    const insertion = edits[i];
    if (!['insert_before', 'insert_after'].includes(insertion.op)) continue;
    const entity = selectEntity((await outlineFor(insertion.file)).entities ?? [], insertion.entity);
    if (!entity) throw new Error('EDIT_ENTITY_UNRESOLVED');
    const replacements = edits.filter(other => {
      if (other === insertion || other.file !== insertion.file || other.op !== 'replace') return false;
      const target = selectEntity(outlines.get(other.file).entities ?? [], other.entity);
      return target && target.start_line === entity.start_line && target.end_line === entity.end_line;
    });
    if (replacements.length !== 1) continue;
    const replacement = replacements[0];
    if (typeof insertion.content !== 'string' || typeof replacement.content !== 'string')
      throw new Error('EDIT_CONTENT_REQUIRED');
    // Reverse traversal means earlier after-insertions must precede later ones.
    const key = insertion.op === 'insert_before' ? '_before' : '_after';
    replacement[key] = [insertion.content, ...(replacement[key] ?? [])];
    replacement.allow_signature_change ||= insertion.allow_signature_change ?? false;
    edits.splice(i, 1);
  }
  for (const edit of edits) {
    if (edit._before || edit._after) {
      edit.content = [...(edit._before ?? []), edit.content, ...(edit._after ?? [])].join('\n');
      delete edit._before;
      delete edit._after;
    }
  }
  // Explicit full-entity edits cannot safely be combined with replacements of
  // their parent or child. Refuse before any mutation, rather than guess.
  const ranges=[];
  for(const edit of edits) {
    const entity=selectEntity((await outlineFor(edit.file)).entities??[],edit.entity);
    if(!entity) throw new Error('EDIT_ENTITY_UNRESOLVED');
    if(ranges.some(r=>r.file===edit.file&&r.start<=entity.end_line&&entity.start_line<=r.end))
      throw new Error('OVERLAPPING_ENTITY_EDITS: compose changes as old/new edits in one batch');
    ranges.push({file:edit.file,start:entity.start_line,end:entity.end_line});
  }
  return { creates, edits, textEdits };
}
