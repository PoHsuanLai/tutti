//! The MIDI Tuning Standard's values (MTS, "MIDI Tuning Updated Specification":
//! CA-020, CA-021, RP-020): the frequency data word, the key table and the
//! messages that carry them, as typed values.
//!
//! [`TuningMessage`] is what [`Universal::Tuning`](crate::Universal::Tuning)
//! holds. What the messages mean for a note is the instrument's
//! (`tutti-voicing` keeps the tables); this module is the wire read once.
//!
//! # The frequency data word
//!
//! "A 3-byte (21-bit) frequency data word": the first byte is "the nearest
//! equal-tempered semitone below the frequency", the next two bytes (14 bits,
//! most significant first) "the fraction of 100 cents above the semitone". So a
//! word is a pitch in units of `1/16384` semitone above MIDI note 0, and
//! [`TuningFrequency::pitch`] reads it as that many [`Semitones`]. `7F 7F 7F` "is
//! reserved to indicate not frequency data but a 'no change' condition": it is
//! [`TuningFrequency::NO_CHANGE`], a value of the same type, and has no pitch.

use tutti_types::{Cents, Semitones};
use tutti_ump::{MidiChannel, NoteNumber, U7, U14};

/// Steps of a [`TuningFrequency`] in a semitone: the 14-bit fraction.
const FRACTION_STEPS: u32 = 1 << 14;

/// One frequency data word of MTS: 21 bits, the semitone (7 bits) over the
/// fraction of a semitone (14 bits). See the [module docs](self).
///
/// `new` refuses a raw with a bit above the 21st; the raw is `semitone << 14 |
/// fraction`. [`NO_CHANGE`](Self::NO_CHANGE) (`7F 7F 7F`, the raw `0x1F_FFFF`)
/// is a value like any other; [`pitch`](Self::pitch) is `None` for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TuningFrequency(u32);

/// A raw refused by [`TuningFrequency::try_from`]: it has a bit above the 21st.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotFrequencyData(pub u32);

impl core::fmt::Display for NotFrequencyData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#x} is more than 21 bits", self.0)
    }
}

impl core::error::Error for NotFrequencyData {}

impl TuningFrequency {
    /// `7F 7F 7F`: "no change" (the instrument keeps what it has for the key).
    pub const NO_CHANGE: Self = Self(0x1F_FFFF);
    /// `7F 7F 7E`: "top of range", the highest frequency a word names.
    pub const TOP: Self = Self(0x1F_FFFE);

