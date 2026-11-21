//! OS machine-id hashing for abuse detection.
//!
//! Returns a SHA-256 hex hash of the OS-level machine identifier.
//! The bridge sends this with every token request so the token service
//! can detect credential sharing across machines.

use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tracing::warn;

/// Returns a SHA-256 hex hash of the OS machine identifier.
///
/// - macOS: `IOPlatformUUID` via `ioreg`
/// - Linux: `/etc/machine-id`
/// - Windows: `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`
///
/// Falls back to a generated UUID stored in `.aptove/device_id` if the
/// OS-level identifier is unavailable.
pub fn get_device_id() -> Option<String> {
    let raw = get_os_machine_id();
    match raw {
        Some(id) => Some(hash_id(&id)),
        None => {
            warn!("OS machine-id unavailable, using generated fallback");
            let fallback = get_or_create_fallback_id();
            fallback.map(|id| hash_id(&id))
        }
    }
}

fn hash_id(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)
}

#[cfg(target_os = "macos")]
fn get_os_machine_id() -> Option<String> {
    let output = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains("IOPlatformUUID") {
            // Extract the UUID value from: "IOPlatformUUID" = "XXXXXXXX-..."
            if let Some(start) = line.rfind('"') {
                let before = &line[..start];
                if let Some(uuid_start) = before.rfind('"') {
                    let uuid = &before[uuid_start + 1..];
                    if !uuid.is_empty() {
                        return Some(uuid.to_string());
                    }
                }
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn get_os_machine_id() -> Option<String> {
    std::fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(target_os = "windows")]
fn get_os_machine_id() -> Option<String> {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains("MachineGuid") {
            // Format: "    MachineGuid    REG_SZ    <guid>"
            if let Some(guid) = line.split_whitespace().last() {
                return Some(guid.to_string());
            }
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn get_os_machine_id() -> Option<String> {
    None
}

/// Generate a UUID and store it in the config directory as a fallback.
fn get_or_create_fallback_id() -> Option<String> {
    let config_dir = crate::common_config::CommonConfig::config_dir();
    let path: PathBuf = config_dir.join("device_id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = std::fs::write(&path, &id) {
        warn!("Failed to write fallback device_id to {:?}: {}", path, e);
    }
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_id_produces_hex_string() {
        let hash = hash_id("test-machine-id");
        assert_eq!(hash.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn get_device_id_returns_some() {
        // Should always return Some — either OS ID or fallback
        let id = get_device_id();
        assert!(id.is_some());
        assert_eq!(id.unwrap().len(), 64);
    }
}
