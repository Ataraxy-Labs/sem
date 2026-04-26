import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptsDirectory = path.dirname(fileURLToPath(import.meta.url));

export const PACKAGE_ROOT = path.resolve(scriptsDirectory, '..');

export function getBinaryName(platform = process.platform) {
  return platform === 'win32' ? 'sem.exe' : 'sem';
}

export function getVendorDirectory(packageRoot = PACKAGE_ROOT) {
  return path.join(packageRoot, 'vendor');
}

export function getInstalledBinaryPath({
  packageRoot = PACKAGE_ROOT,
  platform = process.platform,
} = {}) {
  return path.join(getVendorDirectory(packageRoot), getBinaryName(platform));
}

export function resolveReleaseArtifact({
  platform = process.platform,
  arch = process.arch,
} = {}) {
  const key = `${platform}:${arch}`;

  switch (key) {
    case 'linux:x64':
      return 'sem-linux-x86_64.tar.gz';
    case 'linux:arm64':
      return 'sem-linux-arm64.tar.gz';
    case 'darwin:arm64':
      return 'sem-darwin-arm64.tar.gz';
    case 'win32:x64':
      return 'sem-windows-x86_64.tar.gz';
    default:
      throw new Error(
        `Unsupported platform ${key}. Supported targets: linux/x64, linux/arm64, darwin/arm64, win32/x64.`,
      );
  }
}

export function getReleaseBaseUrl(version, env = process.env) {
  const override = env.SEM_RELEASE_BASE_URL?.trim();
  if (override) {
    return override.replace(/\/+$/, '');
  }

  return `https://github.com/Ataraxy-Labs/sem/releases/download/v${version}`;
}

export function getReleaseDownloadUrl(version, options = {}) {
  const baseUrl = getReleaseBaseUrl(version, options.env);
  const artifact = resolveReleaseArtifact(options);
  return `${baseUrl}/${artifact}`;
}

export async function readPackageVersion(packageRoot = PACKAGE_ROOT) {
  const packageJsonPath = path.join(packageRoot, 'package.json');
  const packageJson = JSON.parse(await fs.readFile(packageJsonPath, 'utf8'));
  return packageJson.version;
}

export async function readCargoPackageVersion(
  manifestPath = path.join(PACKAGE_ROOT, 'crates', 'sem-cli', 'Cargo.toml'),
) {
  const cargoToml = await fs.readFile(manifestPath, 'utf8');
  const versionMatch = cargoToml.match(/^version\s*=\s*"([^"]+)"/m);

  if (!versionMatch) {
    throw new Error(`Could not find version in ${manifestPath}`);
  }

  return versionMatch[1];
}

export async function syncPackageVersion({
  packageRoot = PACKAGE_ROOT,
  version,
} = {}) {
  const resolvedVersion = version ?? (await readCargoPackageVersion());
  const packageJsonPath = path.join(packageRoot, 'package.json');
  const packageJson = JSON.parse(await fs.readFile(packageJsonPath, 'utf8'));
  const changed = packageJson.version !== resolvedVersion;

  if (changed) {
    packageJson.version = resolvedVersion;
    await fs.writeFile(
      packageJsonPath,
      `${JSON.stringify(packageJson, null, 2)}\n`,
      'utf8',
    );
  }

  return {
    changed,
    version: resolvedVersion,
  };
}
