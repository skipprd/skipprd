//! Durability I/O for local WAL files.
//!
//! Linux uses `O_DIRECT` (aligned bounce buffer). macOS uses `F_NOCACHE`.
//! Windows keeps ordinary buffered `std::fs::File`. `fsync`/`fdatasync` failure
//! poisons the handle; callers must drop it and must not retry on the same fd.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
#[cfg(not(windows))]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
use std::os::unix::fs::FileExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(target_os = "linux")]
const ALIGN: usize = 4096;
#[cfg(not(windows))]
static WARNED_FALLBACK: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "linux")]
struct AlignedBuf {
    ptr: std::ptr::NonNull<u8>,
    layout: std::alloc::Layout,
}

#[cfg(target_os = "linux")]
impl AlignedBuf {
    fn new() -> io::Result<Self> {
        let layout = std::alloc::Layout::from_size_align(ALIGN, ALIGN)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = std::ptr::NonNull::new(raw).ok_or_else(|| {
            io::Error::new(io::ErrorKind::OutOfMemory, "aligned WAL bounce buffer")
        })?;
        Ok(Self { ptr, layout })
    }

    fn as_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), ALIGN) }
    }
}

#[cfg(target_os = "linux")]
impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

enum Inner {
    #[cfg(target_os = "linux")]
    Direct {
        file: File,
        bounce: AlignedBuf,
        pos: u64,
        logical_len: u64,
    },
    Buffered {
        file: File,
    },
}

pub struct DirectIoFile {
    inner: Inner,
    poisoned: bool,
}

impl DirectIoFile {
    pub fn create(path: &Path) -> io::Result<Self> {
        Self::open_with(path, OpenSpec::create())
    }

    pub fn create_new(path: &Path) -> io::Result<Self> {
        Self::open_with(path, OpenSpec::create_new())
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with(path, OpenSpec::open())
    }

    pub fn open_rw(path: &Path) -> io::Result<Self> {
        Self::open_with(path, OpenSpec::open_rw())
    }

    pub fn open_append(path: &Path) -> io::Result<Self> {
        let mut file = Self::open_with(path, OpenSpec::open_rw_or_create())?;
        let len = file.logical_len()?;
        file.seek(SeekFrom::Start(len))?;
        Ok(file)
    }

    pub fn write_path_sync(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut file = Self::create(path)?;
        file.write_all(bytes)?;
        file.sync_data()
    }

