//! [`Ump`]: one Universal MIDI Packet, as the words it travels in.

use bytemuck::{Pod, Zeroable};

/// One Universal MIDI Packet: four 32-bit words, of which the message type in
/// the top nibble of the first word says how many are meaningful
/// ([`words`](Self::words)). Unused words are zero.
///
/// The type is storage. It names no message and checks no field. The one
/// message it reads is the note-off, because the graph's event queues must tell
/// one apart ([`is_note_off`](Self::is_note_off)).
///
/// `Pod`, so a block of packets crosses a process boundary as one copy.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Pod, Zeroable)]
pub struct Ump(pub [u32; 4]);

impl Ump {
    /// The meaningful words, 1 to 4 of them, by the message type (M2-104 Table
    /// 4): types `0x0` to `0x2`, `0x6` and `0x7` are 32 bits, `0x3`, `0x4`,
    /// `0x8` to `0xA` 64 bits, `0xB` and `0xC` 96 bits, and `0x5`, `0xD` to
    /// `0xF` 128 bits. A reserved type has its size too, so its words travel.
    #[inline]
    #[must_use]
    pub const fn words(&self) -> &[u32] {
        let n = match self.0[0] >> 28 {
            0x0..=0x2 | 0x6 | 0x7 => 1,
            0x3 | 0x4 | 0x8..=0xA => 2,
            0xB | 0xC => 3,
            _ => 4,
        };
        self.0.split_at(n).0
    }

    /// Whether this is a note-off: a MIDI 1.0 channel-voice (type `0x2`) Note
    /// Off, a MIDI 1.0 Note On at velocity 0 (its spelling of the same thing),
    /// or a MIDI 2.0 channel-voice (type `0x4`) Note Off.
    ///
    /// A MIDI 2.0 Note On at velocity 0 is a note-on (M2-104 §7.4.2).
    #[inline]
    #[must_use]
    pub const fn is_note_off(&self) -> bool {
        let w = self.0[0];
        let status = (w >> 20) & 0xf;
        match w >> 28 {
            0x2 => status == 0x8 || (status == 0x9 && w & 0x7f == 0),
            0x4 => status == 0x8,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutation: move `0xB | 0xC` into the 4-word default, or `0x6 | 0x7` into
    /// the 2-word arm -> the lengths fail.
    #[test]
    fn the_word_count_follows_the_message_type() {
        let want = [1, 1, 1, 2, 2, 4, 1, 1, 2, 2, 2, 3, 3, 4, 4, 4];
        for (mt, n) in want.into_iter().enumerate() {
            let u = Ump([((mt as u32) << 28) | 0x0123_4567, 1, 2, 3]);
            assert_eq!(u.words().len(), n, "message type {mt:#x}");
            assert_eq!(u.words(), &u.0[..n]);
        }
    }

    /// Mutation: drop the velocity-0 note-on arm, or let the type `0x4` arm
    /// accept status 0x9 -> fails.
    #[test]
    fn note_offs_are_recognised() {
        let m1 =
            |status: u32, vel: u32| Ump([0x2000_0000 | (status << 16) | (60 << 8) | vel, 0, 0, 0]);
        assert!(m1(0x80, 64).is_note_off());
        assert!(m1(0x90, 0).is_note_off());
        assert!(!m1(0x90, 64).is_note_off());
        assert!(!m1(0xa0, 0).is_note_off());
        assert!(Ump([0x4080_3c00, 0x8000_0000, 0, 0]).is_note_off());
        assert!(!Ump([0x4090_3c00, 0, 0, 0]).is_note_off());
        assert!(!Ump([0x4090_3c00, 0x8000_0000, 0, 0]).is_note_off());
        assert!(!Ump([0x1080_0000, 0, 0, 0]).is_note_off());
        assert!(!Ump([0; 4]).is_note_off());
    }

    #[test]
    fn it_is_four_words_and_castable() {
        assert_eq!(core::mem::size_of::<Ump>(), 16);
        let u = Ump([1, 2, 3, 4]);
        assert_eq!(bytemuck::bytes_of(&u).len(), 16);
        assert_eq!(bytemuck::cast::<Ump, [u32; 4]>(u), [1, 2, 3, 4]);
    }
}
