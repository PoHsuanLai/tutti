//! The Property Data of a message in its encoding (M2-103 §6.1.6, Table 12):
//! ASCII as it is, Mcoded7, or zlib compression (RFC 1950) then Mcoded7.

use std::num::NonZeroUsize;

use miniz_oxide::deflate::compress_to_vec_zlib;
use miniz_oxide::inflate::{TINFLStatus, decompress_to_vec_zlib_with_limit};

use super::PeEncoding;
use super::limits::NotInRange;
use super::mcoded7::{self, Mcoded7Error};

/// The zlib compression level of [`encode_body`]: 6, zlib's default.
const ZLIB_LEVEL: u8 = 6;

/// The most bytes a decoded body may have: the caller's bound on what a peer
/// can make it allocate, since a few bytes of zlib inflate to a great many.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "usize", into = "usize"))]
pub struct BodyLimit(NonZeroUsize);

impl BodyLimit {
    /// The limit, or `None` for 0.
    #[must_use]
    pub const fn new(bytes: usize) -> Option<Self> {
        match NonZeroUsize::new(bytes) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The limit in bytes.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl TryFrom<usize> for BodyLimit {
    type Error = NotInRange<usize>;

    fn try_from(raw: usize) -> Result<Self, NotInRange<usize>> {
        Self::new(raw).ok_or(NotInRange {
            what: "body limit",
            raw,
        })
    }
}

impl From<BodyLimit> for usize {
    fn from(limit: BodyLimit) -> usize {
        limit.get()
    }
}

/// A body that cannot be encoded or decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PeBodyError {
    /// ASCII Property Data holds 7-bit bytes only.
    #[error("byte {at} is not 7-bit ASCII")]
    NotAscii {
        /// The offset of the byte.
        at: usize,
    },
    /// The Mcoded7 layer is malformed.
    #[error("Mcoded7: {0}")]
    Mcoded7(#[from] Mcoded7Error),
    /// The decoded body is longer than the [`BodyLimit`].
    #[error("the decoded body is longer than {} bytes", .0.get())]
    TooLarge(BodyLimit),
    /// The zlib stream ends before its last block or its checksum.
    #[error("the zlib stream is truncated")]
    Truncated,
    /// The checksum at the end of the zlib stream is not the Adler-32 of the data
    /// (RFC 1950 §2.2).
    #[error("the zlib stream's Adler-32 checksum is wrong")]
    BadChecksum,
    /// The zlib header or the deflate data is invalid.
    #[error("the zlib stream is invalid")]
    BadStream,
}

/// The bytes Property Data takes on the wire: `body` in `encoding`.
///
/// # Errors
///
/// [`PeBodyError::NotAscii`] for a byte past 7 bits in an ASCII body.
pub fn encode_body(encoding: PeEncoding, body: &[u8]) -> Result<Vec<u8>, PeBodyError> {
    match encoding {
        PeEncoding::Ascii => {
            ascii(body)?;
            Ok(body.to_vec())
        }
        PeEncoding::Mcoded7 => Ok(mcoded7::encode_vec(body)),
        PeEncoding::ZlibMcoded7 => Ok(mcoded7::encode_vec(&compress_to_vec_zlib(body, ZLIB_LEVEL))),
    }
}

/// The body that Property Data on the wire carries in `encoding`, of at most
/// `limit` bytes.
///
/// For [`PeEncoding::ZlibMcoded7`] the wire is Mcoded7 decoded, then
/// inflated as a zlib stream, header and Adler-32 included. The inflation stops
/// at `limit`, so a bomb costs the caller `limit` bytes and no more.
///
/// # Errors
///
/// [`PeBodyError`] naming the layer that broke, and [`PeBodyError::TooLarge`]
/// for a body past `limit`.
pub fn decode_body(
    encoding: PeEncoding,
    wire: &[u8],
    limit: BodyLimit,
) -> Result<Vec<u8>, PeBodyError> {
    let body = match encoding {
        PeEncoding::Ascii => {
            ascii(wire)?;
            wire.to_vec()
        }
        PeEncoding::Mcoded7 => mcoded7::decode_vec(wire)?,
        PeEncoding::ZlibMcoded7 => {
            let compressed = mcoded7::decode_vec(wire)?;
            // The inflation itself stops at the limit.
            return decompress_to_vec_zlib_with_limit(&compressed, limit.get()).map_err(
                |e| match e.status {
                    TINFLStatus::HasMoreOutput => PeBodyError::TooLarge(limit),
                    TINFLStatus::Adler32Mismatch => PeBodyError::BadChecksum,
                    TINFLStatus::FailedCannotMakeProgress | TINFLStatus::NeedsMoreInput => {
                        PeBodyError::Truncated
                    }
                    _ => PeBodyError::BadStream,
                },
            );
        }
    };
    if body.len() > limit.get() {
        return Err(PeBodyError::TooLarge(limit));
    }
    Ok(body)
}

fn ascii(bytes: &[u8]) -> Result<(), PeBodyError> {
    match bytes.iter().position(|b| !b.is_ascii()) {
        Some(at) => Err(PeBodyError::NotAscii { at }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;
