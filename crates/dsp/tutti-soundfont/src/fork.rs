//! Forking a [`SoundFontUnit`] for the graph's export (`Editor::fork`,
//! design doc 013 PR 12): a fresh unit over the same decoded SoundFont, at
//! the render's rate. Its MIDI comes from its event input, as the live
//! unit's does: the fork of the graph forks the clip node feeding it too
//! (doc 013, rewrite item 5).
//!
//! # Why not a clone of the graph's shadow
//!
//! A host that inserts the unit through `tutti_graph::Legacy::controlled` gets
//! a fork source for free: a clone of the node's **shadow**, taken when the
//! node was inserted. It kept the live unit's rate whatever the export's was:
//! a 96 kHz export of a 48 kHz unit played every note an octave low, at half
//! its frame.
//!
//! # What this forks from instead
//!
//! A **template**: a clone of the unit taken when the source is made, never
//! processed. Its synthesizer is a clone too, and that clone shares the
//! decoded SoundFont (`Arc<SoundFont>`, sample data included): nothing is
//! reloaded or copied but the per-channel and voice state. Its preset is
//! whatever `program_change` set before the source was made. A fork is a
//! clone of the template, `AudioUnit::isolate` (nothing queued), then
//! `AudioUnit::reset` — every key released. The template never rendered, so
//! it holds no voice to release, and the fork starts silent.
//!
//! **The forked node follows its graph's rate.** A live unit's rate is fixed
//! (see [`SoundFontUnit`]'s "The sample rate is fixed"); the node a fork
//! source hands the graph wraps its unit in [`RateFollowing`], whose
//! `set_sample_rate` — called when the fork is prepared at the render's rate
//! — swaps in [`SoundFontUnit::with_sample_rate`], keeping the preset. A rate
//! RustySynth refuses (outside 16–192 kHz) leaves the unit at its own, as a
//! live unit stays.
//!
//! What the fork does **not** carry: sounding voices, and channel state the
//! live unit reached through MIDI after the template was taken (a program
//! change, a CC) — the clip replays whatever of that it holds.

use tutti_core::{AudioUnit, BufferMut, BufferRef, SampleRate, Setting, SignalFrame};
use tutti_graph::{ForkCause, ForkMode, ForkSource, Forked, IntoNode, Legacy};

use crate::SoundFontUnit;

/// [`SoundFontUnit::fork_source`]'s source: the template in the module docs.
pub(crate) struct SoundFontFork {
    template: SoundFontUnit,
    /// Whether the fork is a graph node (the unit was inserted as one,
    /// `IntoNode for SoundFontUnit`, which re-rates in `prepare`) rather than
    /// a `Legacy` one.
    native: bool,
}

impl SoundFontFork {
    /// The fork, as the unit the graph runs: what [`ForkSource::fork`]
    /// wraps.
    fn unit(&self) -> RateFollowing {
        RateFollowing(self.template.fork_instance())
    }
}

impl ForkSource for SoundFontFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        if self.native {
            // A graph node follows its graph's rate itself (`Node::prepare`).
            return Ok(Forked::new(Box::new(self.template.fork_instance())));
        }
        let fork = self.unit();
        // Run through `Legacy`, as a host runs the live unit; `into_node` so
        // it carries no fork source of its own (a fork is not forked again).
        Ok(Forked::new(Legacy::new(fork).into_node().0))
    }
}

/// A forked [`SoundFontUnit`] that renders at whatever rate its graph
/// prepares it for (see "The forked node follows its graph's rate" in the
/// module docs). Every other method forwards, `as_any` included, so a
/// downcast sees the unit.
#[derive(Clone)]
struct RateFollowing(SoundFontUnit);

impl AudioUnit for RateFollowing {
    fn reset(&mut self) {
        self.0.reset();
    }
    fn isolate(&mut self) {
        self.0.isolate();
    }
    /// Control thread (a graph prepares a unit there): may allocate.
    fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        if sample_rate.get().round() == self.0.sample_rate().get() {
            return;
        }
        if let Ok(unit) = self.0.with_sample_rate(sample_rate) {
            self.0 = unit;
        }
    }
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.0.tick(input, output);
    }
    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.0.process(size, input, output);
    }
    fn inputs(&self) -> usize {
        self.0.inputs()
    }
    fn outputs(&self) -> usize {
        self.0.outputs()
    }
    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        self.0.route(input, frequency)
    }
    fn set(&mut self, setting: Setting) {
        self.0.set(setting);
    }
    fn get_id(&self) -> u64 {
        self.0.get_id()
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self.0.as_any()
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self.0.as_any_mut()
    }
    fn footprint(&self) -> usize {
        self.0.footprint()
    }
    fn allocate(&mut self) {
        self.0.allocate();
    }
}

