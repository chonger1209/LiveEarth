//! One in-memory directory inventory, updated from Windows filename notifications.
//! Image decoding remains in reuse checks; indexing and deletion read metadata only.
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
    fn scan(&mut self) -> Result<()> {
        let mut files = BTreeSet::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if archive_name(&entry.path())
                && entry.file_type()?.is_file()
                && crate::storage::is_archive(&entry.path())
            {
                files.insert(entry.path());
            }
        }
        self.files = files;
        Ok(())
    }
    fn sync(&mut self, directory: &Path) -> Result<()> {
        if self.directory != directory || self.watch.is_none() {
            self.directory = directory.to_owned();
            self.watch = Some(Watch::new(directory)?);
            return self.scan();
        }
        match self.watch.as_mut().unwrap().changes() {
            Ok(Some(names)) => {
                for name in names.into_iter().collect::<BTreeSet<_>>() {
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
            Ok(None) => self.scan()?,
            Err(_) => {
                self.watch = Some(Watch::new(directory)?);
                self.scan()?;
            }
        }
        Ok(())
    }
}
pub fn files(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut index = INDEX
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Err(error) = index.sync(directory) {
        index.watch = None;
        index.files.clear();
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
