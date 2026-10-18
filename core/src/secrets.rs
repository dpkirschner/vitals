//! Secret-file helpers. Secrets never touch the DB or argv (invariant): they
//! resolve from env or a 0600 file only. This is the headless-server posture —
//! launchd has no unlocked GUI keychain, so there is no keychain path.

/// Read a secret from a file, refusing it if group/other can read it.
pub fn read_secret_file(path: &str) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("stat {path}: {e}"))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "{path} is group/other-accessible (mode {:o}); run `chmod 600 {path}`",
                mode & 0o777
            ));
        }
    }
    let contents = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let secret = contents.trim();
    if secret.is_empty() {
        return Err(format!("{path} is empty"));
    }
    Ok(secret.to_string())
}

/// Write a secret to a file with 0600 permissions (owner-only).
pub fn write_secret_file(path: &str, secret: &str) -> Result<(), String> {
    std::fs::write(path, format!("{secret}\n")).map_err(|e| format!("write {path}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {path}: {e}"))?;
    }
    Ok(())
}
