//! The synth as a graph node: MIDI arrives
//! on an event input port, on its frame, from whatever feeds it — a
//! [`MidiClipNode`](tutti_midi_runtime::MidiClipNode), an arpeggiator, a
//! hardware source — in the same block it was written.
//!
//! A keyboard reaches it the same way: through an event inlet
//! (`tutti_graph::EventInletNode`) wired to its input. Driven by hand (a test, a bench), its events are handed to it
//! the same way, through `tutti_graph::contract::{drive_input, Direct}`.
//!
//! Its controls are its [`ParamSet`]: the master volume and, with a unison
//! engine, the unison detune and stereo spread, by `UnitParam`. It is a
//! [`ParamNode`], inserted through [`tutti_graph::param_parts`], so a fork
//! starts from the values last set through the set (see `src/fork.rs`).

use tutti_graph::{
    Chase, Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_types::{Amplitude, BusTopology, Cents, Spread};
use tutti_voicing::Voicing;

use super::PolySynth;

// The params a `PolySynth` lets the graph modulate, in port order: master
// volume and, with a unison engine, detune and stereo spread. Each is
// block-rate in the synth (read once per block), so a driven port is read the
// same way: its value on the block's last frame.
tutti_graph::declare_params!([] PolySynth, n {
    Volume => Amplitude = &n.master_volume,
    Detune => Cents = n.unison.as_ref().map(|u| u.detune()),
    StereoSpread => Spread = n.unison.as_ref().map(|u| u.spread()),
});

/// The block's value of declared param port `k` when a lane drives it: its
/// last frame. `None`: undriven, read the cell.
#[inline]
fn block_driven(io: &Io<'_>, k: usize) -> Option<f32> {
    io.param(k).frames().and_then(|v| v.last().copied())
}

/// The most oscillator lanes (`max_voices` × unison sub-voices) a synth
/// may render and still declare [`Cost::Moderate`](tutti_graph::Cost).
///
/// The voice bank renders each SIMD group of lanes that holds a sounding
/// voice, so at full polyphony the synth costs what its lane count does.
/// Measured with a saw, every voice held (doc 013, "The cost model and coarsening"):
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
}

impl Node for PolySynth {
    /// No audio in, stereo out, one MIDI event input; the release as its
    /// tail. Events land on their frame ([`Resolution::Sample`](tutti_graph::Resolution::Sample)).
    fn shape(&self) -> Shape {
        // The unison engine is built with the synth or never (`&mut`
        // setters only), so every read of the shape declares the same ports.
        Shape::source(BusTopology::STEREO)
            .with_events(1, 0)
            .with_tail(self.release_tail())
            .with_cost(self.cost())
            .with_param_addrs(self.declared_ports().as_slice())
            // The channel state it hands the synth that replaces it (`chase`).
            .with_chase(Voicing::CHASE_MESSAGES as u32)
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
        let blocks = io.sub_blocks(0);
        let volume = block_driven(&io, 0).map_or_else(
            || {
                let cell = self.master_volume.load().get();
                if cell.is_finite() {
                    self.last_volume = cell;
                }
                self.last_volume
            },
            Self::clamp_volume,
        );
        let (_, mut outputs) = io.split();
        let mut channels = outputs.iter_mut();
        let (Some(left), right) = (channels.next(), channels.next()) else {
            return Status::Modified;
        };
        match right {
            Some(right) => self.render_events(blocks, volume, &mut |i, l, r| {
                left[i] = l;
                right[i] = r;
            }),
            None => self.render_events(blocks, volume, &mut |i, l, _| left[i] = l),
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.reset_voices();
    }

    /// Hands the instrument that replaces this one every channel's
    /// controllers, pedals, pitch wheel, pressure and registered controllers,
    /// as MIDI 2.0 messages in the synth's Group (`Voicing::chase`): what a
    /// performer set survives a swap, whatever the successor is. The sounding
    /// notes are not carried (nothing re-strikes a held key), nor the
    /// per-note controllers or the key a portamento glides from. A synth has
    /// no program or bank.
    fn chase(&self, out: &mut Chase<'_>) {
        self.allocator.chase(self.config.group, |ump| {
            out.push(ump);
        });
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        self.declared_base(k)
    }
}

impl ParamNode for PolySynth {
    /// `Volume` (the master gain, as a linear amplitude), and — only when
    /// this synth has a unison engine — `Detune` (cents) and `StereoSpread`
    /// (0..1): the cells the synth reads once per block, which a host
    /// writes (and a control lane takes as its knob).
    fn param_set(&self) -> ParamSet {
        self.declared_set()
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

/// The synth is an instrument: MIDI on event input 0, its stereo pair on
/// outputs 0 and 1.
impl tutti_graph::Instrument for PolySynth {}

#[cfg(test)]
mod tests;
