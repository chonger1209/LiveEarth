//! One in-memory directory inventory, updated from Windows filename notifications.
//! Image decoding remains in reuse checks; indexing and deletion read metadata only.
use crate::domain::Stop;
use anyhow::Result;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_IO_INCOMPLETE, HANDLE},
        Storage::FileSystem::*,
        System::{IO::*, Threading::CreateEventW},
    },
    core::PCWSTR,
};

#[repr(align(4))]
struct Buffer([u8; 65536]);

struct Watch {
    directory: HANDLE,
    overlap: Box<OVERLAPPED>,
    buffer: Box<Buffer>,
    pending: bool,
}
// The pending request points only to heap allocations. Access is serialized by INDEX.
unsafe impl Send for Watch {}
impl Watch {
    fn new(path: &Path) -> Result<Self> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let directory = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )?
        };
        let event = match unsafe { CreateEventW(None, true, false, None) } {
            Ok(event) => event,
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(directory);
                }
                return Err(e.into());
            }
        };
        let mut watch = Self {
            directory,
            overlap: Box::new(OVERLAPPED {
                hEvent: event,
                ..Default::default()
            }),
            buffer: Box::new(Buffer([0; 65536])),
            pending: false,
        };
        watch.arm()?;
        Ok(watch)
    }
    fn arm(&mut self) -> Result<()> {
        *self.overlap = OVERLAPPED {
            hEvent: self.overlap.hEvent,
            ..Default::default()
        };
        unsafe {
            ReadDirectoryChangesW(
                self.directory,
                self.buffer.0.as_mut_ptr().cast(),
                self.buffer.0.len() as u32,
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_SIZE,
                None,
                Some(&mut *self.overlap),
                None,
            )?;
        }
        self.pending = true;
        Ok(())
    }
    // None means the OS buffer overflowed: the caller must rebuild the inventory.
    fn changes(&mut self) -> Result<Option<Vec<PathBuf>>> {
        let mut bytes = 0;
        match unsafe { GetOverlappedResult(self.directory, &*self.overlap, &mut bytes, false) } {
            Err(e) if e.code() == ERROR_IO_INCOMPLETE.to_hresult() => return Ok(Some(Vec::new())),
            result => result?,
        }
        self.pending = false;
        let mut names = Vec::new();
        let mut offset = 0;
        let mut valid = bytes > 0;
        while valid && offset + 12 <= bytes as usize {
            let number =
                |at| u32::from_le_bytes(self.buffer.0[at..at + 4].try_into().unwrap()) as usize;
            let next = number(offset);
            let length = number(offset + 8);
            if !length.is_multiple_of(2) || offset + 12 + length > bytes as usize {
                valid = false;
                break;
            }
            let chars: Vec<u16> = self.buffer.0[offset + 12..offset + 12 + length]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            names.push(PathBuf::from(std::ffi::OsString::from_wide(&chars)));
            if next == 0 {
                break;
            }
            if next < 12 || offset + next >= bytes as usize {
                valid = false;
                break;
            }
            offset += next;
        }
        self.arm()?;
        Ok(valid.then_some(names))
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        unsafe {
            if self.pending {
                let _ = CancelIoEx(self.directory, Some(&*self.overlap));
                let mut bytes = 0;
                let _ = GetOverlappedResult(self.directory, &*self.overlap, &mut bytes, true);
            }
            let _ = CloseHandle(self.overlap.hEvent);
            let _ = CloseHandle(self.directory);
        }
    }
}

