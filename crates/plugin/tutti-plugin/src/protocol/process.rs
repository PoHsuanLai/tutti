//! Boxed bulk payload for the `ProcessAudio` wire message.

use serde::{Deserialize, Serialize};

use super::midi::IpcMidiEventVec;
use super::{
    ChordChanges, NoteExpressionChanges, NoteExpressionIntChanges, NoteExpressionTextChanges,
    ParameterChanges, ScaleChanges, TransportInfo,
};

/// One process block's inputs, except the audio itself.
///
/// Carries which block this is, the sample count, and all the per-block
/// side-band (MIDI, automation, note expression, VST3 sequencer context,
/// transport). Audio travels in the shared `AudioSlab`; everything here rides
/// the control socket alongside it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessAudioData {
    /// Which block this is. Monotonic per plugin instance from 1, and the index
    /// into the slab's ring: the server reads `seq`'s input slot and publishes
    /// into `seq`'s output slot, so both sides address the shared region by this
    /// number alone. `u64` is wide enough that it never wraps, so a reply can
    /// never be matched to the wrong block.
    pub seq: u64,
    /// Block length in **frames**, not samples — the slab's stride carries the
    /// channel count.
    pub num_samples: usize,
    /// MIDI to deliver at the top of the block: at most
    /// `MIDI_STACK_CAPACITY` from the host, and the server keeps at most that
    /// many of a longer one.
    pub midi_events: IpcMidiEventVec,
    /// Sample-accurate automation points to apply across the block.
    pub param_changes: ParameterChanges,
    /// Per-note expression (pitch, pressure, brightness) for the block.
    pub note_expression: NoteExpressionChanges,
    /// VST3 sequencer-context chords. VST3-only; other formats ignore it.
    pub chords: ChordChanges,
    /// VST3 sequencer-context scales. VST3-only; other formats ignore it.
    pub scales: ScaleChanges,
    /// VST3 per-note text expression. VST3-only; other formats ignore it.
    pub expr_texts: NoteExpressionTextChanges,
    /// VST3 per-note integer expression. VST3-only; other formats ignore it.
    pub expr_ints: NoteExpressionIntChanges,
    /// Tempo, time signature and play state for the block.
    pub transport: TransportInfo,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block behind [`GOLDEN_BLOCK`]: every list populated, MIDI,
    /// automation on two parameters, note expression (a named and a custom
    /// dimension), a chord, a scale, a note-expression text and an integer
    /// expression.
    fn golden_block() -> ProcessAudioData {
        use crate::protocol::{
            ChordValue, IpcMidiEvent, NoteExpressionIntValue, NoteExpressionTextValue,
            NoteExpressionType, NoteExpressionValue, ParamAddress, ScaleValue,
        };
        let mut param_changes = ParameterChanges::reserved(2, 2);
        param_changes.add_change(ParamAddress::Index(3), 0, 0.25);
        param_changes.add_change(ParamAddress::Index(3), 32, 0.75);
        param_changes.add_change(ParamAddress::Opaque(0xDEAD_BEEFu32.into()), 16, 1.0);
        let mut note_expression = NoteExpressionChanges::new();
        note_expression.add_change(NoteExpressionValue {
            sample_offset: 4,
            note_id: 7,
            expression_type: NoteExpressionType::Tuning,
            value: 0.5,
        });
        note_expression.add_change(NoteExpressionValue {
            sample_offset: 9,
            note_id: 8,
            expression_type: NoteExpressionType::Custom(0x1234),
            value: 0.125,
        });
        let mut chords = ChordChanges::new();
        let mut scales = ScaleChanges::new();
        let mut expr_texts = NoteExpressionTextChanges::new();
        let mut expr_ints = NoteExpressionIntChanges::new();
        let kept = [
            chords.add_change(ChordValue {
                sample_offset: 0,
                root: 60,
                bass_note: 48,
                mask: 0b1001_0001,
                text: "Cmaj".into(),
            }),
            scales.add_change(ScaleValue {
                sample_offset: 8,
                root: 2,
                mask: 0x0AB5,
                text: "D major".into(),
            }),
            expr_texts.add_change(NoteExpressionTextValue {
                sample_offset: 12,
                note_id: 7,
                type_id: 5,
                text: "ah".into(),
            }),
            expr_ints.add_change(NoteExpressionIntValue {
                sample_offset: 20,
                note_id: 7,
                type_id: 6,
                value: -1234,
            }),
        ];
        assert!(kept.iter().all(|k| *k), "each container has room");
        ProcessAudioData {
            seq: 42,
            num_samples: 64,
            midi_events: [
                IpcMidiEvent {
                    frame_offset: 0,
                    data: [0x2090_3C40, 0, 0, 0],
                },
                IpcMidiEvent {
                    frame_offset: 63,
                    data: [0x2080_3C00, 0, 0, 0],
                },
            ]
            .into_iter()
            .collect(),
            param_changes,
            note_expression,
            chords,
            scales,
            expr_texts,
            expr_ints,
            transport: TransportInfo {
                sample_rate: 48_000.0,
                ..Default::default()
            },
        }
    }

    /// `golden_block()` encoded by origin/main at `f07984d6`, the code before
    /// `smallvec` was removed (MIDI, parameter points, note expression and
    /// the harmony containers were `SmallVec`s then). Recorded by running
    /// the same fixture there.
    const GOLDEN_BLOCK: [u8; 427] = [
        0x2a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x3c,
        0x90, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3f,
        0x00, 0x00, 0x00, 0x00, 0x3c, 0x80, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x03, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xd0, 0x3f, 0x20, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0xe8, 0x3f, 0x00, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0xf0, 0x3f, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00,
        0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xe0, 0x3f, 0x09, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00,
        0x34, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x3f, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3c, 0x00, 0x30, 0x00, 0x91, 0x00,
        0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x43, 0x6d, 0x61, 0x6a, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x02, 0x00, 0xb5, 0x0a, 0x07, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x44, 0x20, 0x6d, 0x61, 0x6a, 0x6f, 0x72, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x05,
        0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x61, 0x68, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x06,
        0x00, 0x00, 0x00, 0x2e, 0xfb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x5e, 0x40, 0x04, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x70, 0xe7, 0x40,
    ];

    /// **A fully populated block encodes to the bytes the code before this
    /// change produced, and decodes from them.** Recorded from the old code,
    /// not restated from the layout: a host and a server built on either
    /// side of the change exchange the same `ProcessAudio` body, note
    /// expressions and texts included. `ProcessAudioData` has no
    /// `PartialEq`, so the decode is checked by re-encoding and by reading
    /// back the fields the change touched.
    ///
    /// Mutation (run): `chords` and `scales` swapped in `ProcessAudioData`'s
    /// declaration → the encoding differs → fails.
    #[test]
    fn a_block_encodes_as_before_smallvec_was_removed() {
        assert_eq!(bincode::serialize(&golden_block()).unwrap(), GOLDEN_BLOCK);

        let back: ProcessAudioData = bincode::deserialize(&GOLDEN_BLOCK).unwrap();
        assert_eq!(bincode::serialize(&back).unwrap(), GOLDEN_BLOCK);
        assert_eq!(back.midi_events.len(), 2);
        assert_eq!(back.midi_events[1].frame_offset, 63);
        assert_eq!(back.param_changes.queues.len(), 2);
        assert_eq!(back.param_changes.queues[0].points.len(), 2);
        assert_eq!(back.note_expression.changes.len(), 2);
        assert_eq!(
            back.note_expression.changes[1].expression_type,
            crate::protocol::NoteExpressionType::Custom(0x1234)
        );
        assert_eq!(back.chords.changes[0].text, "Cmaj");
        assert_eq!(back.scales.changes[0].text, "D major");
        assert_eq!(back.expr_texts.changes[0].text, "ah");
        assert_eq!(back.expr_ints.changes[0].value, -1234);
    }
}
