//! Cutting a block at its MIDI events.

use core::ops::Range;
use tutti_graph::UmpAt;

/// What to do with an event whose offset is at or past the block's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overrun {
    /// Apply it after the last frame has rendered.
    Defer,
    /// Apply it just before the last frame. An empty block applies nothing.
    ClampToLastFrame,
}

/// One step of a block, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Apply this event to the instrument now.
    Event(UmpAt),
    /// Render these frames, which no event falls inside.
    Span(Range<usize>),
}

/// Walks a block of `size` frames in the order an instrument must act: every
/// event is applied at its own offset, before the frame it governs, and the
/// frames between events are rendered as spans. Events at one offset apply in
/// the order they arrive.
///
/// The events are passed to [`step`](Self::step) rather than held, so a caller
/// can keep them in the instrument it is driving.
///
/// ```
/// use tutti_midi_runtime::{BlockSplitter, Overrun, Step};
/// let mut walk = BlockSplitter::new(8, Overrun::Defer);
/// assert_eq!(walk.step(&[]), Some(Step::Span(0..8)));
/// assert_eq!(walk.step(&[]), None);
/// ```
#[derive(Debug, Clone)]
pub struct BlockSplitter {
    size: usize,
    overrun: Overrun,
    pos: usize,
    next_event: usize,
}

impl BlockSplitter {
    /// Starts a walk over `size` frames.
    #[must_use]
    pub fn new(size: usize, overrun: Overrun) -> Self {
        Self {
            size,
            overrun,
            pos: 0,
            next_event: 0,
        }
    }

    /// Where `event` takes effect in this block.
    #[inline]
    fn offset_of(&self, event: &UmpAt) -> usize {
        let at = event.0.index();
        match self.overrun {
            Overrun::Defer => at,
            Overrun::ClampToLastFrame => at.min(self.size.saturating_sub(1)),
        }
    }

    /// The next step over `events` (sorted by offset, the same slice every
    /// call), or `None` when the block is done.
    #[inline]
    #[must_use]
    pub fn step(&mut self, events: &[UmpAt]) -> Option<Step> {
        let due = events.get(self.next_event);
        if let Some(event) = due
            && (self.pos >= self.size || self.offset_of(event) <= self.pos)
        {
            // Past the end, only `Defer` has events left to apply.
            if self.pos >= self.size && self.overrun == Overrun::ClampToLastFrame {
                return None;
            }
            self.next_event += 1;
            return Some(Step::Event(*event));
        }
        if self.pos >= self.size {
            return None;
        }
        let end = due.map_or(self.size, |e| self.offset_of(e).min(self.size));
        let span = self.pos..end;
        self.pos = end;
        Some(Step::Span(span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_fixtures::midi::note_on;
    use tutti_graph::Offset;
    use tutti_types::Samples;
    use tutti_ump::{MidiChannel, MidiGroup};

    /// An event at `offset`, which may lie past the walked block: it is built
    /// against a block long enough to hold it.
    fn at(offset: u32, note: u8) -> UmpAt {
        let long = Samples(1 << 20);
        (
            Offset::new(offset as usize, long).unwrap(),
            note_on(MidiGroup::FIRST, MidiChannel::FIRST, note, 0xFFFF),
        )
    }

    fn walk(size: usize, overrun: Overrun, events: &[UmpAt]) -> Vec<String> {
        let mut split = BlockSplitter::new(size, overrun);
        let mut out = Vec::new();
        while let Some(step) = split.step(events) {
            // A splitter that stops advancing fails here instead of hanging.
            assert!(out.len() < 64, "the walk does not terminate: {out:?}");
            out.push(match step {
                Step::Event(e) => format!("e{}@{}", (e.1.0[0] >> 8) & 0x7f, e.0.get()),
                Step::Span(r) => format!("{}..{}", r.start, r.end),
            });
        }
        out
    }

    /// Each event lands before the frame it governs, and equal offsets keep
    /// their order.
    ///
    /// Mutation (run): the `<=` in `step` as `<` -> at pos == offset == 0 the
    /// span is `0..0` forever -> the step cap in `walk` panics -> fails.
    #[test]
    fn events_split_the_block_at_their_offsets() {
        let events = [at(0, 60), at(3, 61), at(3, 62), at(6, 63)];
        assert_eq!(
            walk(8, Overrun::Defer, &events),
            ["e60@0", "0..3", "e61@3", "e62@3", "3..6", "e63@6", "6..8"]
        );
        assert_eq!(
            walk(8, Overrun::ClampToLastFrame, &events),
            walk(8, Overrun::Defer, &events)
        );
    }

    /// An event past the end waits for the render under `Defer` and lands on
    /// the last frame under `ClampToLastFrame`.
    ///
    /// Mutation (run): `Defer` clamping like the other policy -> the event
    /// comes before the last frame -> fails.
    #[test]
    fn an_overrun_event_follows_its_policy() {
        let events = [at(20, 60)];
        assert_eq!(walk(8, Overrun::Defer, &events), ["0..8", "e60@20"]);
        assert_eq!(
            walk(8, Overrun::ClampToLastFrame, &events),
            ["0..7", "e60@20", "7..8"]
        );
    }

    /// An empty block renders nothing; only `Defer` still applies its events.
    ///
    /// Mutation (run): dropping the `ClampToLastFrame` guard past the end ->
    /// the clamped walk applies the event -> fails.
    #[test]
    fn an_empty_block_applies_events_only_when_deferred() {
        let events = [at(0, 60)];
        assert_eq!(walk(0, Overrun::Defer, &events), ["e60@0"]);
        assert!(walk(0, Overrun::ClampToLastFrame, &events).is_empty());
    }

    /// An event at exactly the block's length is an overrun: `Defer` applies it
    /// after the last frame, `ClampToLastFrame` before the last frame.
    ///
    /// Mutation (run): `ClampToLastFrame` clamping to `size` instead of
    /// `size - 1` -> the event lands after frame 8, not before frame 7 -> fails.
    #[test]
    fn an_event_at_exactly_the_block_length_is_an_overrun() {
        let events = [at(8, 60)];
        assert_eq!(walk(8, Overrun::Defer, &events), ["0..8", "e60@8"]);
        assert_eq!(
            walk(8, Overrun::ClampToLastFrame, &events),
            ["0..7", "e60@8", "7..8"]
        );
    }

    /// Several overrun events under `Defer` all apply, in arrival order.
    ///
    /// Mutation (run): ending the walk after the first deferred event -> the
    /// later ones are lost -> fails.
    #[test]
    fn deferred_events_keep_their_arrival_order() {
        let events = [at(10, 60), at(10, 61), at(40, 62)];
        assert_eq!(
            walk(8, Overrun::Defer, &events),
            ["0..8", "e60@10", "e61@10", "e62@40"]
        );
    }

    /// An overrun event after in-block ones does not move the in-block spans.
    ///
    /// Mutation (run): the `.min(self.size)` on the span end removed -> the
    /// span runs to the overrun offset, `2..20` -> fails.
    #[test]
    fn an_overrun_event_leaves_the_in_block_spans_alone() {
        let events = [at(2, 60), at(20, 61)];
        assert_eq!(
            walk(8, Overrun::Defer, &events),
            ["0..2", "e60@2", "2..8", "e61@20"]
        );
        assert_eq!(
            walk(8, Overrun::ClampToLastFrame, &events),
            ["0..2", "e60@2", "2..7", "e61@20", "7..8"]
        );
    }
}
