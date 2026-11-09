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

/// Replace a secret file atomically: write a 0600 temp file in the same
/// directory, flush it to disk, then rename it over `path`. A crash leaves
/// either the old file or the new one, never a torn or missing one. Use it
/// for secrets that rotate, where losing the new value is unrecoverable
/// (Oura refresh tokens work only once).
pub fn replace_secret_file(path: &std::path::Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| format!("{}: not a file path", path.display()))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let write = || -> std::io::Result<()> {
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            // The mode above only applies when the file is created.
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(dir)?.sync_all()
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("write {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_is_0600_and_readable_back() {
        let dir = std::env::temp_dir().join(format!("vitals-secrets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("tok.json");
        replace_secret_file(&p, "one").unwrap();
        replace_secret_file(&p, "two").unwrap();
        assert_eq!(read_secret_file(p.to_str().unwrap()).unwrap(), "two");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(left.len(), 1, "temp file left behind");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
