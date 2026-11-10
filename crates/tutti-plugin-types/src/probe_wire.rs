//! The probe's result file: the wire between `tutti-plugin-probe` and the host
//! that spawned it.
//!
//! The probe writes its outcome to `<result>.tmp` and renames it to `<result>`,
//! so the final path holds nothing or a whole result, never a prefix of one. The
//! payload is framed (a magic, a length, the bytes) as well, so a file that is
//! cut short some other way is refused rather than decoded as far as it goes.
//!
//! Present means finished: a descriptor, or a typed failure. Absent means the
//! probe did not finish (a crash or a hang), which the reader tells from the
//! process, not from the file.

use crate::PluginDescriptor;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Leads every result file; the last byte is the format version.
const MAGIC: [u8; 5] = *b"TPRB\x01";
/// Magic, then the payload length as a little-endian `u64`.
const HEADER: usize = MAGIC.len() + 8;

/// What a probe that finished found.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum ProbeOutcome {
    /// The plugin loaded far enough to read its descriptor.
    Described(PluginDescriptor),
    /// The probe ran to its end and the plugin could not be read. The message is
    /// the loader's own reason.
    Failed(String),
}

/// Why a result file could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ResultError {
    /// The file is shorter than its own header or its declared payload.
    #[error("the probe result is truncated")]
    Truncated,
    /// The file does not begin with a result's magic and version.
    #[error("not a probe result (bad magic or version)")]
    BadMagic,
    /// The payload does not decode as a [`ProbeOutcome`].
    #[error("malformed probe result: {0}")]
    Malformed(String),
    /// The file exists and could not be read.
    #[error("probe result unreadable: {0}")]
    Io(String),
}

/// The bytes of a result file for `outcome`.
///
/// # Panics
///
/// Never in practice: a [`ProbeOutcome`] holds strings, numbers and enums, which
/// bincode writes to a `Vec` without failing.
pub fn encode_result(outcome: &ProbeOutcome) -> Vec<u8> {
    // A `ProbeOutcome` holds only strings, numbers and enums, which bincode
    // writes to a `Vec` without failing.
    let payload = bincode::serialize(outcome).expect("a ProbeOutcome serializes to a Vec");
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes
}

/// The outcome in `bytes`, which must be a whole result file.
///
/// # Errors
///
/// [`ResultError::Truncated`] if the file is cut short, [`ResultError::BadMagic`]
/// if it is not a result, [`ResultError::Malformed`] if the payload does not
/// decode or is followed by more bytes.
///
/// # Panics
///
/// Never: the eight-byte slice converts to an array by construction.
pub fn decode_result(bytes: &[u8]) -> Result<ProbeOutcome, ResultError> {
    let Some((magic, rest)) = bytes.split_at_checked(MAGIC.len()) else {
        return Err(ResultError::Truncated);
    };
    if magic != MAGIC {
        return Err(ResultError::BadMagic);
    }
    let Some((len, payload)) = rest.split_at_checked(8) else {
        return Err(ResultError::Truncated);
    };
    let len = u64::from_le_bytes(len.try_into().expect("eight bytes"));
    if (payload.len() as u64) < len {
        return Err(ResultError::Truncated);
    }
    if (payload.len() as u64) > len {
        return Err(ResultError::Malformed("bytes after the payload".into()));
    }
    bincode::deserialize(payload).map_err(|e| ResultError::Malformed(e.to_string()))
}

/// Where `write_result` stages `result` before it renames it into place.
pub fn result_tmp_path(result: &Path) -> PathBuf {
    let mut name: OsString = result.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Writes `outcome` to `result`: to `<result>.tmp` first, flushed to disk, then
/// renamed over `result`, so a reader sees all of it or none.
///
/// # Errors
///
/// Any I/O error from the write or the rename; the staging file is removed.
pub fn write_result(result: &Path, outcome: &ProbeOutcome) -> io::Result<()> {
    let tmp = result_tmp_path(result);
    let staged = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&encode_result(outcome))?;
        file.sync_all()
    })();
    let moved = staged.and_then(|()| std::fs::rename(&tmp, result));
    if moved.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    moved
}

/// The outcome at `result`, or `None` if the probe left no file there. The
/// staging file is never looked at.
///
/// # Errors
///
/// [`ResultError`] if a file is there and is not a whole result.
pub fn read_result(result: &Path) -> Result<Option<ProbeOutcome>, ResultError> {
    match std::fs::read(result) {
        Ok(bytes) => decode_result(&bytes).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ResultError::Io(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn described() -> ProbeOutcome {
        ProbeOutcome::Described(PluginDescriptor {
            id: "clap.test".into(),
            name: "Test".into(),
            ..PluginDescriptor::default()
        })
    }

    /// Both outcomes survive the file.
    ///
    /// Mutation (run): `encode_result` serialising a fixed `Failed("")` instead
    /// of its argument fails the round trip.
    #[test]
    fn an_outcome_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result");
        for outcome in [described(), ProbeOutcome::Failed("no factory".into())] {
            write_result(&path, &outcome).unwrap();
            assert_eq!(read_result(&path).unwrap(), Some(outcome));
        }
    }

    /// No file is `None`, and the staging file is not a result.
    ///
    /// Mutation (run): `read_result` reading `result_tmp_path(result)` fails it.
    #[test]
    fn only_the_final_path_is_a_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result");
        assert_eq!(read_result(&path), Ok(None));
        std::fs::write(result_tmp_path(&path), encode_result(&described())).unwrap();
        assert_eq!(read_result(&path), Ok(None), "a staged result is not one");
    }

    /// Every proper prefix of a result is refused, none decodes.
    ///
    /// Mutation (run): dropping the length comparison in `decode_result` makes
    /// the prefixes that cut the payload decode as garbage instead of
    /// `Truncated`, failing the loop; `read_result` turning a decode error into
    /// `Ok(None)` (a missing file) fails the last assertion.
    #[test]
    fn a_cut_short_file_is_never_a_result() {
        let bytes = encode_result(&described());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result");
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(read_result(&path), Err(ResultError::Truncated));
        for cut in 0..bytes.len() {
            let r = decode_result(&bytes[..cut]);
            assert!(
                matches!(r, Err(ResultError::Truncated | ResultError::BadMagic)),
                "a prefix of {cut} bytes decoded as {r:?}"
            );
        }
        assert_eq!(decode_result(&bytes), Ok(described()));
    }

    /// Bytes that are not a result are refused by name.
    #[test]
    fn foreign_bytes_are_refused() {
        assert_eq!(
            decode_result(b"garbage!garbage!"),
            Err(ResultError::BadMagic)
        );
        let mut extra = encode_result(&described());
        extra.push(0);
        assert!(matches!(
            decode_result(&extra),
            Err(ResultError::Malformed(_))
        ));
    }
}
