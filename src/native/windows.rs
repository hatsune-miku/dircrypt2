//! Windows namespace operations. Child lookups are one component relative to a
//! verified directory handle; reparse points are opened, never traversed.
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::c_void,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    mem::{offset_of, size_of},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
    ptr,
    sync::Arc,
};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*};

use super::{DIRECTORY, Entry, Info, Name};

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: HANDLE,
    name: *const UnicodeString,
    attributes: u32,
    security: *const c_void,
    qos: *const c_void,
}
#[repr(C)]
struct IoStatus {
    status: usize,
    information: usize,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut HANDLE,
        access: u32,
        object: *const ObjectAttributes,
        status: *mut IoStatus,
        allocation: *const i64,
        attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_size: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
    fn NtSetInformationFile(
        handle: HANDLE,
        status: *mut IoStatus,
        information: *const c_void,
        length: u32,
        class: u32,
    ) -> i32;
}

pub struct Handle {
    file: File,
}
pub struct Dir {
    handle: Handle,
    pub info: Info,
}

impl Handle {
    fn raw(&self) -> HANDLE {
        self.file.as_raw_handle() as HANDLE
    }
    pub fn info(&self) -> Result<Info> {
        let mut value: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        win(unsafe { GetFileInformationByHandle(self.raw(), &mut value) })?;
        Ok(Info {
            id: ((value.nFileIndexHigh as u64) << 32) | value.nFileIndexLow as u64,
            volume: value.dwVolumeSerialNumber as u64,
            size: ((value.nFileSizeHigh as u64) << 32) | value.nFileSizeLow as u64,
            born: time_value(value.ftCreationTime),
            modified: time_value(value.ftLastWriteTime),
            modified_subtick: 0,
            attributes: value.dwFileAttributes,
            links: value.nNumberOfLinks,
        })
    }
    pub fn rename(&self, destination: &Dir, name: &Name) -> Result<()> {
        name.validate()?;
        let wide = name.wide();
        let bytes = offset_of!(FILE_RENAME_INFO, FileName) + (wide.len() + 1) * 2;
        let mut buffer = vec![0u64; bytes.div_ceil(8)];
        let info = buffer.as_mut_ptr() as *mut FILE_RENAME_INFO;
        unsafe {
            (*info).RootDirectory = destination.handle.raw();
            (*info).FileNameLength = (wide.len() * 2) as u32;
            // Zeroed ReplaceIfExists: unrelated destinations are never replaced.
            ptr::copy_nonoverlapping(wide.as_ptr(), (*info).FileName.as_mut_ptr(), wide.len());
            let mut status = IoStatus {
                status: 0,
                information: 0,
            };
            nt_retry(|| {
                NtSetInformationFile(self.raw(), &mut status, info.cast(), bytes as u32, 10)
            })
            .with_context(|| format!("Renaming to {:?}", name.display()))?;
        }
        Ok(())
    }
    pub fn header(&mut self) -> Result<[u8; 16]> {
        let mut bytes = [0u8; 16];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
    pub fn write_header(&mut self, bytes: &[u8; 16], saved: &Info, flush: bool) -> Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        #[cfg(debug_assertions)]
        if std::env::var("DIRCRYPT_TEST_CRASH_POINT").as_deref() == Ok("header_partial") {
            self.file.write_all(&bytes[..7])?;
            crate::engine::fault("header_partial");
            self.file.seek(SeekFrom::Start(0))?;
        }
        self.file.write_all(bytes)?;
        let value = FILETIME {
            dwLowDateTime: saved.modified as u32,
            dwHighDateTime: (saved.modified >> 32) as u32,
        };
        win(unsafe { SetFileTime(self.raw(), ptr::null(), ptr::null(), &value) })?;
        if flush {
            self.file.sync_all()?;
        }
        Ok(())
    }
    pub fn delete_empty(&self) -> Result<()> {
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        win(unsafe {
            SetFileInformationByHandle(
                self.raw(),
                FileDispositionInfo,
                (&info as *const FILE_DISPOSITION_INFO).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        })?;
        Ok(())
    }
}

impl Dir {
    pub fn root(path: &Path) -> Result<Arc<Self>> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error().into());
        }
        let handle = Handle {
            file: unsafe { File::from_raw_handle(raw) },
        };
        let info = handle.info()?;
        ensure!(info.kind() == DIRECTORY, "Root must be a plain directory");
        Ok(Arc::new(Self { handle, info }))
    }
    pub fn filesystem(&self) -> Result<String> {
        let mut name = [0u16; 256];
        win(unsafe {
            GetVolumeInformationByHandleW(
                self.handle.raw(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        })?;
        Ok(String::from_utf16_lossy(
            &name[..name.iter().position(|c| *c == 0).unwrap_or(name.len())],
        ))
    }
    fn child(&self, name: &Name, access: u32, create: bool, directory: bool) -> Result<Handle> {
        let share = FILE_SHARE_READ
            | FILE_SHARE_DELETE
            | if access & FILE_WRITE_DATA == 0 {
                FILE_SHARE_WRITE
            } else {
                0
            };
        self.child_shared(name, access, create, directory, share)
    }
    fn child_shared(
        &self,
        name: &Name,
        access: u32,
        create: bool,
        directory: bool,
        share: u32,
    ) -> Result<Handle> {
        name.validate()?;
        let wide = name.wide();
        let string = UnicodeString {
            length: (wide.len() * 2) as u16,
            maximum_length: (wide.len() * 2) as u16,
            buffer: wide.as_ptr(),
        };
        let object = ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root: self.handle.raw(),
            name: &string,
            attributes: 0x40,
            security: ptr::null(),
            qos: ptr::null(),
        };
        let mut status = IoStatus {
            status: 0,
            information: 0,
        };
        let mut raw = ptr::null_mut();
        nt_retry(|| unsafe {
            NtCreateFile(
                &mut raw,
                access | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                &object,
                &mut status,
                ptr::null(),
                FILE_ATTRIBUTE_NORMAL,
                share,
                if create { 2 } else { 1 }, // FILE_CREATE / FILE_OPEN, never overwrite.
                0x00200000 | 0x20 | if directory { 1 } else { 0 }, // OPEN_REPARSE_POINT, synchronous
                ptr::null(),
                0,
            )
        })
        .with_context(|| format!("Opening {:?}", name.display()))?;
        Ok(Handle {
            file: unsafe { File::from_raw_handle(raw) },
        })
    }
    pub fn open_dir(&self, name: &Name) -> Result<Arc<Self>> {
        let handle = self.child(name, FILE_LIST_DIRECTORY | DELETE, false, true)?;
        let info = handle.info()?;
        ensure!(
            info.kind() == DIRECTORY,
            "Refusing to traverse reparse point {:?}",
            name.display()
        );
        Ok(Arc::new(Self { handle, info }))
    }
    pub fn control_dir(&self, name: &Name, create: bool) -> Result<Arc<Self>> {
        // SQLite opens this directory by path. Pin its name until it closes.
        let handle = self.child_shared(
            name,
            FILE_LIST_DIRECTORY | DELETE,
            create,
            true,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )?;
        let info = handle.info()?;
        ensure!(info.kind() == DIRECTORY, "DCDATA must be a plain directory");
        Ok(Arc::new(Self { handle, info }))
    }
    pub fn create_dir(&self, name: &Name) -> Result<Arc<Self>> {
        let handle = self.child(name, FILE_LIST_DIRECTORY | DELETE, true, true)?;
        let info = handle.info()?;
        Ok(Arc::new(Self { handle, info }))
    }
    pub fn open_item(&self, name: &Name, header: bool) -> Result<Handle> {
        self.child(
            name,
            DELETE
                | if header {
                    FILE_READ_DATA | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES
                } else {
                    0
                },
            false,
            false,
        )
    }
    pub fn remove_empty(&self) -> Result<()> {
        self.handle.delete_empty()
    }
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let mut result = Vec::new();
        let mut buffer = vec![0u64; 8192];
        let mut class = FileIdBothDirectoryRestartInfo;
        loop {
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    self.handle.raw(),
                    class,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 8) as u32,
                )
            };
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    break;
                }
                return Err(error).context("Enumerating directory by handle");
            }
            class = FileIdBothDirectoryInfo;
            let mut offset = 0usize;
            loop {
                ensure!(
                    offset + offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) <= buffer.len() * 8,
                    "Invalid directory response"
                );
                let raw = unsafe {
                    &*((buffer.as_ptr().cast::<u8>().add(offset)).cast::<FILE_ID_BOTH_DIR_INFO>())
                };
                let length = raw.FileNameLength as usize;
                ensure!(
                    length.is_multiple_of(2)
                        && offset + offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) + length
                            <= buffer.len() * 8,
                    "Invalid directory name response"
                );
                use std::os::windows::ffi::OsStringExt;
                let name = Name(std::ffi::OsString::from_wide(unsafe {
                    std::slice::from_raw_parts(raw.FileName.as_ptr(), length / 2)
                }));
                if name.0 != "." && name.0 != ".." {
                    name.validate()?;
                    result.push(Entry {
                        name,
                        info: Info {
                            id: raw.FileId as u64,
                            volume: self.info.volume,
                            size: raw.EndOfFile as u64,
                            born: raw.CreationTime as u64,
                            modified: raw.LastWriteTime as u64,
                            modified_subtick: 0,
                            attributes: raw.FileAttributes,
                            links: 0,
                        },
                    });
                }
                if raw.NextEntryOffset == 0 {
                    break;
                }
                ensure!(
                    raw.NextEntryOffset as usize >= offset_of!(FILE_ID_BOTH_DIR_INFO, FileName),
                    "Invalid directory offset"
                );
                offset += raw.NextEntryOffset as usize;
            }
        }
        Ok(result)
    }
    pub fn rename_child(&self, name: &Name, target: &Dir, new_name: &Name) -> Result<()> {
        self.open_item(name, false)?.rename(target, new_name)
    }
}