    /// The word with raw `raw`, or `None` above 21 bits.
    #[must_use]
    pub const fn new(raw: u32) -> Option<Self> {
        if raw <= Self::NO_CHANGE.0 {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// The word with raw `raw`; panics above 21 bits. For a literal.
    #[must_use]
    pub const fn of(raw: u32) -> Self {
        match Self::new(raw) {
            Some(word) => word,
            None => panic!("frequency data is 21 bits"),
        }
    }

    /// The raw: `semitone << 14 | fraction`.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The word of `semitone` and `fraction` of a semitone above it.
    #[must_use]
    pub const fn from_parts(semitone: U7, fraction: U14) -> Self {
        Self((semitone.get() as u32) << 14 | fraction.get() as u32)
    }

    /// The equal-tempered semitone below (the first byte).
    #[must_use]
    pub const fn semitone(self) -> U7 {
        U7::masked((self.0 >> 14) as u8)
    }

    /// The fraction of a semitone above it, in `1/16384`ths (the other two
    /// bytes, "0.0061 cents" each).
    #[must_use]
    pub const fn fraction(self) -> U14 {
        U14::masked((self.0 & 0x3FFF) as u16)
    }

    /// Equal temperament's word for `key`: the key itself, no fraction.
    #[must_use]
    pub const fn equal(key: NoteNumber) -> Self {
        Self((key.get() as u32) << 14)
    }

    /// Whether this is [`NO_CHANGE`](Self::NO_CHANGE).
    #[must_use]
    pub const fn is_no_change(self) -> bool {
        self.0 == Self::NO_CHANGE.0
    }

    /// The pitch the word names, in semitones above MIDI note 0 (note 69, `45
    /// 00 00`, is 69, A440 at master tuning's centre); `None` for "no change".
    #[must_use]
    pub fn pitch(self) -> Option<Semitones> {
        (!self.is_no_change()).then(|| Semitones(self.0 as f32 / FRACTION_STEPS as f32))
    }

    /// The word nearest `pitch` (semitones above note 0), saturating at note 0
    /// and at [`TOP`](Self::TOP); a NaN is note 0. Never
    /// [`NO_CHANGE`](Self::NO_CHANGE).
    #[must_use]
    pub fn nearest(pitch: Semitones) -> Self {
        let steps = (f64::from(pitch.get()) * f64::from(FRACTION_STEPS)).round();
        if steps.is_nan() || steps <= 0.0 {
            Self(0)
        } else if steps >= f64::from(Self::TOP.0) {
            Self::TOP
        } else {
            Self(steps as u32)
        }
    }
}

impl TryFrom<u32> for TuningFrequency {
    type Error = NotFrequencyData;

    fn try_from(raw: u32) -> Result<Self, NotFrequencyData> {
        Self::new(raw).ok_or(NotFrequencyData(raw))
    }
}

impl From<TuningFrequency> for u32 {
    fn from(word: TuningFrequency) -> u32 {
        word.0
    }
}

/// A tuning's frequency data for all 128 keys, "in order from note 0 [...] to
/// note 127": what a Bulk Tuning Dump holds and what a tuning program stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyTable([TuningFrequency; 128]);

impl KeyTable {
    /// Twelve-tone equal temperament: every key at its own number (RP-020:
    /// "the initial tuning of the instrument is equal temperament").
    #[must_use]
    pub const fn equal_temperament() -> Self {
        let mut words = [TuningFrequency(0); 128];
        let mut key = 0;
        while key < 128 {
            words[key] = TuningFrequency((key as u32) << 14);
            key += 1;
        }
        Self(words)
    }

    /// A table from its 128 words.
    #[must_use]
    pub const fn new(words: [TuningFrequency; 128]) -> Self {
        Self(words)
    }

    /// The word of `key`.
    #[must_use]
    pub const fn get(&self, key: NoteNumber) -> TuningFrequency {
        self.0[key.get() as usize]
    }

    /// Sets the word of `key`. "No change" leaves what is stored (the spec:
    /// "it should make no change to its stored frequency data for that MIDI key
    /// number").
    pub fn set(&mut self, key: NoteNumber, word: TuningFrequency) {
        if !word.is_no_change() {
            self.0[usize::from(key.get())] = word;
        }
    }

    /// The words, key 0 first.
    #[must_use]
    pub const fn words(&self) -> &[TuningFrequency; 128] {
        &self.0
    }
}

/// A tuning's name: "16 ASCII characters". A data byte is 7-bit before it gets
/// here, so every byte is ASCII.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TuningName([U7; 16]);

impl TuningName {
    /// A name of its sixteen bytes.
    #[must_use]
    pub const fn new(bytes: [U7; 16]) -> Self {
        Self(bytes)
    }

    /// The bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[U7; 16] {
        &self.0
    }

    /// The name's byte at `i` (`None` past 15).
    #[must_use]
    pub fn byte(&self, i: usize) -> Option<u8> {
        self.0.get(i).map(|b| b.get())
    }
}

/// When a tuning change takes hold of a note that sounds: the real-time
/// messages "WILL affect currently sounding notes", the non-real-time ones
/// "will NOT update the currently sounding notes" (CA-020).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timing {
    /// Universal Real Time (`7F`): sounding notes follow at once.
    RealTime,
    /// Universal Non-Real Time (`7E`): a setup message; sounding notes are
    /// left, the next Note On has it.
    NonRealTime,
}

/// One change of a Single Note Tuning Change: a key and its new word
/// (`kk xx yy zz`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyChange {
    /// The key.
    pub key: NoteNumber,
    /// What it is tuned to (or "no change").
    pub frequency: TuningFrequency,
}

/// The most changes one message holds: `ll` is a data byte.
pub const MAX_KEY_CHANGES: usize = 127;

/// The changes of one Single Note Tuning Change, in message order ("the number
/// of changes following is indicated by the byte `ll`", at most 127). Fixed
/// size, so reading one allocates nothing.
#[derive(Clone, Copy, Debug)]
pub struct KeyChanges {
    len: u8,
    items: [KeyChange; MAX_KEY_CHANGES],
}

impl KeyChanges {
    const BLANK: KeyChange = KeyChange {
        key: NoteNumber::of(0),
        frequency: TuningFrequency::NO_CHANGE,
    };