    pub fn read_path(path: &Path) -> io::Result<Vec<u8>> {
        let mut file = Self::open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn open_with(path: &Path, spec: OpenSpec) -> io::Result<Self> {
        #[cfg(windows)]
        {
            return Ok(Self {
                inner: Inner::Buffered {
                    file: spec.open_buffered(path)?,
                },
                poisoned: false,
            });
        }
        #[cfg(not(windows))]
        {
            match try_open_unbuffered(path, spec) {
                Ok(file) => Self::from_unbuffered(file, spec.truncate),
                Err(err) if is_direct_unsupported(&err) => {
                    if clustered_wal_requires_direct_io() {
                        return Err(io::Error::new(
                            err.kind(),
                            format!("WAL Direct I/O is required on clustered volumes: {err}"),
                        ));
                    }
                    if !WARNED_FALLBACK.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            path = %path.display(),
                            "WAL Direct I/O is unavailable; falling back to buffered I/O"
                        );
                    }
                    Ok(Self {
                        inner: Inner::Buffered {
                            file: spec.open_buffered(path)?,
                        },
                        poisoned: false,
                    })
                }
                Err(err) => Err(err),
            }
        }
    }

    #[cfg(not(windows))]
    fn from_unbuffered(file: File, truncated: bool) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let logical_len = if truncated { 0 } else { file.metadata()?.len() };
            Ok(Self {
                inner: Inner::Direct {
                    file,
                    bounce: AlignedBuf::new()?,
                    pos: 0,
                    logical_len,
                },
                poisoned: false,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = truncated;
            Ok(Self {
                inner: Inner::Buffered { file },
                poisoned: false,
            })
        }
    }

    fn ensure_live(&self) -> io::Result<()> {
        if self.poisoned {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "WAL file handle poisoned after fsync failure",
            ))
        } else {
            Ok(())
        }
    }

    fn logical_len(&self) -> io::Result<u64> {
        match &self.inner {
            #[cfg(target_os = "linux")]
            Inner::Direct { logical_len, .. } => Ok(*logical_len),
            Inner::Buffered { file } => Ok(file.metadata()?.len()),
        }
    }

    pub fn sync_data(&mut self) -> io::Result<()> {
        self.sync_kind(false)
    }

    pub fn sync_all(&mut self) -> io::Result<()> {
        self.sync_kind(true)
    }

    fn sync_kind(&mut self, metadata: bool) -> io::Result<()> {
        self.ensure_live()?;
        self.flush_logical_len()?;
        let result = match &mut self.inner {
            #[cfg(target_os = "linux")]
            Inner::Direct { file, .. } => {
                if metadata {
                    file.sync_all()
                } else {
                    file.sync_data()
                }
            }
            Inner::Buffered { file } => {
                if metadata {
                    file.sync_all()
                } else {
                    file.sync_data()
                }
            }
        };
        if let Err(err) = result {
            self.poisoned = true;
            return Err(err);
        }
        Ok(())
    }

    fn flush_logical_len(&mut self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if let Inner::Direct {
            file, logical_len, ..
        } = &self.inner
        {
            file.set_len(*logical_len)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn poison_for_test(&mut self) {
        self.poisoned = true;
    }
}

#[derive(Clone, Copy)]
struct OpenSpec {
    read: bool,
    write: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
}

impl OpenSpec {
    fn create() -> Self {
        Self {
            read: true,
            write: true,
            create: true,
            create_new: false,
            truncate: true,
        }
    }

    fn create_new() -> Self {
        Self {
            read: true,
            write: true,
            create: true,
            create_new: true,
            truncate: false,
        }
    }

    fn open() -> Self {
        Self {
            read: true,
            write: false,
            create: false,
            create_new: false,
            truncate: false,
        }
    }

    fn open_rw() -> Self {
        Self {
            read: true,
            write: true,
            create: false,
            create_new: false,
            truncate: false,
        }
    }

    fn open_rw_or_create() -> Self {
        Self {
            read: true,
            write: true,
            create: true,
            create_new: false,
            truncate: false,
        }
    }

    fn apply(self, options: &mut OpenOptions) {
        options
            .read(self.read)
            .write(self.write)
            .create(self.create)
            .create_new(self.create_new)
            .truncate(self.truncate);
    }

    fn open_buffered(self, path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        self.apply(&mut options);
        options.open(path)
    }
}

#[cfg(unix)]
fn try_open_unbuffered(path: &Path, spec: OpenSpec) -> io::Result<File> {
    let mut options = OpenOptions::new();
    spec.apply(&mut options);
    #[cfg(target_os = "linux")]
    {
        options.custom_flags(libc::O_DIRECT);
        options.read(true);
        options.write(true);
    }
    let file = options.open(path)?;
    #[cfg(target_os = "macos")]
    {
        let rc = unsafe {
            libc::fcntl(
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                libc::F_NOCACHE,
                1,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(file)
}

#[cfg(unix)]
fn is_direct_unsupported(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
    )
}

#[cfg(not(windows))]
fn clustered_wal_requires_direct_io() -> bool {
    crate::helpers::configuration::Config::wal_storage_raw()
        .parse::<crate::helpers::wal_storage::WalStorage>()
        .ok()
        == Some(crate::helpers::wal_storage::WalStorage::Clustered)
}

impl Read for DirectIoFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.ensure_live()?;
        match &mut self.inner {
            Inner::Buffered { file } => file.read(buf),
            #[cfg(target_os = "linux")]
            Inner::Direct {
                file,
                bounce,
                pos,
                logical_len,
            } => read_direct(file, bounce, pos, *logical_len, buf),
        }
    }
}

impl Write for DirectIoFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.ensure_live()?;
        if buf.is_empty() {
            return Ok(0);
        }
        match &mut self.inner {
            Inner::Buffered { file } => file.write(buf),
            #[cfg(target_os = "linux")]
            Inner::Direct {
                file,
                bounce,
                pos,
                logical_len,
            } => write_direct(file, bounce, pos, logical_len, buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ensure_live()?;
        match &mut self.inner {
            Inner::Buffered { file } => file.flush(),
            #[cfg(target_os = "linux")]
            Inner::Direct { .. } => Ok(()),
        }
    }
}

impl Seek for DirectIoFile {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64> {
        self.ensure_live()?;
        match &mut self.inner {
            Inner::Buffered { file } => file.seek(style),
            #[cfg(target_os = "linux")]
            Inner::Direct {
                pos, logical_len, ..
            } => {
                let next = match style {
                    SeekFrom::Start(off) => off as i128,
                    SeekFrom::Current(delta) => *pos as i128 + delta as i128,
                    SeekFrom::End(delta) => *logical_len as i128 + delta as i128,
                };
                if next < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "WAL seek before start of file",
                    ));
                }
                *pos = next as u64;
                Ok(*pos)
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn read_direct(
    file: &File,
    bounce: &mut AlignedBuf,
    pos: &mut u64,
    logical_len: u64,
    buf: &mut [u8],
) -> io::Result<usize> {
    if *pos >= logical_len || buf.is_empty() {
        return Ok(0);
    }
    let page = *pos / ALIGN as u64 * ALIGN as u64;
    load_page(file, bounce, page, logical_len)?;
    let page_off = (*pos - page) as usize;
    let available = (ALIGN - page_off).min((logical_len - *pos) as usize);
    let n = available.min(buf.len());
    buf[..n].copy_from_slice(&bounce.as_mut()[page_off..page_off + n]);
    *pos += n as u64;
    Ok(n)
}

#[cfg(target_os = "linux")]
fn write_direct(
    file: &File,
    bounce: &mut AlignedBuf,
    pos: &mut u64,
    logical_len: &mut u64,
    buf: &[u8],
) -> io::Result<usize> {
    let page = *pos / ALIGN as u64 * ALIGN as u64;
    load_page(file, bounce, page, *logical_len)?;
    let page_off = (*pos - page) as usize;
    let n = (ALIGN - page_off).min(buf.len());
    bounce.as_mut()[page_off..page_off + n].copy_from_slice(&buf[..n]);
    file.write_at(bounce.as_mut(), page)?;
    *pos += n as u64;
    if *pos > *logical_len {
        *logical_len = *pos;
    }
    Ok(n)
}

#[cfg(target_os = "linux")]
fn load_page(file: &File, bounce: &mut AlignedBuf, page: u64, logical_len: u64) -> io::Result<()> {
    let buf = bounce.as_mut();
    buf.fill(0);
    if page >= logical_len {
        return Ok(());
    }
    match file.read_at(buf, page) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-dio");
        (dir, path)
    }

    #[test]
    fn unaligned_append_round_trips_after_fdatasync() {
        let (_dir, path) = temp_path();
        let mut payload = Vec::new();
        payload.extend_from_slice(&(7u32).to_le_bytes());
        payload.extend_from_slice(b"payload");
        payload.extend_from_slice(&[0x11; 32]);
        {
            let mut file = DirectIoFile::open_append(&path).unwrap();
            file.write_all(&payload).unwrap();
            file.write_all(b"tail").unwrap();
            file.sync_data().unwrap();
        }
        let mut file = DirectIoFile::open(&path).unwrap();
        let mut got = Vec::new();
        file.read_to_end(&mut got).unwrap();
        let mut expected = payload;
        expected.extend_from_slice(b"tail");
        assert_eq!(got, expected);
    }

    #[test]
    fn fsync_failure_is_not_retried_on_the_same_handle() {
        let (_dir, path) = temp_path();
        let mut file = DirectIoFile::create(&path).unwrap();
        file.write_all(b"x").unwrap();
        file.poison_for_test();
        let err = file.sync_data().unwrap_err();
        assert!(err.to_string().contains("poisoned"));
        assert!(file.write_all(b"y").is_err());
        drop(file);
        let mut file = DirectIoFile::create(&path).unwrap();
        file.write_all(b"z").unwrap();
        file.sync_data().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn paper_direct_read_misses_dirty_page_cache() {
        let (_dir, path) = temp_path();
        let data = vec![0xABu8; ALIGN];
        {
            let mut buffered = File::create(&path).unwrap();
            buffered.write_all(&data).unwrap();
        }
        let mut before = vec![0u8; ALIGN];
        {
            let mut direct = DirectIoFile::open(&path).unwrap();
            let _ = direct.read(&mut before);
        }
        {
            let buffered = File::options().write(true).open(&path).unwrap();
            buffered.sync_all().unwrap();
        }
        let mut after = vec![0u8; ALIGN];
        {
            let mut direct = DirectIoFile::open(&path).unwrap();
            direct.read_exact(&mut after).unwrap();
        }
        assert_eq!(after, data);
    }
}