impl SoundFontUnit {
    /// A fresh unit for a fork of the graph this one plays in: the same
    /// SoundFont (shared, not reloaded), settings, rate and preset, nothing
    /// queued and no sounding voice. See the `fork` module docs
    /// (`src/fork.rs`); [`with_sample_rate`](Self::with_sample_rate) moves it
    /// to a render's rate.
    pub fn fork_instance(&self) -> SoundFontUnit {
        let mut fork = self.clone();
        fork.isolate();
        fork.reset();
        fork
    }

    /// The [`ForkSource`] a host hands the graph's editor when it inserts
    /// this unit, so that a fork of the graph (an export) forks it through
    /// [`fork_instance`](Self::fork_instance), at the fork's rate.
    ///
    /// For a host that wraps the unit in its own node builder (bevy-tutti's
    /// `Legacy::controlled`, for a settings ring and a shadow):
    /// `NodeParts { node, controls, fork: Some(unit.fork_source()) }`.
    ///
    /// Take it from the unit that goes into the graph, **before** it goes in
    /// and after its `program_change`: it keeps a template clone (see the
    /// `fork` module docs), and costs a second
    /// copy of the synthesizer's voice and effect state — not of the
    /// SoundFont — for as long as the node is in the graph.
    pub fn fork_source(&self) -> Box<dyn ForkSource> {
        Box::new(self.fork_template())
    }

    fn fork_template(&self) -> SoundFontFork {
        SoundFontFork {
            template: self.clone(),
            native: false,
        }
    }

    /// The fork source of a unit inserted as a graph node.
    pub(crate) fn native_fork(&self) -> SoundFontFork {
        SoundFontFork {
            template: self.clone(),
            native: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tutti_core::{AudioUnit, BufferVec, SampleRate, MAX_BUFFER_SIZE};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    use crate::{SoundFont, SoundFontUnit, SynthesizerSettings};

    /// The live unit's rate.
    const LIVE: SampleRate = SampleRate(48_000.0);

    /// The repo's committed test soundfont. Panics with the path when it is
    /// missing: a checkout without it is broken, not a reason to skip.
    fn soundfont() -> Arc<SoundFont> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../assets/soundfonts/TimGM6mb.sf2");
        let mut file = std::fs::File::open(&path).unwrap_or_else(|e| {
            panic!(
                "committed test soundfont missing at {}: {e}",
                path.display()
            )
        });
        Arc::new(SoundFont::new(&mut file).expect("the test soundfont parses"))
    }

    /// A unit at `rate`, on preset `preset`.
    fn unit(soundfont: &Arc<SoundFont>, rate: SampleRate, preset: i32) -> SoundFontUnit {
        let mut settings = SynthesizerSettings::new(rate.get() as i32);
        settings.enable_reverb_and_chorus = false;
        let mut unit =
            SoundFontUnit::new(Arc::clone(soundfont), &settings).expect("the unit builds");
        unit.program_change(0, preset);
        unit
    }

    fn note_on() -> MidiEvent {
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF)
    }

    /// Render `frames` of channel 0 in 64-frame blocks.
    fn render(unit: &mut dyn AudioUnit, frames: usize) -> Vec<f32> {
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        let mut out = Vec::with_capacity(frames);
        while out.len() < frames {
            let n = (frames - out.len()).min(MAX_BUFFER_SIZE);
            unit.process(n, &input.buffer_ref(), &mut output.buffer_mut());
            out.extend_from_slice(&output.buffer_ref().channel_f32(0)[..n]);
        }
        out
    }

    /// **A fork plays at the render's rate, on the live unit's preset, and
    /// shares the decoded SoundFont.** A 48 kHz unit on preset 24 (a guitar,
    /// not the default piano), forked for a render at 48 and at 96 kHz: a
    /// note renders sample for sample what a fresh unit built at the render's
    /// rate on that preset renders.
    ///
    /// Mutation (run): `RateFollowing::set_sample_rate` doing nothing → at
    /// 96 kHz the fork renders the 48 kHz unit's note, not the reference.
    /// Mutation (run): `Synthesizer::with_sample_rate` not copying `channels`
    /// → the 96 kHz fork plays the piano.
    #[test]
    fn a_fork_plays_at_the_render_rate_on_the_live_preset() {
        let font = soundfont();
        let live = unit(&font, LIVE, 24);
        let source = live.fork_template();

        for rate in [LIVE, SampleRate(96_000.0)] {
            let held = Arc::strong_count(&font);
            let mut fork = source.unit();
            assert_eq!(
                Arc::strong_count(&font),
                held + 1,
                "the fork shares the decoded SoundFont"
            );
            // What a graph does when it prepares the fork.
            fork.set_sample_rate(rate);
            fork.0.queue_midi(&[note_on()]);
            let out = render(&mut fork, 4_096);
            let reference = {
                let mut fresh = unit(&font, rate, 24);
                fresh.queue_midi(&[note_on()]);
                render(&mut fresh, 4_096)
            };
            assert!(reference.iter().any(|&s| s != 0.0), "the note sounds");
            assert_eq!(out, reference, "{rate:?}: the note at the render's rate");
        }
        assert_eq!(live.sample_rate(), LIVE, "the live unit keeps its rate");
    }
}
