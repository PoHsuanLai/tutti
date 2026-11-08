//! The unit as a graph node: MIDI arrives on an
//! event input, on its frame (to the voices' 8-frame chunk, see
//! [`SoundFontUnit`]'s "How an event's offset is honoured").
//!
//! It follows its graph's rate: `prepare` rebuilds the synthesizer at the
//! prepared rate (control thread, where that may allocate), keeping the
//! preset.

use tutti_graph::{
    Cx, IntoNode, Io, Node, NodeParts, Prepare, Resolution, Shape, SortedEvents, Status,
};
use tutti_types::ChannelLayout;

use crate::SoundFontUnit;

impl Node for SoundFontUnit {
    /// No audio in, stereo out, one MIDI event input, placed to the 8-frame
    /// chunk the voices render in, and the tail of the longest release.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
            .with_events(1, 0)
            .with_event_resolution(Resolution::Frames(crate::SYNTH_BLOCK_FRAMES as u32))
            // Sample playback, filters and effects per voice: measured 76 ns
            // per frame and channel with one note held, 140 with eight,
            // against `SvfFilterNode`'s 7.2 (doc 013, "The cost model and coarsening").
            // The default, stated.
            .with_cost(tutti_graph::Cost::Heavy)
            // The longest a voice rings after its key is up, and the effects
            // after it: an export that ends at the last note-off extends by
            // this, and keeps the release.
            .with_tail(self.shape_tail())
    }

    /// Re-rate to the prepared rate (keeping the preset), and size the
    /// render scratch to the largest block. Control thread.
    fn prepare(&mut self, p: &Prepare) {
        let rate = p.sample_rate();
        if rate.get().round() != self.sample_rate.get().round()
            && let Ok(unit) = self.with_sample_rate(rate)
        {
            *self = unit;
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
        let count = tutti_midi_runtime::gather_midi(events, &mut self.midi_buffer);
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
        self.reset_player();
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

/// A SoundFont unit is an instrument: MIDI on event input 0, its stereo pair on
/// outputs 0 and 1.
impl tutti_graph::Instrument for SoundFontUnit {}
