//! The one framing of the control socket, shared by the host and the
//! subprocess.
//!
//! A frame is a four-byte big-endian length, then that many bytes of bincode.
//! The length is bounded by [`MAX_FRAME_BYTES`] on both ends: a sender refuses
//! to emit a frame its peer must reject (and never lets a cast truncate the
//! prefix, which would desynchronise a stream that has no resynchronisation
//! point), and a receiver refuses an advertised length before allocating for
//! it.
//!
//! The host reads incrementally against a deadline, so it takes the prefix and
//! body halves ([`decode_prefix`], [`decode_body`]); the subprocess blocks, and
//! takes [`read_frame`]. Both send through [`write_frame`].
//!
//! **A frame costs no allocation once its buffers are warm** (doc 027 P2b).
//! Each thread writes through one buffer and the subprocess reads through one
//! (the host's read buffer is its `PartialFrame`'s), each kept between frames
//! and trimmed back to [`KEEP_BYTES`] after a larger one ([`keep`]). A message
//! with no heap fields (a block's reply) then decodes without allocating too.

use std::cell::RefCell;
use std::io::{Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use super::MAX_FRAME_BYTES;
use crate::error::{BridgeError, Result};

/// A frame whose length is over [`MAX_FRAME_BYTES`].
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("frame of {len} bytes is over the {MAX_FRAME_BYTES}-byte protocol limit")]
pub struct FrameTooLarge {
    /// The body length, in bytes, that was advertised or about to be sent.
    pub len: usize,
}

impl From<FrameTooLarge> for BridgeError {
    fn from(e: FrameTooLarge) -> Self {
        Self::ConnectionFailed(e.to_string())
    }
}

/// The length prefix for a body of `body_len` bytes.
///
/// # Errors
///
/// [`FrameTooLarge`] when `body_len` is over [`MAX_FRAME_BYTES`].
pub fn encode_prefix(body_len: usize) -> std::result::Result<[u8; 4], FrameTooLarge> {
    if body_len > MAX_FRAME_BYTES {
        return Err(FrameTooLarge { len: body_len });
    }
    // The cap is checked at compile time to fit a `u32` (the `const _` below).
    Ok((body_len as u32).to_be_bytes())
}

/// The body length a prefix advertises.
///
/// # Errors
///
/// [`FrameTooLarge`] when it is over [`MAX_FRAME_BYTES`]; no body is to be
/// read or allocated for it.
pub fn decode_prefix(prefix: [u8; 4]) -> std::result::Result<usize, FrameTooLarge> {
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(FrameTooLarge { len });
    }
    Ok(len)
}

/// Decodes one frame's body.
///
/// # Errors
///
/// A bincode error, which is a protocol-version skew the handshake missed.
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    Ok(bincode::deserialize(body)?)
}

/// How much capacity a frame buffer keeps between frames. A block's frames
/// are a few KiB; a state chunk is far larger and rare, so a buffer it grew is
/// trimmed back rather than held for the connection's life.
pub(crate) const KEEP_BYTES: usize = 256 * 1024;

/// Trim `buf` back to [`KEEP_BYTES`] of capacity after a larger frame.
pub(crate) fn keep(buf: &mut Vec<u8>) {
    if buf.capacity() > KEEP_BYTES {
        buf.clear();
        buf.shrink_to(KEEP_BYTES);
    }
}

thread_local! {
    /// This thread's outgoing frame: prefix and body, serialized in place.
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// This thread's incoming body, for [`read_frame`].
    static IN: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Serializes `msg` and writes it as one frame, in one write, then flushes.
///
/// The frame is serialized into this thread's reused buffer behind a prefix
/// placeholder, so nothing is allocated once the buffer is warm, and one
/// `write_all` sends prefix and body together.
///
/// # Errors
///
/// [`FrameTooLarge`] (as [`BridgeError::ConnectionFailed`]) when the body is
/// over the cap, with nothing written; a serialization or I/O error otherwise.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    OUT.with_borrow_mut(|frame| {
        frame.clear();
        frame.extend_from_slice(&[0; 4]);
        let written = bincode::serialize_into(&mut *frame, msg)
            .map_err(BridgeError::from)
            .and_then(|()| Ok(encode_prefix(frame.len() - 4)?))
            .and_then(|prefix| {
                frame[..4].copy_from_slice(&prefix);
                w.write_all(frame)?;
                Ok(w.flush()?)
            });
        keep(frame);
        written
    })
}

