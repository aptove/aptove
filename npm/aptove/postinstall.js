/**
 * aptove postinstall script
 *
 * Verifies the correct platform-specific package was installed
 * and the binary is executable.
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

function main() {
  const platformKey = `${process.platform}-${process.arch}`;
  const packageName = PLATFORM_PACKAGES[platformKey];

  if (!packageName) {
    console.warn(`⚠️  aptove: Unsupported platform ${platformKey}`);
    console.warn('   Supported platforms: darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64');
    return;
  }

  try {
    const packagePath = require.resolve(`${packageName}/package.json`);
    const binaryName = process.platform === 'win32' ? 'aptove.exe' : 'aptove';
    const binaryPath = path.join(path.dirname(packagePath), 'bin', binaryName);

    if (!fs.existsSync(binaryPath)) {
      console.warn(`⚠️  aptove: Binary not found at ${binaryPath}`);
      return;
    }

    if (process.platform !== 'win32') {
      try {
        fs.chmodSync(binaryPath, 0o755);
      } catch (e) {
        // Not critical
      }
    }

    try {
      execSync(`"${binaryPath}" --version`, { stdio: 'pipe' });
      console.log(`✓ aptove installed successfully for ${platformKey}`);
    } catch (e) {
      console.warn(`⚠️  aptove: Binary exists but failed to execute on ${platformKey}`);
    }
  } catch (e) {
    console.warn(`⚠️  aptove: Platform package ${packageName} not installed`);
    console.warn('   This is expected on CI or unsupported platforms');
  }
}

main();
