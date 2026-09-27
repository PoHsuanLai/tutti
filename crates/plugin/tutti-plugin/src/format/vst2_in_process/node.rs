//! The in-process VST2 plugin as a graph node: MIDI
//! arrives on an event input, on its frame, and the plugin's MIDI-out leaves
//! on an event output (a plugin that declared `Features::MIDI_OUT`), each
//! event on the frame the plugin gave it.
//!
//! Not forkable (see its `IntoNode` below): a graph holding one is refused a
//! fork. Its transport still comes from the reader installed with
//! `set_transport_source`, polled once per block.

use tutti_core::{ChannelLayout, Samples};
use tutti_graph::{
    Cx, Event, EventKind, IntoNode, Io, Node, NodeParts, Offset, Prepare, Shape, Status, Ump,
};
use tutti_midi_types::ump::MidiEvent;
use tutti_types::Latency;

use super::client::{drive_f32, InProcessVst2Client};
use crate::protocol::Features;

/// The most channels a block hands the plugin (`drive_f32`'s stack tables).
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

    /// The rate (parked for the plugin, dispatched from the main thread),
    /// and scratch for the largest block. Control thread.
    fn prepare(&mut self, p: &Prepare) {
        self.set_rate(p.sample_rate());
        self.ensure_scratch_size(p.max_block().get());
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        let n_in = (self.metadata.num_inputs.count() as usize).min(MAX_CHANNELS);
        let n_out = (self.metadata.num_outputs.count() as usize).min(MAX_CHANNELS);
        // No clamp and no size check: `prepare` sized the scratch to the
        // prepared `MaxBlock`, and a block is never longer.
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
        // Nothing reaches the plugin from here, and nothing can. VST 2.4 has no
        // opcode that clears DSP state on its own: the only two that touch it
        // are `effMainsChanged`, where plugins allocate and free their
        // rate-dependent buffers, and the `effStartProcess`/`effStopProcess`
        // pair, which announces an interruption rather than a clear and is only
        // legal while resumed. Neither may race the audio thread's `process`.
        // `Vst2Instance::reset_processing_state` is that cycle, on the main
        // thread, for a host that wants it on a locate or a loop wrap.
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
/// are installed through it before it goes in), and **no fork source**: a
/// graph holding an in-process VST2 plugin is refused a fork
/// (`ForkError::NotForkable`), so it cannot be rendered offline through
/// `Editor::fork`. Use the out-of-process
/// [`PluginClient`](crate::handles::PluginClient) when you need that.
//
// A clone shares the one in-process instance, so a fork would render through
// the live plugin's state. Forking would need a second `AEffect` from the same
// library in this process (one more image-global the two could share), and a
// plugin whose state is not a chunk (`programsAreChunks` clear) saves only its
// current program's parameters.
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
