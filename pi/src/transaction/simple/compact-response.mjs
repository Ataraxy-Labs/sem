// Lossless within-response source deduplication. Never assumes the client has
// retained an earlier response, and never drops truncated/missing context.
export function compactResponse(value) {
  if (!value || !Array.isArray(value.definitions) || !Array.isArray(value.files)) return value;
  const files = new Map(value.files.filter(f => !f.error && !f.truncated && typeof f.content === 'string')
    .map(f => [f.file, f.content]));
  let omitted = 0;
  const definitions = value.definitions.map(d => {
    const source = files.get(d.file);
    if (source === undefined || d.truncated || typeof d.content !== 'string' || !d.content.length) return d;
    // Use exact offsets, not names or approximate identity. Nonunique content is
    // still unambiguous at the returned range within the same response's file.
    const start = source.indexOf(d.content);
    if (start < 0) return d;
    const {content, ...rest} = d;
    omitted += Buffer.byteLength(content);
    return {...rest, content_source: {kind: 'file_in_this_response', file: d.file,
      start_utf16: start, end_utf16: start+content.length}};
  });
  if (!omitted) return value;
  return {...value, definitions, source_encoding: {
    instruction: 'For content_source, take files[file].content.slice(start_utf16,end_utf16); source is included once in this response.',
    duplicate_source_bytes_omitted: omitted}};
}

export function toolResult(value) {
  // No outputSchema is advertised. A text-only MCP result is sufficient and
  // avoids sending a second identical structuredContent representation.
  return {content: [{type: 'text', text: JSON.stringify(compactResponse(value))}]};
}
