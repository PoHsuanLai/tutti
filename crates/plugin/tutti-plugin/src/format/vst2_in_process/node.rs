//! The in-process VST2 plugin as a graph node (doc 013, rewrite item 5): MIDI
//! arrives on an event input, on its frame, and the plugin's MIDI-out leaves
//! on an event output (a plugin that declared `Features::MIDI_OUT`), each
//! event on the frame the plugin gave it.
//!
//! Not forkable (see `AudioUnit::forkable` on [`InProcessVst2Client`]): a
//! graph holding one is refused a fork. Its transport still comes from the
//! reader installed with `set_transport_source`, polled once per block.

use tutti_core::{AudioUnit, ChannelLayout, Samples};
use tutti_graph::{
    Cx, Event, EventKind, IntoNode, Io, Node, NodeParts, Offset, Prepare, Shape, Status, Ump,
};
use tutti_midi_types::ump::MidiEvent;
use tutti_types::Latency;

use super::audio_unit::{drive_f32, InProcessVst2Client};
use crate::protocol::Features;

/// The most channels a block hands the plugin, as the `AudioUnit` path does.
const MAX_CHANNELS: usize = 16;

impl Node for InProcessVst2Client {
    /// The plugin's channels, one MIDI event input, and a MIDI event output
    /// for a plugin that declared MIDI out; its latency and tail as loaded.
    fn shape(&self) -> Shape {
        let midi_out = u16::from(self.features.contains(Features::MIDI_OUT));
        let latency = Latency::new(self.metadata.latency_samples);
        Shape::audio(
            ChannelLayout::from_count(self.metadata.num_inputs.count()),
            ChannelLayout::from_count(self.metadata.num_outputs.count()),
        )
        .with_events(1, midi_out)
        .with_latency(latency)
        .with_tail(self.metadata.tail)
    }

    /// The rate (queued for the plugin, as `AudioUnit::set_sample_rate`
    /// does), and scratch for the largest block. Control thread.
    fn prepare(&mut self, p: &Prepare) {
        <Self as AudioUnit>::set_sample_rate(self, p.sample_rate());
        self.ensure_scratch_size(p.max_block().get());
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        let n_in = (self.metadata.num_inputs.count() as usize).min(MAX_CHANNELS);
        let n_out = (self.metadata.num_outputs.count() as usize).min(MAX_CHANNELS);
        if size == 0 || size > self.process_scratch.f32_in.first().map_or(size, Vec::len) {
            return Status::Modified;
        }
        // The event input's MIDI, in its (sorted) order; past the inline
        // capacity it is dropped rather than spill (allocate).
        self.midi.clear();
        if io.event_input_count() > 0 {
            for e in io.events(0) {
                if let EventKind::Midi(Ump(data)) = e.kind {
                    if self.midi.len() < self.midi.inline_size() {
                        self.midi.push(MidiEvent {
                            frame_offset: e.offset.get(),
                            data,
                        });
                    }
                }
            }
        }
        {
            let (inputs, _) = io.split();
            for ch in 0..n_in {
                self.process_scratch.f32_in[ch][..size].copy_from_slice(&inputs.get(ch)[..size]);
            }
        }
        let transport = *self.transport.drain(self.features);
        let sends = self.features.contains(Features::MIDI_OUT) && io.event_output_count() > 0;
        let mut out_events = crate::protocol::MidiEventVec::new();
        let processed = drive_f32(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &mut |events| {
                if sends {
                    for e in events {
                        if out_events.len() < out_events.inline_size() {
                            out_events.push(*e);
                        }
                    }
                }
            },
            &transport,
            &mut self.scratch,
            &mut self.process_scratch,
            n_in,
            n_out,
            size,
            self.sample_rate,
        );
        let (_, mut outputs) = io.split();
        for (ch, out) in outputs.iter_mut().enumerate().take(n_out) {
            if processed {
                out[..size].copy_from_slice(&self.process_scratch.f32_out[ch][..size]);
            } else {
                out[..size].fill(0.0);
            }
        }
        if sends {
            sort_by_offset(&mut out_events);
            let writer = io.event_out(0);
            let block = Samples(size);
            for e in &out_events {
                let at = (e.frame_offset as usize).min(size - 1);
                if let Some(at) = Offset::new(at, block) {
                    let _ = writer.push(Event::midi(at, e.data));
                }
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        <Self as AudioUnit>::reset(self);
    }
}

/// Stable insertion sort by frame offset; allocation-free.
fn sort_by_offset(events: &mut crate::protocol::MidiEventVec) {
    for i in 1..events.len() {
        let mut j = i;
        while j > 0 && events[j - 1].frame_offset > events[j].frame_offset {
            events.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// The plugin, inserted as a graph node. No controls (its per-block inputs
/// are installed through it before it goes in), and no fork source: see the
/// module docs.
impl IntoNode for InProcessVst2Client {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: None,
        }
    }
}
