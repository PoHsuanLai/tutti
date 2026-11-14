//! What the stream tests share.

/// `len` 7-bit bytes counting up from 0 and wrapping at 128: valid SysEx7 data of any
/// length.
pub fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 128) as u8).collect()
}
