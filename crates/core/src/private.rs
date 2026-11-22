//! Files and directories only the user can read (CONCEPT 3.4): key names
//! often carry emails, customer ids and tenant names, and an SSH host key
//! file another local user can write to is no check at all.
//!
//! On Unix that is mode 0700 for directories and 0600 for files, set when
//! the entry is created, so there is no moment with the umask's wider
//! mode. Elsewhere the entries are created plainly: a Windows profile
//! directory is already private to its user.

use std::io;
use std::path::Path;

/// Create `dir` and its missing parents, each user-only. A directory that
/// exists is left as it is: it may be the user's own (`--index ./x.sqlite`).
pub fn create_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// [`create_dir`], and an existing `dir` is tightened too. For directories
/// that are the application's own.
pub fn own_dir(dir: &Path) -> io::Result<()> {
    create_dir(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Make sure `path` exists as a user-only file: created empty with mode
/// 0600 when missing, tightened when it exists.
pub fn create_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => restrict_existing(path),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Tighten a file to 0600 if it exists; a missing file is fine.
pub fn restrict_existing(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(m) if m.permissions().mode() & 0o777 == 0o600 => Ok(()),
            Ok(_) => std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn new_and_existing_entries_end_up_user_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("b");
        create_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);

        let loose = tmp.path().join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_dir(&loose).unwrap();
        assert_eq!(mode(&loose), 0o755);
        own_dir(&loose).unwrap();
        assert_eq!(mode(&loose), 0o700);

        let file = dir.join("f");
        create_file(&file).unwrap();
        assert_eq!(mode(&file), 0o600);

        std::fs::write(&file, b"kept").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        create_file(&file).unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(std::fs::read(&file).unwrap(), b"kept");

        restrict_existing(&dir.join("missing")).unwrap();
    }
}
