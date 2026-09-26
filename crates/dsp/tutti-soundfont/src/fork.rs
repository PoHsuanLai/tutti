//! Forking a [`SoundFontUnit`] for the native graph's export
//! (`Editor::fork`, design doc 013 PR 12): a fresh unit over the same decoded
//! SoundFont, at the render's rate, playing the live unit's clip on the
//! render's timeline.
//!
//! # Why not a clone of the graph's shadow
//!
//! A host that inserts the unit through `tutti_graph::Legacy::controlled` gets
//! a fork source for free: a clone of the node's **shadow**, isolated when the
//! node was inserted. Its MIDI port was severed then (`MidiInPort::isolate`),
//! so it never saw the clip a host installs on the live port afterwards
//! (bevy-tutti's `MidiSourceInstall`), and the export rendered silence. And
//! it kept the live unit's rate whatever the export's was: a 96 kHz export of
//! a 48 kHz unit played every note an octave low, at half its frame.
//!
//! # What this forks from instead
//!
//! A **template**: a clone of the unit taken when the source is made, never
//! processed and deliberately **not** isolated, so it shares the live unit's
//! MIDI port (mailbox and source cell); it never polls it. Its synthesizer is
//! a clone too, and that clone shares the decoded SoundFont (`Arc<SoundFont>`,
//! sample data included): nothing is reloaded or copied but the per-channel
//! and voice state. Its preset is whatever `program_change` set before the
//! source was made. A fork is then, in order:
//!
//! 1. a clone of the template;
//! 2. `AudioUnit::isolate` — a fresh private MIDI port;
//! 3. **offline only:** the live port's source (a `MidiClipSource`) rebound
//!    onto the fork's own port and the render's timeline
//!    ([`MidiInPort::rebind_offline_into`](tutti_midi_runtime::MidiInPort::rebind_offline_into)).
//!    A source that cannot be rebound fails the fork ([`Error::MidiSource`])
//!    rather than render its notes as silence. A live duplicate
//!    ([`ForkMode::Live`]) carries no clip;
//! 4. `AudioUnit::reset` — every key released. The template never rendered,
//!    so it holds no voice to release, and the fork starts silent.
//!
//! **The forked node follows its graph's rate.** A live unit's rate is fixed
//! (see [`SoundFontUnit`]'s "The sample rate is fixed"); the node a fork
//! source hands the graph wraps its unit in [`RateFollowing`], whose
//! `set_sample_rate` — called when the fork is prepared at the render's rate
//! — swaps in [`SoundFontUnit::with_sample_rate`], keeping the preset and the
//! rebound clip (the port is shared by the copy). A rate RustySynth refuses
//! (outside 16–192 kHz) leaves the unit at its own, as a live unit stays.
//!
//! What the fork does **not** carry: the live mailbox, sounding voices, and
//! channel state the live unit reached through MIDI after the template was
//! taken (a program change, a CC) — the clip replays whatever of that it
//! holds.

use tutti_core::{AudioUnit, BufferMut, BufferRef, SampleRate, Setting, SignalFrame};
use tutti_graph::{ForkCause, ForkMode, ForkSource, Forked, IntoNode, Legacy};
use tutti_midi_runtime::OfflineRebind;

use crate::{Error, SoundFontUnit};

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
    fn unit(&self, mode: ForkMode<'_>) -> crate::Result<RateFollowing> {
        self.template.fork_instance(mode).map(RateFollowing)
    }
}

