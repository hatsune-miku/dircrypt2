//! Platform-independent recovery records and platform-specific filesystem operations.
use anyhow::{Result, bail, ensure};
use std::{ffi::OsString, io};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

pub const FILE: i64 = 0;
pub const DIRECTORY: i64 = 1;
pub const LINK: i64 = 2;

/// Unicode names use UTF-8 on disk on every platform. Non-Unicode names retain
/// their native bytes (Unix) or UTF-16 units (Windows), never a lossy conversion.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Name(pub OsString);
impl Name {
    pub fn text(value: &str) -> Self {
        Self(value.into())
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let (&encoding, bytes) = bytes
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("Empty encoded name"))?;
        let name = match encoding {
            0 => Self(std::str::from_utf8(bytes)?.into()),
            #[cfg(unix)]
            1 => {
                use std::os::unix::ffi::OsStringExt;
                Self(OsString::from_vec(bytes.to_vec()))
            }
            #[cfg(windows)]
            2 => {
                use std::os::windows::ffi::OsStringExt;
                ensure!(bytes.len().is_multiple_of(2), "Invalid UTF-16 name");
                let units: Vec<_> = bytes
                    .chunks_exact(2)
                    .map(|v| u16::from_le_bytes([v[0], v[1]]))
                    .collect();
                Self(OsString::from_wide(&units))
            }
            _ => bail!("Filename encoding cannot be represented on this operating system"),
        };
        name.validate()?;
        ensure!(
            name.bytes() == [vec![encoding], bytes.to_vec()].concat(),
            "Noncanonical filename encoding"
        );
        Ok(name)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.0.is_empty() && self.0 != "." && self.0 != "..",
            "Invalid relative name"
        );
        #[cfg(windows)]
        {
            let units = self.wide();
            ensure!(units.len() <= 255, "Invalid component length");
            ensure!(
                !units.iter().any(|c| [0, 47, 92, 58].contains(c)),
                "Names must be single relative components"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bytes = self.0.as_bytes();
            ensure!(
                bytes.len() <= 255,
                "Name exceeds this platform's component limit"
            );
            ensure!(
                !bytes.contains(&0) && !bytes.contains(&b'/'),
                "Names must be single relative components"
            );
        }
        Ok(())
    }
    pub fn bytes(&self) -> Vec<u8> {
        if let Some(text) = self.0.to_str() {
            return [vec![0], text.as_bytes().to_vec()].concat();
        }
        #[cfg(windows)]
        {
            [
                vec![2],
                self.wide().iter().flat_map(|c| c.to_le_bytes()).collect(),
            ]
            .concat()
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            [vec![1], self.0.as_bytes().to_vec()].concat()
        }
    }
    pub fn key(&self) -> Vec<u8> {
        #[cfg(windows)]
        {
            self.wide()
                .iter()
                .flat_map(|c| {
                    if (97..=122).contains(c) {
                        (c - 32).to_le_bytes()
                    } else {
                        c.to_le_bytes()
                    }
                })
                .collect()
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            self.0.as_bytes().to_vec()
        }
    }
    pub fn display(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
    #[cfg(windows)]
    fn wide(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.0.encode_wide().collect()
    }
}

#[derive(Clone, Debug)]
pub struct Info {
    pub id: u64,
    pub volume: u64,
    pub size: u64,
    /// FILETIME-compatible 100 ns ticks, including on Unix.
    pub born: u64,
    pub modified: u64,
    /// Preserve the remaining Unix nanoseconds when writing a header.
    pub modified_subtick: u32,
    /// Portable type bits, using the Windows directory/reparse bit values.
    pub attributes: u32,
    pub links: u32,
}
impl Info {
    pub fn kind(&self) -> i64 {
        if self.attributes & 0x400 != 0 {
            LINK
        } else if self.attributes & 0x10 != 0 {
            DIRECTORY
        } else {
            FILE
        }
    }
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: Name,
    pub info: Info,
}

pub fn is_missing(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|e| e.downcast_ref::<io::Error>())
        .any(|e| e.kind() == io::ErrorKind::NotFound)
}
pub fn is_fat(fs: &str) -> bool {
    ["FAT", "FAT32", "exFAT"]
        .iter()
        .any(|v| fs.eq_ignore_ascii_case(v))
}
pub fn stable_ids(fs: &str) -> bool {
    !is_fat(fs)
}
