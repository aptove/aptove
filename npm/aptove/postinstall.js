/**
 * aptove postinstall script
 *
 * Ensures the correct platform-specific binary package is installed.
 * npm sometimes skips optional dependencies during global installs, so
 * we detect this and install the platform package explicitly if needed.
 */

const { execSync } = require('child_process');
const path = require('path');
const fs = require('fs');

const PLATFORM_PACKAGES = {
  'darwin-arm64': '@aptove/aptove-darwin-arm64',
  'darwin-x64':   '@aptove/aptove-darwin-x64',
  'linux-arm64':  '@aptove/aptove-linux-arm64',
  'linux-x64':    '@aptove/aptove-linux-x64',
  'win32-x64':    '@aptove/aptove-win32-x64',
};

function getPlatformPackage() {
  const platformKey = `${process.platform}-${process.arch}`;
  return { platformKey, packageName: PLATFORM_PACKAGES[platformKey] };
}

const BINARY_NAME = process.platform === 'win32' ? 'aptove.exe' : 'aptove';

// Check via require.resolve (works before install, may use module cache after).
function isBinaryInstalled(packageName) {
  try {
    const packagePath = require.resolve(`${packageName}/package.json`);
    return fs.existsSync(path.join(path.dirname(packagePath), 'bin', BINARY_NAME));
  } catch (e) {
    return false;
  }
}

// Check directly using the prefix path, bypassing Node.js module resolution cache.
// Used for post-install verification after an explicit `npm install --prefix`.
function isBinaryInstalledAtPrefix(packageName) {
  const prefix = process.env.npm_config_prefix;
  if (!prefix) return false;
  // Scoped packages: @aptove/aptove-darwin-arm64 → lib/node_modules/@aptove/aptove-darwin-arm64
  const packageDir = path.join(prefix, 'lib', 'node_modules', ...packageName.split('/'));
  return fs.existsSync(path.join(packageDir, 'bin', BINARY_NAME));
}

function getPackageVersion() {
  try {
    const pkg = require('./package.json');
    // optionalDependencies values are updated by the release workflow to match the published version
    const deps = pkg.optionalDependencies || {};
    const versions = Object.values(deps).filter(v => v !== '*');
    return versions[0] || pkg.version;
  } catch (e) {
    return 'latest';
  }
}

function installPlatformPackage(packageName, version) {
  // During `npm install -g`, npm_config_prefix points to the global prefix (e.g. /usr/local or ~/.nvm/...).
  // Installing with --prefix ensures the package lands in the same global node_modules tree.
  const prefix = process.env.npm_config_prefix;
  const prefixFlag = prefix ? `--prefix "${prefix}"` : '';

  console.log(`  Installing ${packageName}@${version}...`);
  execSync(
    `npm install ${prefixFlag} --no-save --no-audit --no-fund "${packageName}@${version}"`,
    { stdio: 'inherit' }
  );
}

function main() {
  const { platformKey, packageName } = getPlatformPackage();

  if (!packageName) {
    console.warn(`⚠️  aptove: Unsupported platform ${platformKey}`);
    console.warn('   Supported: darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64');
    return;
  }

  if (isBinaryInstalled(packageName)) {
    console.log(`✓ aptove installed successfully for ${platformKey}`);
    return;
  }

  // Optional dependency was not installed (common with `npm install -g`).
  // Install it explicitly.
  console.log(`⬇  aptove: platform package not found, installing ${packageName}...`);
  const version = getPackageVersion();

  try {
    installPlatformPackage(packageName, version);
  } catch (e) {
    console.error(`✗ aptove: failed to install ${packageName}@${version}`);
    console.error(`  You can install it manually with:`);
    console.error(`    npm install -g ${packageName}@${version}`);
    process.exit(1);
  }

  // After explicit install, verify using the prefix path directly.
  // require.resolve cannot be used here because Node.js caches negative
  // module resolution results within the same process.
  if (isBinaryInstalledAtPrefix(packageName)) {
    console.log(`✓ aptove installed successfully for ${platformKey}`);
  } else {
    console.error(`✗ aptove: binary not found after installing ${packageName}`);
    console.error(`  Try: npm install -g ${packageName}@${version}`);
    process.exit(1);
  }
}

main();
