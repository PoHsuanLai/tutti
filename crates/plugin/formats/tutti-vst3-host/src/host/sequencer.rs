//! The sequencer context of a block (chords, scales, per-note texts and
//! integer note expressions) in the form VST3's event list takes.
//!
//! The Processor converts a block's [`ExpressiveContext`] into these buffers
//! and stages them as [`Vst3InputEvents`](crate::types::Vst3InputEvents).

use tutti_plugin_types::{
    ChordChanges, NoteExpressionIntChanges, NoteExpressionTextChanges, ScaleChanges,
};

/// Reusable buffers for the sequencer-context conversion in [`process_block`].
///
/// Every field is a `Vec` the converters `clear()` and refill rather than
/// rebuild, so a plugin advertising [`Features::SEQUENCER_CONTEXT`] costs no
/// allocation per block once the buffers have grown. The nested `text` buffers
/// matter as much as the outer ones: each chord / scale / note-expression text
/// is UTF-16 re-encoded, so building these fresh was one heap allocation *per
/// event*, not per block.
///
/// Sized by use, never by `set_block_size` — the event count is a property of
/// the score, not of the buffer length, so there is no maximum to preallocate
/// against. Steady state is reached after the first block that carries each
/// kind of event.
#[derive(Default)]
pub(super) struct SeqScratch {
    pub(super) chords: Vec<crate::types::ChordValue>,
    pub(super) scales: Vec<crate::types::ScaleValue>,
    pub(super) expr_texts: Vec<crate::types::NoteExpressionText>,
    pub(super) expr_ints: Vec<crate::types::NoteExpressionIntValue>,
}

/// Refill `out` from `chords`, reusing both the outer buffer and each entry's
/// UTF-16 `text` allocation.
///
/// The overwrite-in-place / truncate / push shape is what keeps the nested
/// `text` buffers alive: overwriting entry `i` re-encodes into the `Vec<u16>`
/// already sitting there, so a steady stream of chord events allocates nothing
/// after the first block. `clear()` + `push` would drop every `text` buffer and
/// re-grow it, which is the per-event allocation this exists to remove.
///
/// The same three-part shape appears in the two siblings below; the types have
/// no common trait to hoist it onto, and a macro would hide four short bodies
/// behind an indirection worth less than it costs.
pub(super) fn convert_chords_into(chords: &ChordChanges, out: &mut Vec<crate::types::ChordValue>) {
    let src = &chords.changes;
    for (dst, c) in out.iter_mut().zip(src.iter()) {
        dst.sample_offset = c.sample_offset;
        dst.root = c.root;
        dst.bass_note = c.bass_note;
        dst.mask = c.mask;
        encode_utf16_into(&c.text, &mut dst.text);
    }
    out.truncate(src.len());
    for c in src.iter().skip(out.len()) {
        out.push(crate::types::ChordValue {
            sample_offset: c.sample_offset,
            root: c.root,
            bass_note: c.bass_note,
            mask: c.mask,
            text: c.text.encode_utf16().collect(),
        });
    }
}

/// Re-encode `src` into `dst` in place, keeping `dst`'s capacity.
fn encode_utf16_into(src: &str, dst: &mut Vec<u16>) {
    dst.clear();
    dst.extend(src.encode_utf16());
}

/// Refill `out` from `scales`. Same buffer-reuse shape as
/// [`convert_chords_into`].
pub(super) fn convert_scales_into(scales: &ScaleChanges, out: &mut Vec<crate::types::ScaleValue>) {
    let src = &scales.changes;
    for (dst, s) in out.iter_mut().zip(src.iter()) {
        dst.sample_offset = s.sample_offset;
        dst.root = s.root;
        dst.mask = s.mask;
        encode_utf16_into(&s.text, &mut dst.text);
    }
    out.truncate(src.len());
    for s in src.iter().skip(out.len()) {
        out.push(crate::types::ScaleValue {
            sample_offset: s.sample_offset,
            root: s.root,
            mask: s.mask,
            text: s.text.encode_utf16().collect(),
        });
    }
}

/// Refill `out` from `texts`. Same buffer-reuse shape as
/// [`convert_chords_into`].
pub(super) fn convert_expr_texts_into(
    texts: &NoteExpressionTextChanges,
    out: &mut Vec<crate::types::NoteExpressionText>,
) {
    let src = &texts.changes;
    for (dst, t) in out.iter_mut().zip(src.iter()) {
        dst.sample_offset = t.sample_offset;
        dst.note_id = t.note_id;
        dst.type_id = t.type_id;
        encode_utf16_into(&t.text, &mut dst.text);
    }
    out.truncate(src.len());
    for t in src.iter().skip(out.len()) {
        out.push(crate::types::NoteExpressionText {
            sample_offset: t.sample_offset,
            note_id: t.note_id,
            type_id: t.type_id,
            text: t.text.encode_utf16().collect(),
        });
    }
}

