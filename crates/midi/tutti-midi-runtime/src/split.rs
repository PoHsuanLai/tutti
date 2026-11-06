//! Cutting a block at its MIDI events.

use core::ops::Range;
use tutti_midi_types::MidiEvent;

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
    Event(MidiEvent),
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
    fn offset_of(&self, event: &MidiEvent) -> usize {
        let at = event.frame_offset as usize;
        match self.overrun {
            Overrun::Defer => at,
            Overrun::ClampToLastFrame => at.min(self.size.saturating_sub(1)),
        }
    }

    /// The next step over `events` (sorted by offset, the same slice every
    /// call), or `None` when the block is done.
    pub fn step(&mut self, events: &[MidiEvent]) -> Option<Step> {
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
    use tutti_midi_types::{MidiChannel, MidiGroup};

    fn at(offset: u32, note: u8) -> MidiEvent {
        let mut e = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, note, 0xFFFF);
        e.frame_offset = offset;
        e
    }

    fn walk(size: usize, overrun: Overrun, events: &[MidiEvent]) -> Vec<String> {
        let mut split = BlockSplitter::new(size, overrun);
        let mut out = Vec::new();
        while let Some(step) = split.step(events) {
            out.push(match step {
                Step::Event(e) => format!("e{}@{}", (e.data[0] >> 8) & 0x7f, e.frame_offset),
                Step::Span(r) => format!("{}..{}", r.start, r.end),
            });
        }
        out
    }

    /// Each event lands before the frame it governs, and equal offsets keep
    /// their order.
    ///
    /// Mutation (run): the `<=` in `step` as `<` -> an event at the span start
    /// is skipped past and the span is empty -> fails.
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
}
