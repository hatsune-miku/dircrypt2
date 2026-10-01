//! Linux/macOS directory-descriptor operations. No path-based fallback is used
//! when the filesystem cannot provide an atomic rename without replacement.
use super::{DIRECTORY, Entry, FILE, Info, Name};
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::{CStr, CString, OsString},
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    sync::Arc,
};

const UNIX_EPOCH_TICKS: i128 = 116_444_736_000_000_000;

pub struct Handle {
    file: File,
    parent: Arc<File>,
    name: Name,
}
pub struct Dir {
    file: Arc<File>,
    location: Option<(Arc<File>, Name)>,
    pub info: Info,
}

fn component(name: &Name) -> Result<CString> {
    name.validate()?;
    Ok(CString::new(name.0.as_bytes())?)
}
fn check(code: libc::c_int) -> io::Result<()> {
    if code == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn open_at(parent: RawFd, name: &CStr, flags: libc::c_int) -> Result<File> {
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags | libc::O_CLOEXEC) };
    check(fd)?;
    // Ownership transfers to File exactly once after the syscall succeeded.
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn stat_at(parent: RawFd, name: &Name) -> Result<Info> {
    let name = component(name)?;
    let mut value: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstatat(parent, name.as_ptr(), &mut value, libc::AT_SYMLINK_NOFOLLOW) })?;
    info_from_stat(&value)
}
fn file_info(file: &File) -> Result<Info> {
    let mut value: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstat(file.as_raw_fd(), &mut value) })?;
    info_from_stat(&value)
}
fn ticks(seconds: i64, nanoseconds: i64) -> Result<(u64, u32)> {
    ensure!(
        (0..1_000_000_000).contains(&nanoseconds),
        "Invalid filesystem timestamp"
    );
    let value = UNIX_EPOCH_TICKS + i128::from(seconds) * 10_000_000 + i128::from(nanoseconds / 100);
    Ok((
        value
            .try_into()
            .context("Timestamp outside the recovery format's range")?,
        (nanoseconds % 100) as u32,
    ))
}
#[allow(clippy::unnecessary_cast, clippy::unnecessary_fallible_conversions)] // libc field widths differ between Darwin and Linux.
fn info_from_stat(value: &libc::stat) -> Result<Info> {
    let attributes = match value.st_mode & libc::S_IFMT {
        libc::S_IFREG => 0,
        libc::S_IFDIR => 0x10,
        libc::S_IFLNK => 0x400,
        _ => bail!(
            "Special files (sockets, devices and FIFOs) are unsupported; no contents were opened"
        ),
    };
    let (modified, modified_subtick) = ticks(value.st_mtime as i64, value.st_mtime_nsec as i64)?;
    #[cfg(target_os = "macos")]
    let born = ticks(value.st_birthtime, value.st_birthtime_nsec)?.0;
    #[cfg(target_os = "linux")]
    let born = 0; // POSIX stat has no birth time; stable local filesystems use inode identity.
    Ok(Info {
        id: value.st_ino as u64,
        volume: value.st_dev as u64,
        size: value.st_size.try_into().context("Invalid file size")?,
        born,
        modified,
        modified_subtick,
        attributes,
        links: value.st_nlink.try_into().context("Too many hard links")?,
    })
}
fn same_object(actual: &Info, expected: &Info) -> Result<()> {
    ensure!(
        actual.id == expected.id
            && actual.volume == expected.volume
            && actual.kind() == expected.kind(),
        "Directory entry changed while open; preserve DCDATA and stop concurrent writers"
    );
    Ok(())
}

