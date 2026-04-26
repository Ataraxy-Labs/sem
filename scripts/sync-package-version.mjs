import { syncPackageVersion } from './package-meta.mjs';

const requestedVersion = process.argv[2]?.trim();
const result = await syncPackageVersion({
  version: requestedVersion || undefined,
});

if (result.changed) {
  console.log(`Updated package.json version to ${result.version}.`);
} else {
  console.log(`package.json already matches version ${result.version}.`);
}
