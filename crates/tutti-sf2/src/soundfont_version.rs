use std::io;
use std::io::Read;

use crate::binary_reader::ReadExt;

/// Reperesents the version of a SoundFont.
#[derive(Debug)]
#[non_exhaustive]
pub struct SoundFontVersion {
    pub(crate) major: i16,
    pub(crate) minor: i16,
}

impl SoundFontVersion {
    pub(crate) fn default() -> Self {
        Self { major: 0, minor: 0 }
    }

    pub(crate) fn new<R: Read>(reader: &mut R) -> Result<Self, io::Error> {
        let major = reader.read_i16()?;
        let minor = reader.read_i16()?;

        Ok(Self { major, minor })
    }

    /// The major version.
    pub fn major(&self) -> i32 {
        self.major as i32
    }

    /// The minor version.
    pub fn minor(&self) -> i32 {
        self.minor as i32
    }
}