    /// No changes.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            len: 0,
            items: [Self::BLANK; MAX_KEY_CHANGES],
        }
    }

    /// Adds a change; whether there was room (127).
    pub fn push(&mut self, change: KeyChange) -> bool {
        let Some(slot) = self.items.get_mut(usize::from(self.len)) else {
            return false;
        };
        *slot = change;
        self.len += 1;
        true
    }

    /// The changes, in order.
    #[must_use]
    pub fn as_slice(&self) -> &[KeyChange] {
        &self.items[..usize::from(self.len)]
    }
}

impl Default for KeyChanges {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for KeyChanges {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for KeyChanges {}

/// The channels a Scale/Octave Tuning message names: "a 3 byte channel field
/// [...] with 1 bit representing each MIDI channel". Its range is the whole of
/// `u16`, so it has no refusing constructor; bit `n` is channel `n` (0-based).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChannelMask(u16);

impl ChannelMask {
    /// No channel.
    pub const NONE: Self = Self(0);
    /// All sixteen.
    pub const ALL: Self = Self(0xFFFF);

    /// The mask with bit `n` for channel `n`.
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// The bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Whether `channel` is named.
    #[must_use]
    pub const fn contains(self, channel: MidiChannel) -> bool {
        self.0 >> channel.get() & 1 == 1
    }

    /// The channels named, lowest first.
    pub fn channels(self) -> impl Iterator<Item = MidiChannel> {
        (0..16u8)
            .filter(move |&i| self.0 >> i & 1 == 1)
            .filter_map(MidiChannel::new)
    }
}

/// The twelve offsets of a Scale/Octave Tuning message, C to B, in the form the
/// wire gave them (CA-021: "calibrating a single octave of notes in small
/// fractions of an equal-tempered half-step").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OctaveOffsets {
    /// The 1-byte form: `00` is -64 cents, `40` is 0 and `7F` is +63, a cent a
    /// step.
    Cents([U7; 12]),
    /// The 2-byte form (the first byte the most significant): `00 00` is -100
    /// cents, `40 00` is 0, `7F 7F` is +100 ("200 cents divided by 16,384 =
    /// .012207 cents").
    Fine([U14; 12]),
}

impl OctaveOffsets {
    /// No offset: equal temperament, in the 1-byte form.
    pub const ZERO: Self = Self::Cents([U7::of(0x40); 12]);