impl ForkSource for SoundFontFork {
    fn fork(&self, mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        if self.native {
            // A graph node follows its graph's rate itself (`Node::prepare`).
            let fork = self.template.fork_instance(mode).map_err(ForkCause::new)?;
            return Ok(Forked::new(Box::new(fork)));
        }
        let fork = self.unit(mode).map_err(ForkCause::new)?;
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
    /// SoundFont (shared, not reloaded), settings, rate and preset, a private
    /// MIDI port and no sounding voice, and — for [`ForkMode::Offline`] — the
    /// clip installed on this unit's port, rebound onto the render's
    /// timeline. See the `fork` module docs (`src/fork.rs`) for the steps and
    /// what is not carried; [`with_sample_rate`](Self::with_sample_rate)
    /// moves it to a render's rate.
    ///
    /// Control thread. Reads this unit's port's source cell, never its
    /// mailbox, so this unit keeps every event.
    ///
    /// # Errors
    ///
    /// [`Error::MidiSource`] when a source is installed on this unit's port
    /// that cannot be rebound for an offline render: the render would drop
    /// its notes.
    pub fn fork_instance(&self, mode: ForkMode<'_>) -> crate::Result<SoundFontUnit> {
        let mut fork = self.clone();
        fork.isolate();
        if let ForkMode::Offline(ctx) = mode {
            if self.midi_port().rebind_offline_into(fork.midi_port(), ctx)
                == OfflineRebind::NotRebindable
            {
                return Err(Error::MidiSource);
            }
        }
        fork.reset();
        Ok(fork)
    }

    /// The [`ForkSource`] a host hands the graph's editor when it inserts
    /// this unit, so that a fork of the graph (an export) forks it through
    /// [`fork_instance`](Self::fork_instance), at the fork's rate, and plays
    /// its clip.
    ///
    /// For a host that wraps the unit in its own node builder (bevy-tutti's
    /// `Legacy::controlled`, for a settings ring and a shadow):
    /// `NodeParts { node, controls, fork: Some(unit.fork_source()) }`.
    ///
    /// Take it from the unit that goes into the graph, **before** it goes in
    /// and after its `program_change`: it keeps a template clone that shares
    /// that unit's MIDI port (see the `fork` module docs), and costs a second
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

    use tutti_core::transport::{
        OfflineTimeline, OfflineTimelineConfig, OfflineTransport, Timeline, Transport,
    };
    use tutti_core::{AudioUnit, Beat, Bpm, BufferVec, SampleRate, MAX_BUFFER_SIZE};
    use tutti_graph::ForkMode;
    use tutti_midi_runtime::{MidiClipSource, MidiInPort, OfflineRebind, TimedClipEvent};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup, MidiUnitId, MidiUnitIn};

    use crate::{Error, SoundFont, SoundFontUnit, SynthesizerSettings};

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