/// Blocks until one whole frame has been read, and decodes it.
///
/// # Errors
///
/// An I/O error on EOF or a short read; [`FrameTooLarge`] (as
/// [`BridgeError::ConnectionFailed`]) for an advertised length over the cap,
/// before any allocation; a bincode error for a body that does not decode.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    let mut prefix = [0u8; 4];
    r.read_exact(&mut prefix)?;
    let len = decode_prefix(prefix)?;
    IN.with_borrow_mut(|body| {
        body.clear();
        body.resize(len, 0);
        let msg = r
            .read_exact(body)
            .map_err(BridgeError::from)
            .and_then(|()| decode_body(body));
        keep(body);
        msg
    })
}

/// The two bounds `MAX_FRAME_BYTES` has to sit between, checked at compile time.
///
/// A `const` block rather than a `#[test]`: both operands are constants, so
/// there is nothing to observe at runtime that the compiler cannot settle
/// first, and a bad cap should fail the build rather than one test binary.
/// (Clippy says the same thing via `assertions_on_constants`.)
const _: () = {
    // `encode_prefix` writes `body_len as u32`. That cast is only safe because
    // the guard above it rejects everything larger, so this asserts the premise the
    // cast depends on. Raise the constant past `u32::MAX` and the guard stops
    // covering the truncation it was written to prevent — silently, since the
    // cast itself never fails.
    assert!(
        MAX_FRAME_BYTES <= u32::MAX as usize,
        "MAX_FRAME_BYTES exceeds what a u32 length prefix can carry, so \
         `body_len as u32` in `encode_prefix` can still truncate and desynchronise \
         the stream"
    );

    // The cap must also leave room for the largest message the protocol
    // defines. The binding case is a plugin state chunk (`LoadState` /
    // `StateData`), which for a sample-based instrument reaches single-digit
    // MiB. A cap below that would turn preset loading into a connection
    // failure — the regression a "tighten it until nothing complains" edit
    // would cause, and one no test in this crate would catch, since none sends
    // a realistically large state chunk.
    assert!(
        MAX_FRAME_BYTES >= 16 * 1024 * 1024,
        "MAX_FRAME_BYTES is below the state chunk a sampler can legitimately \
         produce, so `save_state`/`load_state` would fail on real plugins"
    );
};

#[cfg(test)]
mod tests {
    use super::*;

    /// **The frame is a big-endian `u32` length and the bincode body.** Bytes
    /// written out by hand for a `u16` message, so a change of prefix width or
    /// endianness fails here rather than as a desync between two binaries.
    ///
    /// Mutation (run): `to_be_bytes` → `to_le_bytes` in `encode_prefix` →
    /// the written bytes differ → fails.
    #[test]
    fn a_frame_is_a_big_endian_length_then_the_body() {
        let mut out = Vec::new();
        write_frame(&mut out, &0x0102u16).unwrap();
        assert_eq!(out, [0, 0, 0, 2, 0x02, 0x01]);

        let back: u16 = read_frame(&mut out.as_slice()).unwrap();
        assert_eq!(back, 0x0102);
    }

    /// **The cap is inclusive on both halves, and a length over it names the
    /// length.** The decode side refuses before any body is read.
    ///
    /// Mutation (run): `>` → `>=` in `decode_prefix` → the at-cap prefix is
    /// refused → fails.
    #[test]
    fn the_cap_is_inclusive_and_over_it_is_typed() {
        let at = u32::try_from(MAX_FRAME_BYTES).unwrap();
        assert_eq!(decode_prefix(at.to_be_bytes()), Ok(MAX_FRAME_BYTES));
        assert_eq!(
            decode_prefix((at + 1).to_be_bytes()),
            Err(FrameTooLarge {
                len: MAX_FRAME_BYTES + 1
            })
        );
        assert_eq!(encode_prefix(MAX_FRAME_BYTES), Ok(at.to_be_bytes()));
        assert_eq!(
            encode_prefix(MAX_FRAME_BYTES + 1),
            Err(FrameTooLarge {
                len: MAX_FRAME_BYTES + 1
            })
        );
    }

    /// An oversize prefix on a stream is refused with no body present, and the
    /// error is the connection-failed form carrying the length.
    #[test]
    fn read_frame_refuses_an_oversize_prefix_without_a_body() {
        let prefix = u32::try_from(MAX_FRAME_BYTES + 1).unwrap().to_be_bytes();
        match read_frame::<_, u16>(&mut prefix.as_slice()) {
            Err(BridgeError::ConnectionFailed(m)) => {
                assert!(m.contains(&(MAX_FRAME_BYTES + 1).to_string()), "{m}");
            }
            other => panic!("expected ConnectionFailed, got {other:?}"),
        }
    }
}
