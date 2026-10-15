//! Harmony / sequencer-context vocabulary: chord, scale, per-note text, and
//! integer-valued per-note expression.
//!
//! These are inputs the host *sequencer* supplies (a chord track, a key/scale
//! lane, per-note annotations), not derived from MIDI. Only the VST3 host
//! consumes them; other formats ignore them. Text is an owned UTF-8 `String`
//! here; the VST3 boundary converts it to UTF-16.
//!
//! The `Serialize`/`Deserialize` impls are gated behind the `serde`
//! feature.
//!
//! # Each block's list is capped
//!
//! A block carries at most [`HARMONY_CAPACITY`] events of each kind, inline:
//! the containers are filled on the audio thread (the plugin node's chunk
//! walk) and moved by value to the bridge, so they never touch the heap, and
//! past the cap an event is dropped ([`add_change`](ChordChanges::add_change)
//! returns `false`). A sequencer moves the chord or the key a few times a
//! bar, so four in one block is already far more than a session produces.
//!
//! # Decoding truncates
//!
//! On the wire a container is a length and its events, as any sequence is.
//! A peer may send more than the cap (nothing on the wire bounds it), and the
//! decoder **keeps the first [`HARMONY_CAPACITY`] and drops the rest**, rather
//! than failing the message: the same answer the producer gives past the cap,
//! and a failed decode would lose the whole block it rides in (its MIDI,
//! automation and transport) over one excess chord.

use arrayvec::ArrayVec;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The most events of one kind (chords, scales, per-note texts, per-note
/// integer expressions) one block carries. Past it, events are dropped.
pub const HARMONY_CAPACITY: usize = 4;

/// A per-note text annotation (lyric, ornament, …) bound to a `note_id`.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NoteExpressionTextValue {
    /// Frame offset from the start of the process block.
    pub sample_offset: i32,
    /// The note this annotates, as a host note id — see
    /// [`note_id_for`](crate::note_id_for).
    pub note_id: i32,
    /// Which text dimension this is, in the format's own numbering. VST3 passes
    /// it through to the plugin verbatim; nothing here interprets it.
    pub type_id: u32,
    /// The annotation, owned UTF-8. The VST3 boundary converts it to UTF-16.
    pub text: String,
}

/// An integer-valued per-note expression event, for stepped or enumerated
/// dimensions.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NoteExpressionIntValue {
    /// Frame offset from the start of the process block.
    pub sample_offset: i32,
    /// The note this drives, as a host note id — see
    /// [`note_id_for`](crate::note_id_for).
    pub note_id: i32,
    /// Which stepped dimension this is, in the format's own numbering. Passed
    /// through to the plugin verbatim; nothing here interprets it.
    pub type_id: u32,
    /// The step, in whatever space `type_id` names. Not normalized — the
    /// stepped dimensions are exactly the ones a `0..=1` scale cannot carry.
    pub value: i64,
}

/// A chord-context event: root and bass note, a degree `mask`, and a display
/// name.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ChordValue {
    /// Frame offset from the start of the process block, at which the chord
    /// takes effect.
    pub sample_offset: i32,
    /// Root pitch as a MIDI note number, `0..=127`.
    pub root: i16,
    /// Bass pitch as a MIDI note number, `0..=127`. Equal to `root` for a chord
    /// in root position; different for a slash chord.
    pub bass_note: i16,
    /// The chord's degrees as a 12-bit mask relative to `root`, bit `n` set when
    /// semitone `n` sounds.
    pub mask: i16,
    /// Display name (`"Cmaj7"`), owned UTF-8. Presentation only — the degrees a
    /// plugin acts on are in `mask`. The VST3 boundary converts it to UTF-16.
    pub text: String,
}

/// A scale/key-context event: root, a 12-bit degree `mask`, and a display
/// name.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ScaleValue {
    /// Frame offset from the start of the process block, at which the scale
    /// takes effect.
    pub sample_offset: i32,
    /// Tonic pitch as a MIDI note number, `0..=127`.
    pub root: i16,
    /// The scale's degrees as a 12-bit mask relative to `root`, bit `n` set when
    /// semitone `n` is in the scale.
    pub mask: i16,
    /// Display name (`"D Dorian"`), owned UTF-8. Presentation only — the degrees
    /// a plugin acts on are in `mask`. The VST3 boundary converts it to UTF-16.
    pub text: String,
}

