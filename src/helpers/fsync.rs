use std::fs::File;
use std::io;
use std::path::Path;

/// Flush a directory after rename so the new name is durable.
///
/// Windows cannot open a directory as a file (`ERROR_ACCESS_DENIED`). Callers
/// must already have fsynced the file bytes before rename; this is then a no-op.
pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn fsync_dir_succeeds_for_an_ordinary_directory() {
        let dir = tempdir().unwrap();
        fsync_dir(dir.path()).unwrap();
    }
}
