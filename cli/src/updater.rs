use self_update::backends::github::Update;
use semver::Version;

const REPO_OWNER: &str = "aptove";
const REPO_NAME: &str = "aptove";
const BIN_NAME: &str = "aptove";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Check if a newer release exists on GitHub. Returns the new version string
/// (without leading "v") if one is available, or None if already up-to-date
/// or if the network is unreachable. Intended to run via spawn_blocking.
pub fn check_for_update() -> Option<String> {
    let releases = Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .current_version(CURRENT_VERSION)
        .no_confirm(true)
        .show_output(false)
        .show_download_progress(false)
        .build()
        .ok()?
        .get_latest_release()
        .ok()?;

    let release = releases.latest()?;
    let version_str = release.version().trim_start_matches('v');
    let latest = Version::parse(version_str).ok()?;
    let current = Version::parse(CURRENT_VERSION).ok()?;
    if latest > current {
        Some(version_str.to_string())
    } else {
        None
    }
}

/// Download and replace the binary with the latest GitHub release.
/// Returns the new version string on success, or an error message.
/// Intended to run via spawn_blocking.
pub fn perform_update() -> Result<String, String> {
    let status = Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .current_version(CURRENT_VERSION)
        .no_confirm(true)
        .show_output(false)
        .show_download_progress(false)
        .build()
        .map_err(|e| e.to_string())?
        .update()
        .map_err(|e| e.to_string())?;
    Ok(status.version().to_string())
}
