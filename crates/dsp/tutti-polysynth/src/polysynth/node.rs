//! The synth as a graph node: MIDI arrives
//! on an event input port, on its frame, from whatever feeds it — a
//! [`MidiClipNode`](tutti_midi_runtime::MidiClipNode), an arpeggiator, a
//! hardware source — in the same block it was written.
//!
//! A keyboard reaches it the same way: through a `MidiQueueNode` wired to
//! its input. Driven by hand (a test, a bench), its events are handed to it
//! the same way, through `tutti_graph::contract::{drive_in, Direct}`.
//!
//! Its controls are its [`ParamSet`]: the master volume and, with a unison
//! engine, the unison detune and stereo spread, by `UnitParam`. It is a
//! [`ParamNode`], inserted through [`tutti_graph::param_parts`], so a fork
//! starts from the values last set through the set (see `src/fork.rs`).

use tutti_core::{ChannelLayout, UnitParam};
use tutti_graph::{
    Cx, EventKind, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape,
    SortedEvents, Status, Ump,
};
use tutti_midi_types::MidiEvent;

use super::PolySynth;

/// The params a [`PolySynth`] with a unison engine lets the graph
/// modulate, in port order: master volume, unison detune and stereo
/// spread. A synth without one declares only the volume (the first). Each
/// is block-rate in the synth (read once per block), so a driven port is
/// read the same way: its value on the block's last frame.
pub const SYNTH_PARAMS: [UnitParam; 3] = [
    UnitParam::Volume,
    UnitParam::Detune,
    UnitParam::StereoSpread,
];

/// The block's value of declared param port `k` when a lane drives it: its
/// last frame. `None`: undriven, read the cell.
#[inline]
fn block_driven(io: &Io<'_>, k: usize) -> Option<f32> {
    io.param(k).frames().and_then(|v| v.last().copied())
}

/// The synth's MIDI scratch, in events: a block's event input past it is
/// dropped.
pub(super) const MIDI_BUFFER: usize = 512;

/// The most oscillator lanes (`max_voices` × unison sub-voices) a synth
/// may render and still declare [`Cost::Moderate`](tutti_graph::Cost).
///
/// The voice bank renders each SIMD group of lanes that holds a sounding
/// voice, so at full polyphony the synth costs what its lane count does.
/// Measured with a saw, every voice held (doc 013, "Phase 6, part (b)"):
/// 18–21 ns per frame and channel at 8 lanes, 74 at 32 (65 at 8 voices ×
/// 4 unison), about 2.2 more per lane, against `SvfFilterNode`'s 7.2
/// (`Moderate`'s reference) and `Heavy`'s 256. 16 lanes come to about
/// 38 ns, under the two classes' geometric middle (45); 20 would be over.
pub(super) const MODERATE_LANES: usize = 16;

impl PolySynth {
    /// What the synth declares to the cost model: `Moderate` up to
    /// [`MODERATE_LANES`] oscillator lanes, `Heavy` past them.
    ///
    /// The editor reads it at every insert and re-prepare. The lane count
    /// can change only through `&mut` setters (`set_unison_voice_count`),
    /// which a node in a graph is out of reach of, so every read agrees.
    pub(crate) fn cost(&self) -> tutti_graph::Cost {
        if self.config.max_voices * self.unison_voice_count() <= MODERATE_LANES {
            tutti_graph::Cost::Moderate
        } else {
            tutti_graph::Cost::Heavy
        }
    }

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

impl Node for PolySynth {
    /// No audio in, stereo out, one MIDI event input; the release as its
    /// tail. Events land on their frame ([`Resolution::Sample`](tutti_graph::Resolution::Sample)).
    fn shape(&self) -> Shape {
        // The unison engine is built with the synth or never (`&mut`
        // setters only), so every read of the shape declares the same ports.
        let ports = if self.unison.is_some() { 3 } else { 1 };
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
            .with_events(1, 0)
            .with_tail(self.release_tail())
            .with_cost(self.cost())
            .with_params(&SYNTH_PARAMS[..ports])
    }

    fn prepare(&mut self, p: &Prepare) {
        self.set_rate(p.sample_rate());
    }