#[derive(Default)]
struct Index {
    directory: PathBuf,
    watch: Option<Watch>,
    files: BTreeSet<PathBuf>,
    initialized: bool,
    degraded: bool,
    mode_change: Option<bool>,
    #[cfg(test)]
    scans: usize,
}
static INDEX: OnceLock<Mutex<Index>> = OnceLock::new();
fn archive_name(path: &Path) -> bool {
    let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
        return false;
    };
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("png" | "webp")
    ) && name.contains(&format!("_{}_", crate::domain::PRODUCT))
        && name
            .get(..16)
            .is_some_and(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ").is_ok())
}
impl Index {
    fn scan(&mut self, stop: &Stop) -> Result<()> {
        stop.check()?;
        #[cfg(test)]
        {
            self.scans += 1;
        }
        let mut files = BTreeSet::new();
        for entry in fs::read_dir(&self.directory)? {
            stop.check()?;
            let entry = entry?;
            if archive_name(&entry.path())
                && entry.file_type()?.is_file()
                && crate::storage::is_archive(&entry.path())
            {
                files.insert(entry.path());
            }
        }
        stop.check()?;
        self.files = files;
        Ok(())
    }
    fn rebuild(&mut self, directory: &Path, stop: &Stop, watch: Result<Watch>) -> Result<()> {
        self.initialized = false;
        self.directory = directory.to_owned();
        self.watch = watch.ok();
        self.scan(stop)?;
        let degraded = self.watch.is_none();
        if degraded != self.degraded {
            self.mode_change = Some(degraded);
        }
        self.degraded = degraded;
        self.initialized = true;
        Ok(())
    }
    fn begin_batch_with(
        &mut self,
        directory: &Path,
        stop: &Stop,
        make_watch: impl FnOnce(&Path) -> Result<Watch>,
    ) -> Result<()> {
        stop.check()?;
        if self.directory != directory || !self.initialized || self.degraded {
            // Reconnect only once at the next batch boundary, never per lookup.
            self.watch = None;
            self.rebuild(directory, stop, make_watch(directory))
        } else {
            self.sync(directory, stop)
        }
    }
    fn sync(&mut self, directory: &Path, stop: &Stop) -> Result<()> {
        stop.check()?;
        if self.directory != directory || !self.initialized {
            self.watch = None;
            return self.rebuild(directory, stop, Watch::new(directory));
        }
        let Some(watch) = &mut self.watch else {
            // Degraded mode reuses this batch's inventory. Our saves/deletes call record().
            return Ok(());
        };
        match watch.changes() {
            Ok(Some(names)) => {
                for name in names.into_iter().collect::<BTreeSet<_>>() {
                    stop.check()?;
                    let path = directory.join(name);
                    if !archive_name(&path) {
                        continue;
                    }
                    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file())
                        && crate::storage::is_archive(&path)
                    {
                        self.files.insert(path);
                    } else {
                        self.files.remove(&path);
                    }
                }
            }
            Ok(None) => self.scan(stop)?,
            Err(_) => {
                self.watch = None;
                self.rebuild(directory, stop, Watch::new(directory))?;
            }
        }
        Ok(())
    }
    fn invalidate(&mut self) {
        self.initialized = false;
        self.watch = None;
    }
}
fn index() -> std::sync::MutexGuard<'static, Index> {
    INDEX
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}
pub fn begin_batch(directory: &Path, stop: &Stop) -> Result<Option<bool>> {
    let mut index = index();
    if let Err(error) = index.begin_batch_with(directory, stop, Watch::new) {
        index.invalidate();
        return Err(error);
    }
    Ok(index.mode_change.take())
}
pub fn take_mode_change() -> Option<bool> {
    index().mode_change.take()
}
pub fn files(directory: &Path, stop: &Stop) -> Result<Vec<PathBuf>> {
    let mut index = index();
    if let Err(error) = index.sync(directory, stop) {
        index.invalidate();
        return Err(error);
    }
    Ok(index.files.iter().cloned().collect())
}
pub fn record(path: &Path, exists: bool) {
    let mut index = INDEX
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if path.parent() == Some(index.directory.as_path()) {
        if exists {
            index.files.insert(path.to_owned());
        } else {
            index.files.remove(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::Observation,
        imaging::{Colors, Frame},
        storage,
    };
    use chrono::{TimeZone, Utc};
    use std::time::{Duration, Instant};

    fn photo(directory: &Path) -> PathBuf {
        let frame = Frame {
            observation: Observation::new(Utc.with_ymd_and_hms(2026, 10, 22, 6, 0, 0).unwrap(), 1)
                .unwrap(),
            pixels: image::RgbImage::new(550, 550),
            colors: Colors::default(),
        };
        storage::save(directory, &frame, &Stop::default())
            .unwrap()
            .path
    }

    #[test]
    fn native_notifications_update_an_isolated_index_without_rescanning() {
        let dir = tempfile::tempdir().unwrap();
        let path = photo(dir.path());
        let stop = Stop::default();
        let mut index = Index::default();
        index
            .begin_batch_with(dir.path(), &stop, Watch::new)
            .unwrap();
        assert!(!index.degraded);
        assert!(index.files.contains(&path));
        fs::remove_file(&path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while index.files.contains(&path) {
            index.sync(dir.path(), &stop).unwrap();
            assert!(
                Instant::now() < deadline,
                "native deletion notification missing"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            index.scans, 1,
            "a rescan must not masquerade as a notification"
        );
    }

    #[test]
    fn unavailable_notifications_scan_once_per_batch_then_recover() {
        let dir = tempfile::tempdir().unwrap();
        let stop = Stop::default();
        let mut index = Index::default();
        let unavailable = |_: &Path| Err(anyhow::anyhow!("notifications unsupported"));
        index
            .begin_batch_with(dir.path(), &stop, unavailable)
            .unwrap();
        assert_eq!(index.mode_change.take(), Some(true));
        let path = photo(dir.path());
        for _ in 0..3 {
            index.sync(dir.path(), &stop).unwrap();
        }
        assert_eq!(index.scans, 1);
        assert!(
            index.files.is_empty(),
            "external changes are reconciled at the next batch"
        );
        index
            .begin_batch_with(dir.path(), &stop, unavailable)
            .unwrap();
        assert_eq!(index.scans, 2);
        assert!(index.files.contains(&path));
        assert_eq!(
            index.mode_change.take(),
            None,
            "do not repeat the degradation log"
        );
        let cancelled = Stop::default();
        cancelled.cancel();
        assert!(index.scan(&cancelled).is_err());
        assert!(
            index.files.contains(&path),
            "cancelled scan must not publish a partial inventory"
        );
        index
            .begin_batch_with(dir.path(), &stop, Watch::new)
            .unwrap();
        assert_eq!(index.mode_change.take(), Some(false));
        assert!(!index.degraded);
    }
}