    /// 90 BPM from beat 0 at `rate`: a beat is 32 000 frames at 48 kHz
    /// (64 000 at 96), a multiple of the unit's 8-frame resolution.
    fn offline(rate: SampleRate) -> Arc<OfflineTimeline> {
        Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(90.0),
            sample_rate: rate,
            loop_range: None,
        }))
    }

    /// Render `frames` of channel 0 in 64-frame blocks, advancing `timeline`
    /// past each block after it renders.
    fn render(unit: &mut dyn AudioUnit, timeline: &OfflineTimeline, frames: usize) -> Vec<f32> {
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        let mut out = Vec::with_capacity(frames);
        while out.len() < frames {
            let n = (frames - out.len()).min(MAX_BUFFER_SIZE);
            unit.process(n, &input.buffer_ref(), &mut output.buffer_mut());
            out.extend_from_slice(&output.buffer_ref().channel_f32(0)[..n]);
            timeline.advance(n);
        }
        out
    }

    /// **A fork plays the live unit's clip on the render's timeline, at the
    /// render's rate, on the live unit's preset, and leaves the live clip
    /// alone.** A 48 kHz unit on preset 24 (a guitar, not the default piano),
    /// forked for a render at 48 and at 96 kHz: silent until beat 1 (frame
    /// 32 000, or 64 000 at 96 kHz), then sample for sample what a fresh unit
    /// built at the render's rate on that preset renders for the note. The
    /// fork shares the decoded SoundFont.
    ///
    /// Mutation (run): dropping the `rebind_offline_into` call in
    /// `fork_instance` → the fork renders silence. Mutation (run):
    /// `fork_template` isolating its template → silence. Mutation (run):
    /// `RateFollowing::set_sample_rate` doing nothing → at 96 kHz the fork
    /// renders the 48 kHz unit's note, not the reference. Mutation (run):
    /// `Synthesizer::with_sample_rate` not copying `channels` → the 96 kHz
    /// fork plays the piano.
    #[test]
    fn a_fork_plays_the_live_clip_at_the_render_rate() {
        let font = soundfont();
        let live = unit(&font, LIVE, 24);
        let source = live.fork_template();
        live.midi_port().install(Arc::new(MidiClipSource::new(
            live.midi_unit_id(),
            vec![TimedClipEvent {
                beat: Beat(1.0),
                event: note_on(),
            }],
            Arc::new(Transport::new(LIVE.0)) as Arc<dyn Timeline>,
        )));

        for rate in [LIVE, SampleRate(96_000.0)] {
            let timeline = offline(rate);
            let ctx: OfflineTransport = OfflineTransport::new(timeline.clone());
            let held = Arc::strong_count(&font);
            let mut fork = source
                .unit(ForkMode::Offline(&ctx))
                .expect("the unit forks");
            assert_eq!(
                Arc::strong_count(&font),
                held + 1,
                "the fork shares the decoded SoundFont"
            );
            // What a graph does when it prepares the fork.
            fork.set_sample_rate(rate);

            let beat = (32_000.0 * rate.get() / LIVE.get()) as usize;
            let out = render(&mut fork, &timeline, beat + 4_096);
            let reference = {
                let mut fresh = unit(&font, rate, 24);
                fresh.midi_sender().queue(&[note_on()]);
                render(&mut fresh, &offline(rate), 4_096)
            };
            assert!(reference.iter().any(|&s| s != 0.0), "the note sounds");
            assert!(
                out[..beat].iter().all(|&s| s == 0.0),
                "{rate:?}: silent before beat 1"
            );
            assert_eq!(
                &out[beat..],
                &reference[..],
                "{rate:?}: from beat 1, the note at the render's rate"
            );
        }
        assert_eq!(
            live.midi_port()
                .rebind_offline_into(&MidiInPort::new(), &OfflineTransport::new(offline(LIVE))),
            OfflineRebind::Rebound,
            "the live unit still holds its clip"
        );
        assert_eq!(live.sample_rate(), LIVE, "the live unit keeps its rate");
    }

    /// A MIDI source that is not a function of a timeline.
    struct Unrebindable;

    impl MidiUnitIn for Unrebindable {
        fn poll_unit(
            &self,
            _unit: MidiUnitId,
            _block: usize,
            _rate: SampleRate,
            _buffer: &mut [MidiEvent],
        ) -> usize {
            0
        }
        fn rebind_offline(
            &self,
            _unit: MidiUnitId,
            _ctx: &tutti_core::transport::OfflineTransport,
        ) -> Option<Arc<dyn MidiUnitIn>> {
            None
        }
    }

    /// **A source that cannot be rebound fails the fork by name**, offline
    /// and through the `ForkSource`; a live duplicate does not ask.
    ///
    /// Mutation (run): `fork_instance` ignoring `NotRebindable` → the offline
    /// fork succeeds.
    #[test]
    fn an_unrebindable_source_is_a_named_fork_error() {
        let live = unit(&soundfont(), LIVE, 0);
        live.midi_port().install(Arc::new(Unrebindable));
        let ctx: OfflineTransport = OfflineTransport::new(offline(LIVE));
        assert!(matches!(
            live.fork_instance(ForkMode::Offline(&ctx)),
            Err(Error::MidiSource)
        ));
        let cause = live
            .fork_source()
            .fork(ForkMode::Offline(&ctx))
            .err()
            .expect("the source fails too");
        assert!(matches!(
            cause.downcast_ref::<Error>(),
            Some(Error::MidiSource)
        ));
        assert!(live.fork_instance(ForkMode::Live).is_ok());
    }
}
