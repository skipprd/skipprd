//! Local volume pressure for replica placement. No operator knobs: the free-byte
//! floor is [`skippr_lease::DISK_PRESSURE_FREE_BYTES`].

use std::path::Path;

use skippr_lease::DISK_PRESSURE_FREE_BYTES;

pub fn available_bytes(path: &Path) -> Option<u64> {
    available_bytes_at(path)
}

pub fn disk_under_pressure(path: &Path) -> bool {
    match available_bytes(path) {
        Some(free) => free < DISK_PRESSURE_FREE_BYTES,
        None => true,
    }
}

#[cfg(unix)]
fn available_bytes_at(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let probe = if path.exists() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let c_path = CString::new(probe.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stats.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    let block_size = u128::from(if stats.f_frsize > 0 {
        stats.f_frsize
    } else {
        stats.f_bsize
    });
    u64::try_from(u128::from(stats.f_bavail).checked_mul(block_size)?).ok()
}

#[cfg(not(unix))]
fn available_bytes_at(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::is_enospc;

    #[test]
    fn tempfile_reports_available_bytes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(available_bytes(dir.path()).is_some());
    }

    #[test]
    fn enospc_detects_storage_full_and_errno() {
        let full = std::io::Error::from(std::io::ErrorKind::StorageFull);
        assert!(is_enospc(&full));
        let posix = std::io::Error::from_raw_os_error(28);
        assert!(is_enospc(&posix));
        let other = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(!is_enospc(&other));
    }

    #[cfg(unix)]
    #[test]
    fn unmeasurable_volume_is_under_pressure() {
        use std::os::unix::ffi::OsStrExt;
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(&[0]));
        assert!(available_bytes(path).is_none());
        assert!(disk_under_pressure(path));
    }
}