impl Handle {
    pub fn info(&self) -> Result<Info> {
        file_info(&self.file)
    }
    pub fn rename(&self, destination: &Dir, name: &Name) -> Result<()> {
        // Unlike Windows, Unix renames a directory entry rather than an open
        // object. Recheck its identity immediately before the atomic syscall.
        same_object(
            &stat_at(self.parent.as_raw_fd(), &self.name)?,
            &self.info()?,
        )?;
        let old = component(&self.name)?;
        let new = component(name)?;
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                self.parent.as_raw_fd(),
                old.as_ptr(),
                destination.file.as_raw_fd(),
                new.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                self.parent.as_raw_fd(),
                old.as_ptr(),
                destination.file.as_raw_fd(),
                new.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        check(result)
            .with_context(|| format!("Renaming to {:?} without replacement", name.display()))
    }
    pub fn header(&mut self) -> Result<[u8; 16]> {
        let mut bytes = [0; 16];
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
        let nanoseconds = (i128::from(saved.modified) - UNIX_EPOCH_TICKS) * 100
            + i128::from(saved.modified_subtick);
        let times = [
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            libc::timespec {
                tv_sec: nanoseconds.div_euclid(1_000_000_000).try_into()?,
                tv_nsec: nanoseconds.rem_euclid(1_000_000_000).try_into()?,
            },
        ];
        check(unsafe { libc::futimens(self.file.as_raw_fd(), times.as_ptr()) })?;
        if flush {
            self.file.sync_all()?;
        }
        Ok(())
    }
    pub fn delete_empty(&self) -> Result<()> {
        let info = self.info()?;
        same_object(&stat_at(self.parent.as_raw_fd(), &self.name)?, &info)?;
        let name = component(&self.name)?;
        check(unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                name.as_ptr(),
                if info.kind() == DIRECTORY {
                    libc::AT_REMOVEDIR
                } else {
                    0
                },
            )
        })?;
        Ok(())
    }
}

impl Dir {
    pub fn root(path: &Path) -> Result<Arc<Self>> {
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let info = file_info(&file)?;
        ensure!(info.kind() == DIRECTORY, "Root must be a plain directory");
        Ok(Arc::new(Self {
            file: Arc::new(file),
            location: None,
            info,
        }))
    }
    pub fn filesystem(&self) -> Result<String> {
        let mut value: libc::statfs = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstatfs(self.file.as_raw_fd(), &mut value) })?;
        #[cfg(target_os = "linux")]
        let name = match value.f_type {
            0xef53 => "ext",
            0x58465342 => "xfs",
            0x9123683e => "btrfs",
            0x01021994 => "tmpfs",
            0xf2f52010 => "f2fs",
            0x2011bab0 => "exFAT",
            0x4d44 => "FAT",
            _ => bail!("Unsupported Linux filesystem type {:#x}; local ext, XFS, Btrfs, F2FS, tmpfs and FAT-family filesystems are supported", value.f_type),
        }.to_string();
        #[cfg(target_os = "macos")]
        let name = unsafe { CStr::from_ptr(value.f_fstypename.as_ptr()) }
            .to_str()?
            .to_string();
        Ok(if name.eq_ignore_ascii_case("msdos") {
            "FAT".into()
        } else {
            name
        })
    }
    pub fn open_dir(&self, name: &Name) -> Result<Arc<Self>> {
        let file = open_at(
            self.file.as_raw_fd(),
            &component(name)?,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )?;
        let info = file_info(&file)?;
        ensure!(info.kind() == DIRECTORY, "Refusing to traverse a symlink");
        ensure!(
            info.volume == self.info.volume,
            "Refusing to cross a nested filesystem mount"
        );
        Ok(Arc::new(Self {
            file: Arc::new(file),
            location: Some((self.file.clone(), name.clone())),
            info,
        }))
    }
    pub fn control_dir(&self, name: &Name, create: bool) -> Result<Arc<Self>> {
        if create {
            self.create_dir(name)
        } else {
            self.open_dir(name)
        }
    }
    pub fn create_dir(&self, name: &Name) -> Result<Arc<Self>> {
        check(unsafe { libc::mkdirat(self.file.as_raw_fd(), component(name)?.as_ptr(), 0o700) })?;
        self.open_dir(name)
    }
    pub fn open_item(&self, name: &Name, header: bool) -> Result<Handle> {
        #[cfg(target_os = "linux")]
        let inspect_flags = libc::O_PATH | libc::O_NOFOLLOW;
        #[cfg(target_os = "macos")]
        // O_SYMLINK opens the link itself; O_NOFOLLOW would reject it on Darwin.
        let inspect_flags = libc::O_EVTONLY | libc::O_SYMLINK;
        let flags = if header {
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK
        } else {
            inspect_flags
        };
        let file = open_at(self.file.as_raw_fd(), &component(name)?, flags)?;
        if header {
            ensure!(
                file_info(&file)?.kind() == FILE,
                "Header operations require a regular file"
            );
            file.try_lock()
                .context("Another process holds the file lock")?;
        }
        Ok(Handle {
            file,
            parent: self.file.clone(),
            name: name.clone(),
        })
    }
    pub fn remove_empty(&self) -> Result<()> {
        let (parent, name) = self
            .location
            .as_ref()
            .context("Cannot remove the root directory")?;
        same_object(&stat_at(parent.as_raw_fd(), name)?, &self.info)?;
        check(unsafe {
            libc::unlinkat(
                parent.as_raw_fd(),
                component(name)?.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        })?;
        Ok(())
    }
    pub fn entries(&self) -> Result<Vec<Entry>> {
        // Opening '.' creates an independent directory offset. dup() would share
        // offsets with concurrent scans and could silently omit files.
        let scan = open_at(
            self.file.as_raw_fd(),
            c".",
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )?;
        let raw = scan.into_raw_fd();
        let stream = unsafe { libc::fdopendir(raw) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(raw);
            }
            return Err(error.into());
        }
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let stream = Stream(stream);
        let mut entries = Vec::new();
        loop {
            #[cfg(target_os = "linux")]
            let errno = unsafe { libc::__errno_location() };
            #[cfg(target_os = "macos")]
            let errno = unsafe { libc::__error() };
            unsafe {
                *errno = 0;
            }
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                if unsafe { *errno } != 0 {
                    return Err(io::Error::last_os_error().into());
                }
                break;
            }
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let name = Name(OsString::from_vec(bytes.to_vec()));
            let info = stat_at(self.file.as_raw_fd(), &name)?;
            ensure!(
                info.volume == self.info.volume,
                "Refusing to cross a nested filesystem mount"
            );
            entries.push(Entry { name, info });
        }
        Ok(entries)
    }
    pub fn rename_child(&self, name: &Name, target: &Dir, new_name: &Name) -> Result<()> {
        self.open_item(name, false)?.rename(target, new_name)
    }
    pub fn sync(&self) -> Result<()> {
        self.file
            .sync_all()
            .context("Synchronizing recovery directory")
    }
    pub fn database_path(&self, root: &Path, name: &str) -> PathBuf {
        #[cfg(target_os = "linux")]
        {
            let _ = root;
            PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd())).join(name)
        }
        #[cfg(target_os = "macos")]
        {
            root.join(name)
        }
    }
}

