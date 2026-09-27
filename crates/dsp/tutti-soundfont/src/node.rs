//! The unit as a graph node: MIDI arrives on an
//! event input, on its frame (to rustysynth's 8-frame chunk, see
//! [`SoundFontUnit`]'s "How an event's offset is honoured").
//!
//! It **follows its graph's rate**: `prepare` rebuilds the synthesizer at the
//! prepared rate (control thread, where that may allocate), keeping the
//! preset.

use tutti_core::ChannelLayout;
use tutti_graph::{
    Cx, EventKind, IntoNode, Io, Node, NodeParts, Prepare, Resolution, Shape, SortedEvents, Status,
    Ump,
};
use tutti_midi_types::ump::MidiEvent;

use crate::SoundFontUnit;

impl SoundFontUnit {
    /// The block's MIDI from `events` into the scratch, in their (sorted)
    /// order; returns how many it holds.
    fn gather_events(&mut self, events: SortedEvents<'_>) -> usize {
        let mut n = 0;
        for e in events {
            if n == self.midi_buffer.len() {
                break;
            }
            if let EventKind::Midi(Ump(data)) = e.kind {
                self.midi_buffer[n] = MidiEvent {
                    frame_offset: e.offset.get(),
                    data,
                };
                n += 1;
            }
        }
        n
    }
}

impl Node for SoundFontUnit {
    /// No audio in, stereo out, one MIDI event input, placed to the 8-frame
    /// chunk rustysynth renders in.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
            .with_events(1, 0)
            .with_event_resolution(Resolution::Frames(crate::SYNTH_BLOCK_FRAMES as u32))
    }

    /// Re-rate to the prepared rate (keeping the preset), and size the
    /// render scratch to the largest block. Control thread.
    fn prepare(&mut self, p: &Prepare) {
        let rate = p.sample_rate();
        if rate.get().round() != self.sample_rate.get().round() {
            if let Ok(unit) = self.with_sample_rate(rate) {
                *self = unit;
            }
        }
        let frames = p.max_block().get();
        if self.left_buffer.len() < frames {
            self.left_buffer.resize(frames, 0.0);
            self.right_buffer.resize(frames, 0.0);
        }
    }

    /// Always [`Status::Modified`]: ringing after its last event, it must
    /// never be parked.
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        // No clamp: `prepare` sized the scratch to the prepared `MaxBlock`,
        // and the graph never hands a node a longer block.
        let size = io.frames();
        let events = if io.event_input_count() > 0 {
            io.events(0)
        } else {
            SortedEvents::EMPTY
        };
        let count = self.gather_events(events);
        self.render_events(size, count);
        let (_, mut outputs) = io.split();
        let mut channels = outputs.iter_mut();
        if let Some(left) = channels.next() {
            left[..size].copy_from_slice(&self.left_buffer[..size]);
        }
        if let Some(right) = channels.next() {
            right[..size].copy_from_slice(&self.right_buffer[..size]);
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.release_all();
    }
}

/// The unit, inserted as a graph node: its fork is a graph node too, which
/// follows its render's rate (see the `fork` module docs). No controls: the
/// unit has no param a host sets while it plays (its preset is MIDI's).
impl IntoNode for SoundFontUnit {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        let fork = self.fork_template();
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(fork)),
        }
    }
}