/// Refill `out` from `ints`.
///
/// `NoteExpressionIntValue` is `Copy` with no nested buffer, so this is the
/// plain `clear()` + `extend` the other three cannot use — there is nothing per
/// entry to preserve, only the outer capacity.
pub(super) fn convert_expr_ints_into(
    ints: &NoteExpressionIntChanges,
    out: &mut Vec<crate::types::NoteExpressionIntValue>,
) {
    out.clear();
    out.extend(
        ints.changes
            .iter()
            .map(|i| crate::types::NoteExpressionIntValue {
                sample_offset: i.sample_offset,
                note_id: i.note_id,
                type_id: i.type_id,
                value: i.value,
            }),
    );
}

/// Sequencer-context conversion, exercised without a plugin binary: the
/// converters are pure functions over the protocol types.
#[cfg(test)]
mod seq_scratch_tests {
    use super::*;
    use tutti_plugin_types::ChordValue;

    fn chord(offset: i32, text: &str) -> ChordValue {
        ChordValue {
            sample_offset: offset,
            root: 60,
            bass_note: 60,
            mask: 0b1001_0001,
            text: text.to_string(),
        }
    }

    fn changes(items: &[ChordValue]) -> ChordChanges {
        let mut c = ChordChanges::new();
        for i in items {
            assert!(c.add_change(i.clone()), "the fixture fits the cap");
        }
        c
    }

    /// The point of the whole change: once the buffers have grown, refilling
    /// them must not allocate.
    ///
    /// Measured with `assert_no_alloc` rather than by comparing buffer
    /// addresses. Addresses do **not** discriminate here — `clear()` frees each
    /// `text` buffer and the very next `push` re-requests the same size, which
    /// the allocator typically satisfies from the block it just freed, so the
    /// pointers come back equal and a naive rebuild passes. Counting the
    /// allocations is the only observable that separates the two, and this
    /// crate already installs `AllocDisabler` as its global allocator
    /// (`lib.rs`) for exactly this kind of gate.
    ///
    /// The source `ChordChanges` is built outside the gate: constructing one
    /// allocates its `String`s, which is the caller's cost, not the
    /// converter's.
    #[test]
    fn refilling_grown_buffers_does_not_allocate() {
        let mut out = Vec::new();
        // Warm up: first pass grows the outer Vec and each nested text buffer.
        convert_chords_into(&changes(&[chord(0, "Cmaj7"), chord(64, "Fmin")]), &mut out);

        // Same event count, same text lengths, different values — the steady
        // state a sequencer-driven plugin sits in block after block.
        let next = changes(&[chord(8, "Dmin9"), chord(96, "G7")]);
        assert_no_alloc::assert_no_alloc(|| {
            convert_chords_into(&next, &mut out);
        });

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].sample_offset, 8);
        assert_eq!(out[0].text, "Dmin9".encode_utf16().collect::<Vec<_>>());
        assert_eq!(out[1].text, "G7".encode_utf16().collect::<Vec<_>>());
    }

    /// A shorter block truncates; a longer one grows. Both must leave the
    /// surviving prefix correct rather than stale.
    #[test]
    fn refilling_tracks_a_changing_event_count() {
        let mut out = Vec::new();
        convert_chords_into(
            &changes(&[chord(0, "a"), chord(1, "b"), chord(2, "c")]),
            &mut out,
        );
        assert_eq!(out.len(), 3);

        convert_chords_into(&changes(&[chord(9, "z")]), &mut out);
        assert_eq!(out.len(), 1, "shrinks to the new count");
        assert_eq!(out[0].sample_offset, 9);
        assert_eq!(out[0].text, "z".encode_utf16().collect::<Vec<_>>());

        convert_chords_into(&changes(&[chord(3, "p"), chord(4, "q")]), &mut out);
        assert_eq!(out.len(), 2, "grows past the retained entry");
        assert_eq!(out[1].text, "q".encode_utf16().collect::<Vec<_>>());
    }

    /// An empty block must clear rather than leave the previous block's chords
    /// visible — the stale-data failure the truncate guards against.
    #[test]
    fn an_empty_block_clears_the_buffer() {
        let mut out = Vec::new();
        convert_chords_into(&changes(&[chord(0, "Cmaj7")]), &mut out);
        assert_eq!(out.len(), 1);

        convert_chords_into(&changes(&[]), &mut out);
        assert!(out.is_empty(), "no chords this block means none are staged");
    }

    /// `NoteExpressionIntValue` has no nested buffer, so its converter is the
    /// plain clear+extend. Covered so the divergence stays deliberate.
    #[test]
    fn int_expressions_refill_without_nesting() {
        use tutti_plugin_types::NoteExpressionIntValue;
        let mut src = NoteExpressionIntChanges::new();
        assert!(src.add_change(NoteExpressionIntValue {
            sample_offset: 4,
            note_id: 7,
            type_id: 2,
            value: 41,
        }));

        let mut out = Vec::new();
        convert_expr_ints_into(&src, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, 41);

        convert_expr_ints_into(&NoteExpressionIntChanges::new(), &mut out);
        assert!(out.is_empty());
    }
}

