//! Platform-portable file permission helpers.
//!
//! Each function enforces "owner-only" access on the platforms where
//! restrictive permissions can be set programmatically.

use std::path::Path;

/// Restrict a file to owner-only access.
///
/// Unix: chmod 0o600 (rw-------)
/// Windows: replace the DACL with a single ACE granting the current user
///   full control, and strip inherited ACEs.
pub fn restrict_file_to_owner(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(windows)]
    {
        windows_acl::set_owner_only(path, false)?;
    }
    // Other platforms: no-op
    #[cfg(not(any(unix, windows)))]
    let _ = path;
    Ok(())
}

/// Restrict a directory to owner-only access.
///
/// Unix: chmod 0o700 (rwx------)
/// Windows: replace the DACL with a single ACE granting the current user
///   full control, with inheritable flags so new files inside inherit the
///   restriction. Strips inherited ACEs.
pub fn restrict_dir_to_owner(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o700);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(windows)]
    {
        windows_acl::set_owner_only(path, true)?;
    }
    // Other platforms: no-op
    #[cfg(not(any(unix, windows)))]
    let _ = path;
    Ok(())
}

#[cfg(windows)]
mod windows_acl {
    use std::path::Path;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL};
    use windows::Win32::Security::{
        GetTokenInformation, OpenProcessToken, TokenUser, TOKEN_QUERY, TOKEN_USER,
        DACL_SECURITY_INFORMATION, OBJECT_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, ACL,
    };
    use windows::Win32::Security::Authorization::{
        SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    use windows::core::{PCWSTR, PWSTR};

    // These constants are defined in the Windows SDK but not always re-exported
    // as typed constants in every version of the windows crate, so we keep them
    // as raw u32 values to avoid version-specific import issues.
    const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
    const OBJECT_INHERIT_ACE: u32 = 0x01;
    const CONTAINER_INHERIT_ACE: u32 = 0x02;
    const NO_INHERITANCE: u32 = 0x00;

    /// Apply an owner-only DACL to `path`.
    ///
    /// `inheritable`: when true, the ACE carries `CONTAINER_INHERIT_ACE |
    /// OBJECT_INHERIT_ACE` so that new files/directories created inside a
    /// restricted directory automatically inherit the restriction.
    pub fn set_owner_only(path: &Path, inheritable: bool) -> std::io::Result<()> {
        use std::os::windows::ffi::OsStrExt;

        unsafe {
            // ── 1. Open the current-process token ─────────────────────────
            let process = GetCurrentProcess();
            let mut token = HANDLE::default();
            OpenProcessToken(process, TOKEN_QUERY, &mut token).map_err(win_err)?;

            // ── 2. Query the owner SID ─────────────────────────────────────
            // First call: get the required buffer size (always returns an error).
            let mut needed: u32 = 0;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);

            let mut buf = vec![0u8; needed as usize];
            let query = GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr() as *mut _),
                needed,
                &mut needed,
            );
            let _ = CloseHandle(token); // close regardless of query result
            query.map_err(win_err)?;

            let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
            let owner_sid = token_user.User.Sid; // PSID (*mut c_void)

            // ── 3. Build a one-entry EXPLICIT_ACCESS for the owner ─────────
            let inheritance: u32 = if inheritable {
                CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
            } else {
                NO_INHERITANCE
            };

            let ea = EXPLICIT_ACCESS_W {
                grfAccessPermissions: FILE_ALL_ACCESS,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: inheritance,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: std::ptr::null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_USER,
                    // When TrusteeForm == TRUSTEE_IS_SID, ptstrName holds the PSID
                    // cast to PWSTR — this is the documented Windows API convention.
                    ptstrName: PWSTR(owner_sid.0 as *mut u16),
                },
            };

            // ── 4. Create a new DACL from that single entry ────────────────
            let mut new_dacl: *mut ACL = std::ptr::null_mut();
            SetEntriesInAclW(Some(&[ea]), None, &mut new_dacl).map_err(win_err)?;

            // ── 5. Apply the DACL; PROTECTED_DACL strips inherited ACEs ────
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let result = SetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                OBJECT_SECURITY_INFORMATION(
                    DACL_SECURITY_INFORMATION.0 | PROTECTED_DACL_SECURITY_INFORMATION.0,
                ),
                None,
                None,
                Some(new_dacl),
                None,
            );

            // Always free the DACL allocated by SetEntriesInAclW.
            // LocalFree(NULL) is a no-op, so this is safe even if the pointer is null.
            windows::Win32::Foundation::LocalFree(HLOCAL(new_dacl as *mut _));

            result.map_err(win_err)?;
        }

        Ok(())
    }

    fn win_err(e: windows::core::Error) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    #[cfg(unix)]
    fn restrict_file_sets_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, b"secret").unwrap();
        restrict_file_to_owner(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "file must be owner-read/write only");
    }

    #[test]
    #[cfg(unix)]
    fn restrict_dir_sets_0700_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempdir().unwrap();
        let dir = parent.path().join("secrets");
        std::fs::create_dir(&dir).unwrap();
        restrict_dir_to_owner(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "directory must be owner-only");
    }

    #[test]
    #[cfg(windows)]
    fn restrict_file_no_error_on_windows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, b"secret").unwrap();
        restrict_file_to_owner(&path).unwrap();
        // Current process must still be able to read the file after restricting it
        assert_eq!(std::fs::read(&path).unwrap(), b"secret");
    }

    #[test]
    #[cfg(windows)]
    fn restrict_dir_no_error_on_windows() {
        let parent = tempdir().unwrap();
        let dir = parent.path().join("secrets");
        std::fs::create_dir(&dir).unwrap();
        restrict_dir_to_owner(&dir).unwrap();
        assert!(dir.is_dir());
    }
}
