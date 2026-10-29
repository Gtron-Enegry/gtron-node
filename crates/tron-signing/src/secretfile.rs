//! Checks for files that hold secrets (keys, tokens, `.env`).

use std::path::Path;

/// The file must be a regular file (not a symlink, FIFO or device), owned by the user this
/// process runs as, and not readable or writable by group or others. A symlink could point
/// a secret path at a file someone else controls; a file owned by another user could be
/// swapped for one with a key they chose.
pub fn check_private_file(path: impl AsRef<Path>) -> Result<(), String> {
    let path = path.as_ref();
    let shown = path.display();
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{shown}: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{shown} is a symlink; point at the file itself"));
    }
    if !meta.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "{shown} is accessible by group/others (mode {:o}); chmod 400 it",
                mode & 0o777
            ));
        }
        let euid = effective_uid();
        if meta.uid() != euid {
            return Err(format!(
                "{shown} is owned by uid {}, not by this process (uid {euid})",
                meta.uid()
            ));
        }
    }
    Ok(())
}

/// The user this process runs as.
#[cfg(unix)]
pub fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn modes_symlinks_and_kinds() {
        let dir = std::env::temp_dir().join(format!("gtron-secretfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("key");
        std::fs::write(&f, "x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_private_file(&f).is_err());
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(check_private_file(&f).is_ok());
        let link = dir.join("link");
        std::os::unix::fs::symlink(&f, &link).unwrap();
        assert!(check_private_file(&link).is_err());
        assert!(check_private_file(&dir).is_err());
        assert!(check_private_file(dir.join("missing")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