    /// The offset of pitch class `class` (0 is C), in cents.
    #[must_use]
    pub fn offset(&self, class: usize) -> Cents {
        match self {
            Self::Cents(bytes) => Cents(f32::from(bytes[class % 12].get()) - 64.0),
            Self::Fine(words) => {
                Cents((f32::from(words[class % 12].get()) - 8192.0) * 200.0 / FRACTION_STEPS as f32)
            }
        }
    }
}

/// A MIDI Tuning Standard message (sub-ID#1 `08`).
///
/// A message that has no bank byte on the wire (the original Bulk Tuning Dump,
/// Request and Single Note Tuning Change) has `bank: None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuningMessage {
    /// Bulk Tuning Dump Request (`08 00`) or its bank form (`08 03`): "the
    /// receiving instrument shall respond by sending the bulk tuning dump".
    DumpRequest {
        /// The bank, if the message names one.
        bank: Option<U7>,
        /// The tuning program.
        program: U7,
    },
    /// Bulk Tuning Dump (`08 01`) or Key-Based Tuning Dump (`08 04`): all 128
    /// keys.
    KeyDump {
        /// The bank, if the message names one (`08 04`).
        bank: Option<U7>,
        /// The tuning program.
        program: U7,
        /// The tuning's name.
        name: TuningName,
        /// Its 128 words.
        table: KeyTable,
    },
    /// Scale/Octave Tuning Dump, 1 byte (`08 05`) or 2 byte (`08 06`).
    OctaveDump {
        /// The bank.
        bank: U7,
        /// The tuning program.
        program: U7,
        /// The tuning's name.
        name: TuningName,
        /// The twelve offsets.
        offsets: OctaveOffsets,
    },
    /// Single Note Tuning Change: real time (`08 02`, `08 07`) or non-real
    /// time (`08 07`).
    NoteChange {
        /// Whether sounding notes follow.
        timing: Timing,
        /// The bank, if the message names one (`08 07`).
        bank: Option<U7>,
        /// The tuning program.
        program: U7,
        /// The keys and their words.
        changes: KeyChanges,
    },
    /// Scale/Octave Tuning, 1 byte (`08 08`) or 2 byte (`08 09`), real time or
    /// non-real time: offsets for the channels `channels` names.
    OctaveTuning {
        /// Whether sounding notes follow.
        timing: Timing,
        /// The channels it is for.
        channels: ChannelMask,
        /// The twelve offsets.
        offsets: OctaveOffsets,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec's examples of frequency data: `3C 00 00` is middle C, note 60,
    /// `45 00 00` is A440, `44 7F 7F` is 439.9984 Hz (one step under it).
    #[test]
    fn a_word_is_a_pitch_in_sixteen_thousandths_of_a_semitone() {
        let a = TuningFrequency::from_parts(U7::of(0x45), U14::of(0));
        assert_eq!(a.pitch(), Some(Semitones(69.0)));
        let under = TuningFrequency::from_parts(U7::of(0x44), U14::of(0x3FFF));
        assert_eq!(under.get() + 1, a.get());
        let step = a.pitch().unwrap().get() - under.pitch().unwrap().get();
        // 100 cents over 2^14 steps: 0.0061 cents.
        assert!((step * 100.0 - 100.0 / 16384.0).abs() < 1e-4, "{step}");
    }

    /// `7F 7F 7F` is "no change" and has no pitch; `7F 7F 7E` is the top.
    #[test]
    fn no_change_has_no_pitch() {
        assert!(TuningFrequency::NO_CHANGE.pitch().is_none());
        assert!(TuningFrequency::TOP.pitch().is_some());
        assert_eq!(TuningFrequency::NO_CHANGE.semitone(), U7::of(0x7F));
        assert_eq!(TuningFrequency::NO_CHANGE.fraction(), U14::MAX);
    }

    /// A "no change" word stored in a table leaves the key as it was.
    #[test]
    fn setting_no_change_keeps_the_stored_word() {
        let mut table = KeyTable::equal_temperament();
        let key = NoteNumber::of(61);
        table.set(key, TuningFrequency::NO_CHANGE);
        assert_eq!(table.get(key), TuningFrequency::equal(key));
        table.set(key, TuningFrequency::of(0x10_0001));
        assert_eq!(table.get(key).get(), 0x10_0001);
    }

    /// `nearest` never answers "no change" and a NaN is note 0.
    #[test]
    fn nearest_saturates_and_never_says_no_change() {
        assert_eq!(
            TuningFrequency::nearest(Semitones(1000.0)),
            TuningFrequency::TOP
        );
        assert_eq!(TuningFrequency::nearest(Semitones(-3.0)).get(), 0);
        assert_eq!(TuningFrequency::nearest(Semitones(f32::NAN)).get(), 0);
        assert_eq!(
            TuningFrequency::nearest(Semitones(69.0)),
            TuningFrequency::equal(NoteNumber::of(69))
        );
    }

    /// The range contract: 21 bits.
    #[test]
    fn a_raw_above_21_bits_is_refused() {
        assert!(TuningFrequency::new(0x1F_FFFF).is_some());
        assert!(TuningFrequency::new(0x20_0000).is_none());
        assert_eq!(
            TuningFrequency::try_from(0x20_0000),
            Err(NotFrequencyData(0x20_0000))
        );
        assert_eq!(u32::from(TuningFrequency::of(5)), 5);
    }

    /// CA-021: 1 byte `00` is -64 cents, `40` is 0, `7F` is +63; 2 byte `00 00`
    /// is -100, `40 00` is 0, `7F 7F` is about +100 (8191 steps of 0.012207).
    #[test]
    fn octave_offsets_follow_the_stated_scales() {
        let one = OctaveOffsets::Cents([0, 0x40, 0x7F, 0, 0, 0, 0, 0, 0, 0, 0, 0].map(U7::of));
        assert_eq!(one.offset(0), Cents(-64.0));
        assert_eq!(one.offset(1), Cents(0.0));
        assert_eq!(one.offset(2), Cents(63.0));
        let two = OctaveOffsets::Fine([0, 0x2000, 0x3FFF, 0, 0, 0, 0, 0, 0, 0, 0, 0].map(U14::of));
        assert_eq!(two.offset(0), Cents(-100.0));
        assert_eq!(two.offset(1), Cents(0.0));
        assert!((two.offset(2).get() - 8191.0 * 0.012207).abs() < 1e-2);
    }

    /// The channel mask: bit n is channel n.
    #[test]
    fn a_channel_mask_names_the_channels_of_its_bits() {
        let mask = ChannelMask::from_bits(0b1000_0000_0000_0101);
        let named: Vec<u8> = mask.channels().map(MidiChannel::get).collect();
        assert_eq!(named, [0, 2, 15]);
        assert!(mask.contains(MidiChannel::of(15)));
        assert!(!mask.contains(MidiChannel::of(1)));
    }
}