    /// Always [`Status::Modified`]: the synth sounds after its input's last
    /// event (its release), so the executor must never park it.
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let frames = io.frames();
        if frames == 0 {
            return Status::Modified;
        }
        if let Some(unison) = &mut self.unison {
            unison.sync_from_atomics([block_driven(&io, 1), block_driven(&io, 2)]);
        }
        let events = if io.event_input_count() > 0 {
            io.events(0)
        } else {
            SortedEvents::EMPTY
        };
        let count = self.gather_events(events);
        let volume =
            block_driven(&io, 0).map_or_else(|| self.master_volume.load().get(), |v| v.max(0.0));
        let (_, mut outputs) = io.split();
        let mut channels = outputs.iter_mut();
        let (Some(left), right) = (channels.next(), channels.next()) else {
            return Status::Modified;
        };
        match right {
            Some(right) => self.render_events(frames, count, volume, &mut |i, l, r| {
                left[i] = l;
                right[i] = r;
            }),
            None => self.render_events(frames, count, volume, &mut |i, l, _| left[i] = l),
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.reset_voices();
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match k {
            0 => Some(self.master_volume.load().get()),
            1 => self
                .detune_atomic()
                .map(|c| c.load(std::sync::atomic::Ordering::Acquire)),
            2 => self
                .spread_atomic()
                .map(|c| c.load(std::sync::atomic::Ordering::Acquire)),
            _ => None,
        }
    }
}

impl ParamNode for PolySynth {
    /// `Volume` (the master gain, as a linear amplitude), and — only when
    /// this synth has a unison engine — `Detune` (cents) and `StereoSpread`
    /// (0..1): the cells the synth reads once per block, which a host
    /// writes (and a control lane takes as its knob).
    fn param_set(&self) -> ParamSet {
        let set = ParamSet::builder().param(UnitParam::Volume, self.volume_atomic());
        match (self.detune_atomic(), self.spread_atomic()) {
            (Some(detune), Some(spread)) => set
                .param(UnitParam::Detune, detune)
                .param(UnitParam::StereoSpread, spread),
            _ => set,
        }
        .build()
    }

    /// See the `fork` module docs (`src/fork.rs`).
    fn fork_fresh(&self) -> Self {
        self.fork_instance()
    }
}

/// The synth, inserted with its [`ParamSet`] as its controls and a
/// fork that starts from the values last set through it
/// ([`tutti_graph::param_parts`]; see the `fork` module docs for what a fork
/// carries).
impl IntoNode for PolySynth {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests {
    use tutti_graph::{Event, Offset, SortedEvents};
    use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};

    use crate::{PolySynth, SynthConfig};

    /// **The ports and their bases.** A synth declares its volume, and with
    /// a unison engine its detune and spread too ([`SYNTH_PARAMS`](super::SYNTH_PARAMS)),
    /// each port's base its cell.
    ///
    /// Mutation (run, PR 5): `param_base(2)` answering the detune cell →
    /// fails. Mutation (run, PR 5): the shape declaring all three without
    /// unison → fails.
    #[test]
    fn the_shape_declares_volume_and_the_unison_params() {
        use tutti_core::UnitParam;
        use tutti_graph::Node;

        let plain = PolySynth::new(SynthConfig::default()).expect("builds");
        plain.set_volume(0.5);
        assert_eq!(plain.shape().params.as_slice(), &[UnitParam::Volume][..]);
        assert_eq!(plain.param_base(0), Some(0.5));
        assert_eq!(plain.param_base(1), None);

        let unison = PolySynth::new(SynthConfig {
            unison: Some(crate::UnisonConfig {
                voice_count: 3,
                detune_cents: tutti_core::Cents(12.0),
                stereo_spread: tutti_core::Spread::new_clamped(0.25),
                phase_randomize: false,
            }),
            ..SynthConfig::default()
        })
        .expect("builds");
        assert_eq!(unison.shape().params.as_slice(), &super::SYNTH_PARAMS[..]);
        assert_eq!(unison.param_base(1), Some(12.0));
        assert_eq!(unison.param_base(2), Some(0.25));
    }

    fn note(n: u8) -> MidiEvent {
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, n, 0xFFFF)
    }

    /// **A graph block plays its event input, on its offsets.**
    ///
    /// Mutation (run): `gather_events` writing every event at index 0 (not
    /// advancing `n`) → no event gathered → fails.
    #[test]
    fn a_graph_block_plays_its_event_input() {
        let mut synth = PolySynth::new(SynthConfig::default()).expect("builds");
        let at = |k: usize| Offset::new(k, tutti_core::Samples(512)).expect("inside");
        let events = [
            Event::midi(at(5), note(70).data),
            Event::midi(at(200), note(72).data),
        ];
        let sorted = SortedEvents::new(&events, 512).expect("sorted");
        let n = synth.gather_events(sorted);
        let got: Vec<(u32, u32)> = synth.midi_buffer[..n]
            .iter()
            .map(|e| (e.frame_offset, (e.data[0] >> 8) & 0x7f))
            .collect();
        assert_eq!(got, [(5, 70), (200, 72)]);
    }
}
