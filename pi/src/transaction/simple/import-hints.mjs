// A source path is not compiler-resolved import authority. These legacy
// Python module-name hints must never be applied to Rust/TS/other languages.
export function pythonModuleHint(file) {
  if (typeof file!=='string'||!file.endsWith('.py')) return null;
  const parts=file.replaceAll('\\','/').split('/').filter(Boolean);
  const stem=parts.pop().slice(0,-3);
  const hint=stem==='__init__' ? parts.pop() : stem;
  return hint && /^[A-Za-z_]\w*$/.test(hint) ? hint : null;
}
