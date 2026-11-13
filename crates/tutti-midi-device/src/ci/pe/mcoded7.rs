//! Mcoded7, the encoding that carries 8-bit data in 7-bit bytes (M2-103 §6.1.7,
//! after the File Dump format of the MIDI 1.0 specification).
//!
//! Each group of up to seven data bytes becomes a sign byte and the low seven
//! bits of each data byte. The sign byte holds the top bits: the first data byte's
//! at bit 6, the second's at bit 5, and so on down to the seventh's at bit 0. A
//! last group of fewer than seven bytes is written the same way, its unused sign
//! bits zero, so `n` data bytes take [`encoded_len`]`(n)` bytes.
//!
//! The functions work over slices and the caller supplies the output, so nothing
//! here allocates; [`encode_vec`] and [`decode_vec`] allocate it for a control
//! thread.
//!
//! ```
//! use tutti_midi_device::mcoded7;
//!
//! let mut wire = [0u8; 4];
//! assert_eq!(mcoded7::encode(&[0x80, 0x01, 0xFF], &mut wire), Ok(4));
//! assert_eq!(wire, [0x50, 0x00, 0x01, 0x7F]);
//! assert_eq!(mcoded7::decode_vec(&wire), Ok(vec![0x80, 0x01, 0xFF]));
//! ```

/// The data bytes in a full group.
const GROUP: usize = 7;

/// Why a buffer could not be encoded or decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Mcoded7Error {
    /// The output slice is shorter than the result.
    #[error("the output holds {have} bytes and the result takes {need}")]
    OutputTooShort {
        /// Bytes the result takes.
        need: usize,
        /// Bytes the output holds.
        have: usize,
    },
    /// A byte of the encoded input has its top bit set.
    #[error("encoded byte {at} has its top bit set")]
    NotSevenBit {
        /// The offset of the byte.
        at: usize,
    },
    /// The input ends after a group's sign byte, which a group has data after.
    #[error("the encoded input ends after a sign byte")]
    Truncated,
}

/// The bytes `data_len` data bytes take once encoded.
#[must_use]
pub const fn encoded_len(data_len: usize) -> usize {
    let rest = data_len % GROUP;
    data_len / GROUP * (GROUP + 1) + if rest == 0 { 0 } else { rest + 1 }
}

/// The data bytes an encoded input of `encoded_len` bytes holds, or `None`
/// when no data encodes to that length.
#[must_use]
pub const fn decoded_len(encoded_len: usize) -> Option<usize> {
    let rest = encoded_len % (GROUP + 1);
    if rest == 1 {
        None
    } else {
        Some(encoded_len / (GROUP + 1) * GROUP + rest.saturating_sub(1))
    }
}

/// Encodes `data` into `out`, and returns the bytes written.
///
/// # Errors
///
/// [`Mcoded7Error::OutputTooShort`] when `out` is shorter than
/// [`encoded_len`]`(data.len())`.
pub fn encode(data: &[u8], out: &mut [u8]) -> Result<usize, Mcoded7Error> {
    let need = encoded_len(data.len());
    if out.len() < need {
        return Err(Mcoded7Error::OutputTooShort {
            need,
            have: out.len(),
        });
    }
    for (group, wire) in data.chunks(GROUP).zip(out[..need].chunks_mut(GROUP + 1)) {
        if let Some((sign, lows)) = wire.split_first_mut() {
            *sign = 0;
            for (i, (&byte, low)) in group.iter().zip(lows).enumerate() {
                *sign |= (byte >> 7) << (GROUP - 1 - i);
                *low = byte & 0x7F;
            }
        }
    }
    Ok(need)
}

/// Decodes the Mcoded7 bytes `wire` into `out`, and returns the bytes written.
///
/// The unused sign bits of a short last group are ignored.
///
/// # Errors
///
/// [`Mcoded7Error::NotSevenBit`] for a byte with its top bit set,
/// [`Mcoded7Error::Truncated`] for an input that ends after a sign byte, and
/// [`Mcoded7Error::OutputTooShort`] when `out` is shorter than
/// [`decoded_len`]`(wire.len())`.
pub fn decode(wire: &[u8], out: &mut [u8]) -> Result<usize, Mcoded7Error> {
    if let Some(at) = wire.iter().position(|b| b & 0x80 != 0) {
        return Err(Mcoded7Error::NotSevenBit { at });
    }
    let need = decoded_len(wire.len()).ok_or(Mcoded7Error::Truncated)?;
    if out.len() < need {
        return Err(Mcoded7Error::OutputTooShort {
            need,
            have: out.len(),
        });
    }
    for (group, data) in wire.chunks(GROUP + 1).zip(out[..need].chunks_mut(GROUP)) {
        if let Some((&sign, lows)) = group.split_first() {
            for (i, (&low, byte)) in lows.iter().zip(data).enumerate() {
                *byte = low | ((sign >> (GROUP - 1 - i)) & 1) << 7;
            }
        }
    }
    Ok(need)
}

/// [`encode`] into a new `Vec`.
#[must_use]
pub fn encode_vec(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0; encoded_len(data.len())];
    // The vector is exactly as long as the result.
    let _ = encode(data, &mut out);
    out
}

/// [`decode`] into a new `Vec`.
///
/// # Errors
///
/// [`Mcoded7Error::NotSevenBit`] or [`Mcoded7Error::Truncated`], as [`decode`].
pub fn decode_vec(wire: &[u8]) -> Result<Vec<u8>, Mcoded7Error> {
    let mut out = vec![0; decoded_len(wire.len()).ok_or(Mcoded7Error::Truncated)?];
    decode(wire, &mut out)?;
    Ok(out)
}