macro_rules! changes_container {
    ($(#[$m:meta])* $name:ident, $item:ty, $field:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Default)]
        #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
        pub struct $name {
            /// The block's events, in the order the sequencer appended them:
            /// at most [`HARMONY_CAPACITY`], inline. `push` panics past the
            /// cap; write through `add_change`, which refuses.
            #[cfg_attr(feature = "serde", serde(deserialize_with = "truncating"))]
            pub $field: ArrayVec<$item, HARMONY_CAPACITY>,
        }

        impl $name {
            /// Builds an empty container.
            pub fn new() -> Self {
                Self::default()
            }

            /// Appends one event, or drops it and returns `false` when the
            /// block already holds [`HARMONY_CAPACITY`]. Allocation-free.
            ///
            /// Order is the caller's to maintain; nothing here sorts by
            /// `sample_offset`.
            #[must_use = "`false` means the event was dropped"]
            pub fn add_change(&mut self, change: $item) -> bool {
                self.$field.try_push(change).is_ok()
            }

            /// Returns `true` when the block carries no event of this kind.
            pub fn is_empty(&self) -> bool {
                self.$field.is_empty()
            }
        }
    };
}

changes_container!(
    /// Per-block chord events.
    ChordChanges, ChordValue, changes
);
changes_container!(
    /// Per-block scale events.
    ScaleChanges, ScaleValue, changes
);
changes_container!(
    /// Per-block per-note text events.
    NoteExpressionTextChanges, NoteExpressionTextValue, changes
);
changes_container!(
    /// Per-block integer per-note expression events.
    NoteExpressionIntChanges, NoteExpressionIntValue, changes
);

/// Decodes a sequence into at most `N` items, keeping the first `N` and
/// reading past the rest (module docs, "Decoding truncates").
///
/// Every item is still read: the excess is decoded and dropped rather than
/// left on the stream, so a length-prefixed format (bincode) stays in step
/// with the fields after this one.
#[cfg(feature = "serde")]
fn truncating<'de, D, T, const N: usize>(d: D) -> Result<ArrayVec<T, N>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visit<T, const N: usize>(core::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const N: usize> serde::de::Visitor<'de> for Visit<T, N> {
        type Value = ArrayVec<T, N>;
        fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("a sequence")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = ArrayVec::new();
            while let Some(item) = seq.next_element::<T>()? {
                // Past the cap the item is dropped: decoded, so the stream
                // moves past it, and not kept.
                let _ = out.try_push(item);
            }
            Ok(out)
        }
    }
    d.deserialize_seq(Visit::<T, N>(core::marker::PhantomData))
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    fn chord(root: i16) -> ChordValue {
        ChordValue {
            sample_offset: i32::from(root),
            root,
            bass_note: root,
            mask: 0b1001_0001,
            text: format!("chord {root}"),
        }
    }

    /// **The wire format is a sequence, unchanged**: a container encodes
    /// byte for byte as a length and its events, which is what the inline
    /// list it replaced encoded as, so a peer built before the change reads
    /// it.
    ///
    /// No mutation of the container type changes these bytes while keeping a
    /// list (every serde sequence encodes so); the comparison is pinned
    /// against the explicit layout instead. Mutation (run): the expected
    /// bytes without the length prefix → fails.
    #[test]
    fn a_container_encodes_as_a_sequence() {
        let mut c = ChordChanges::new();
        assert!(c.add_change(chord(60)));
        assert!(c.add_change(chord(62)));
        let mut expected = (c.changes.len() as u64).to_le_bytes().to_vec();
        for item in &c.changes {
            expected.extend(bincode::serialize(item).expect("serialize"));
        }
        assert_eq!(bincode::serialize(&c).expect("serialize"), expected);
    }

    /// **A container past the cap from the wire keeps its first four, and
    /// the stream stays in step**: the field after it decodes whole.
    ///
    /// Mutations (run): `deserialize_with` removed (the `ArrayVec` default,
    /// which rejects) → the decode fails → fails; the visitor stops reading at
    /// the cap (`if out.is_full() { break }`) → the trailing field reads the
    /// leftover events' bytes → fails.
    #[test]
    fn decoding_past_the_cap_keeps_the_first_and_stays_in_step() {
        #[derive(serde::Serialize)]
        struct Sent {
            changes: Vec<ChordValue>,
            after: u64,
        }
        #[derive(serde::Deserialize)]
        struct Got {
            chords: ChordChanges,
            after: u64,
        }
        let sent = Sent {
            changes: (60..67).map(chord).collect(),
            after: 0xDEAD_BEEF,
        };
        let bytes = bincode::serialize(&sent).expect("serialize");
        let got: Got = bincode::deserialize(&bytes).expect("a long list decodes");
        let roots: Vec<i16> = got.chords.changes.iter().map(|c| c.root).collect();
        assert_eq!(roots, [60, 61, 62, 63], "the first four, in order");
        assert_eq!(got.chords.changes[3].text, "chord 63");
        assert_eq!(got.after, 0xDEAD_BEEF, "the next field is read whole");
    }

    /// **`add_change` refuses past the cap**, never panics.
    ///
    /// Mutation (run): `add_change` through `push` → the fifth panics →
    /// fails.
    #[test]
    fn add_change_refuses_past_the_cap() {
        let mut c = ScaleChanges::new();
        for root in 0..HARMONY_CAPACITY as i16 {
            assert!(c.add_change(ScaleValue {
                sample_offset: 0,
                root,
                mask: 0,
                text: String::new(),
            }));
        }
        assert!(!c.add_change(ScaleValue {
            sample_offset: 0,
            root: 99,
            mask: 0,
            text: String::new(),
        }));
        assert_eq!(c.changes.len(), HARMONY_CAPACITY);
        assert_eq!(c.changes[HARMONY_CAPACITY - 1].root, 3, "the last kept");
    }
}
