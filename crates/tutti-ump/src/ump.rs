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

    /// The held-key fact of this packet, for an observer that must close what
    /// a stream opened: a note on or off by its `(protocol, group, channel,
    /// note)` key, or the sustain pedal (CC 64) of a channel going down or up.
    /// A MIDI 1.0 Note On at velocity 0 is an off, a MIDI 2.0 one an on, as in
    /// [`is_note_off`](Self::is_note_off).
    #[inline]
    #[must_use]
    pub const fn key_event(&self) -> Option<KeyEvent> {
        let w = self.0[0];
        let status = (w >> 20) & 0xf;
        let lane = ((w >> 24) & 0xf) as u8 * 16 + ((w >> 16) & 0xf) as u8;
        let m2 = w >> 28 == 0x4;
        match w >> 28 {
            0x2 | 0x4 => {}
            _ => return None,
        }
        let key = Key::new(m2, lane, ((w >> 8) & 0x7f) as u8);
        match status {
            0x8 => Some(KeyEvent::Off(key)),
            0x9 => {
                if !m2 && w & 0x7f == 0 {
                    Some(KeyEvent::Off(key))
                } else {
                    Some(KeyEvent::On(key))
                }
            }
            0xb if (w >> 8) & 0x7f == 64 => {
                let down = if m2 {
                    self.0[1] >= 0x8000_0000
                } else {
                    w & 0x7f >= 64
                };
                Some(KeyEvent::Sustain { m2, lane, down })
            }
            _ => None,
        }
    }

    /// The note-off that closes `key`, velocity 0 (M1: status 8, M2: status 8
    /// with a zero 16-bit velocity).
    #[must_use]
    pub const fn note_off(key: Key) -> Ump {
        let lane = key.lane() as u32;
        let (g, c) = (lane >> 4, lane & 0xf);
        let head = ((g << 24) | (0x8 << 20) | (c << 16)) | ((key.note() as u32) << 8);
        if key.is_m2() {
            Ump([0x4000_0000 | head, 0, 0, 0])
        } else {
            Ump([0x2000_0000 | head, 0, 0, 0])
        }
    }

    /// Sustain pedal up (CC 64 = 0) on `lane` (`group * 16 + channel`).
    #[must_use]
    pub const fn sustain_up(m2: bool, lane: u8) -> Ump {
        let (g, c) = ((lane >> 4) as u32, (lane & 0xf) as u32);
        let head = (g << 24) | (0xb << 20) | (c << 16) | (64 << 8);
        if m2 {
            Ump([0x4000_0000 | head, 0, 0, 0])
        } else {
            Ump([0x2000_0000 | head, 0, 0, 0])
        }
    }
}

/// A held-note key: protocol, group, channel and note (M2-104 identifies a
/// note by the last three), packed in 16 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(u16);

impl Key {
    /// The key of `note` on `lane` (`group * 16 + channel`).
    #[must_use]
    pub const fn new(m2: bool, lane: u8, note: u8) -> Self {
        Key(((m2 as u16) << 15) | ((lane as u16) << 7) | (note & 0x7f) as u16)
    }
    /// Packed form, `0..=65535`.
    #[must_use]
    pub const fn index(self) -> u16 {
        self.0
    }
    /// From the packed form.
    #[must_use]
    pub const fn from_index(i: u16) -> Self {
        Key(i)
    }
    /// Whether the key was opened by a MIDI 2.0 packet.
    #[must_use]
    pub const fn is_m2(self) -> bool {
        self.0 >> 15 != 0
    }
    /// `group * 16 + channel`.
    #[must_use]
    pub const fn lane(self) -> u8 {
        ((self.0 >> 7) & 0xff) as u8
    }
    /// The note number.
    #[must_use]
    pub const fn note(self) -> u8 {
        (self.0 & 0x7f) as u8
    }
}

/// What [`Ump::key_event`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    /// A note starts.
    On(Key),
    /// A note ends.
    Off(Key),
    /// The sustain pedal of a channel changes.
    Sustain {
        /// MIDI 2.0 packet.
        m2: bool,
        /// `group * 16 + channel`.
        lane: u8,
        /// Pedal down (value at least half).
        down: bool,
    },
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
