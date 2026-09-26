//! The unit as a graph node (doc 013, rewrite item 5): MIDI arrives on an
//! event input, on its frame (to rustysynth's 8-frame chunk, see
//! [`SoundFontUnit`]'s `process`), merged with what reaches its own port
//! ([`MidiInPort::gather`](tutti_midi_runtime::MidiInPort::gather)).
//!
//! As a node it **follows its graph's rate**: `prepare` rebuilds the
//! synthesizer at the prepared rate (control thread, where that may
//! allocate), keeping the preset and the port. An `AudioUnit` cannot, since
//! `set_sample_rate` may be called where rebuilding is not allowed.

use tutti_core::{AudioUnit, ChannelLayout};
use tutti_graph::{
    Cx, IntoNode, Io, Node, NodeParts, Prepare, Resolution, Shape, SortedEvents, Status,
};

use crate::SoundFontUnit;

/// The most events a block takes from the unit's own port; the rest of its
/// MIDI scratch holds the event input's.
const MAILBOX: usize = 128;

impl Node for SoundFontUnit {
    /// No audio in, stereo out, one MIDI event input, placed to the 8-frame
    /// chunk rustysynth renders in.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
            .with_events(1, 0)
            .with_event_resolution(Resolution::Frames(crate::SYNTH_BLOCK_FRAMES as u32))
    }

    /// Re-rate to the prepared rate (keeping preset and port), and size the
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

    /// Always [`Status::Modified`]: fed out of band (its port) and ringing
    /// after its last event, it must never be parked.
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames().min(self.left_buffer.len());
        if size == 0 {
            return Status::Modified;
        }
        let events = if io.event_input_count() > 0 {
            io.events(0)
        } else {
            SortedEvents::EMPTY
        };
        let count = self.midi.gather(
            size,
            self.sample_rate,
            &mut self.midi_buffer,
            MAILBOX,
            events,
        );
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
        AudioUnit::reset(self);
    }
}

/// The unit, inserted as a graph node: its fork is a graph node too, which
/// follows its render's rate. No controls: its port and sender are taken
/// from it before it goes in.
impl IntoNode for SoundFontUnit {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        let fork = self.native_fork();
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(fork)),
        }
    }
}