pub fn lock_directory(path: &Path) -> Result<File> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.nlink() == 1,
        "Lock path must be a regular file without shared hard links"
    );
    file.try_lock()
        .context("Another dircrypt process holds this directory's lock")?;
    Ok(file)
}
pub fn database_guard(path: &Path, create: bool) -> Result<File> {
    let file = File::options()
        .read(true)
        .write(create)
        .create_new(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.nlink() == 1,
        "Recovery database must be a regular file without shared hard links"
    );
    Ok(file)
}
pub fn root_matches(actual: &Info, recorded: &Info, _: &str) -> bool {
    actual.volume == recorded.volume && actual.id == recorded.id
}
pub fn cache_limit(jobs: usize) -> usize {
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return 8;
    }
    // Each cached child can retain one parent descriptor. Reserve room for
    // SQLite, directory scans, control directories and temporary leaf handles.
    ((limit.rlim_cur as usize).saturating_sub(64) / (jobs.max(1) * 2 + 4)).clamp(1, 256)
}
pub fn check_volume(fs: &str) -> Result<()> {
    ensure!(
        [
            "apfs", "hfs", "ext", "xfs", "btrfs", "f2fs", "tmpfs", "FAT", "FAT32", "exFAT"
        ]
        .iter()
        .any(|v| fs.eq_ignore_ascii_case(v)),
        "Unsupported filesystem {fs:?}"
    );
    Ok(())
}
