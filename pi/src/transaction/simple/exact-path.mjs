import fs from 'node:fs/promises';
import path from 'node:path';

// Root and relative path have already passed scope validation. Never silently
// canonicalize an agent's path: case aliases can break on another filesystem.
export async function requireExactPath(root,file,io=fs) {
  const absolute=path.join(root,file),resolved=await io.realpath(absolute);
  if(resolved===absolute)return absolute;
  const parts=file.split('/');
  for(let i=1;i<=parts.length;i++) {
    if((await io.lstat(path.join(root,...parts.slice(0,i)))).isSymbolicLink())
      throw Error('SYMLINK_NOT_SUPPORTED: '+file);
  }
  const canonical=path.relative(root,resolved);
  if(canonical==='..'||canonical.startsWith('../')||path.isAbsolute(canonical))throw Error('PATH_OUTSIDE_REPOSITORY');
  throw Error('PATH_CANONICAL_MISMATCH: '+JSON.stringify({requested:file,canonical,
    action:'Retry using the exact canonical spelling; this path was not read or edited.'}));
}