fn time_value(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}
fn nt_retry(mut operation: impl FnMut() -> i32) -> io::Result<()> {
    let mut code = 0;
    for delay in [0, 25, 75, 150] {
        if delay != 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
        let status = operation();
        if status >= 0 {
            return Ok(());
        }
        code = unsafe { RtlNtStatusToDosError(status) };
        if code != ERROR_SHARING_VIOLATION && code != ERROR_LOCK_VIOLATION {
            break;
        }
    }
    Err(io::Error::from_raw_os_error(code as i32))
}
fn win(value: i32) -> io::Result<()> {
    if value == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn check_volume(fs: &str) -> Result<()> {
    if !["NTFS", "FAT", "FAT32", "exFAT"]
        .iter()
        .any(|f| f.eq_ignore_ascii_case(fs))
    {
        bail!(
            "Unsupported filesystem {fs:?}; this version supports NTFS and FAT-family local volumes"
        );
    }
    Ok(())
}

pub fn lock_directory(path: &Path) -> Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)?;
    let info = file.metadata()?;
    ensure!(
        info.is_file() && info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "Lock path is not a regular file"
    );
    file.try_lock()
        .context("Another dircrypt process holds this directory's lock")?;
    Ok(file)
}
pub fn database_guard(path: &Path, create: bool) -> Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    let file = File::options()
        .read(true)
        .write(create)
        .create_new(create)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)?;
    let info = file.metadata()?;
    ensure!(
        info.is_file() && info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "Recovery database must be a regular file"
    );
    let handle = Handle { file };
    ensure!(
        handle.info()?.links == 1,
        "Recovery database must not have shared hard links"
    );
    Ok(handle.file)
}
pub fn root_matches(actual: &Info, recorded: &Info, filesystem: &str) -> bool {
    actual.volume == recorded.volume
        && if super::stable_ids(filesystem) {
            actual.id == recorded.id
        } else {
            actual.born == recorded.born
        }
}
pub fn cache_limit(_: usize) -> usize {
    256
}
impl Dir {
    pub fn sync(&self) -> Result<()> {
        Ok(())
    }
    pub fn database_path(&self, root: &Path, name: &str) -> std::path::PathBuf {
        root.join(name)
    }
}
