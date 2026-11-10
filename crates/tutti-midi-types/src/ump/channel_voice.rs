//! Channel Voice readers.
//!
//! A single Channel Voice message is built with `tutti_ump::Message` and
//! `From<Message> for Ump` (a `Voice2` for the MIDI 2.0 messages): there is no
//! constructor here. What stays is the one field read the translation and
//! decode code share.

/// The 14-bit bank (`MSB << 7 | LSB`) of a MIDI 2.0 Program Change's words
/// when its Bank Valid bit is set (M2-104 Figure 64).
#[inline]
pub(crate) fn program_change_bank_of(words: &[u32]) -> Option<u16> {
    let (w0, w1) = (*words.first()?, *words.get(1)?);
    if w0 & 1 == 0 {
        return None;
    }
    Some((((w1 >> 8) & 0x7F) << 7 | (w1 & 0x7F)) as u16)
}

#[cfg(test)]
mod tests;
