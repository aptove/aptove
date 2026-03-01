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

function isBinaryInstalled(packageName) {
  try {
    const packagePath = require.resolve(`${packageName}/package.json`);
    const binaryName = process.platform === 'win32' ? 'aptove.exe' : 'aptove';
    const binaryPath = path.join(path.dirname(packagePath), 'bin', binaryName);
    return fs.existsSync(binaryPath);
  } catch (e) {
    return false;
  }
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

  if (isBinaryInstalled(packageName)) {
    console.log(`✓ aptove installed successfully for ${platformKey}`);
  } else {
    console.error(`✗ aptove: binary not found after installing ${packageName}`);
    console.error(`  Try: npm install -g ${packageName}@${version}`);
    process.exit(1);
  }
}

main();
