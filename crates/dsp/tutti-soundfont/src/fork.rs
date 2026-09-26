//! Forking a [`SoundFontUnit`] for the graph's export (`Editor::fork`,
//! design doc 013): a fresh unit over the same decoded SoundFont, at the
//! render's rate. Its MIDI comes from its event input, as the live unit's
//! does: the fork of the graph forks the clip node feeding it too (doc 013,
//! rewrite item 5).
//!
//! # What this forks from
//!
//! A **template**: a clone of the unit taken when it is inserted, never
//! processed. Its synthesizer is a clone too, and that clone shares the
//! decoded SoundFont (`Arc<SoundFont>`, sample data included): nothing is
//! reloaded or copied but the per-channel and voice state. Its preset is
//! whatever `program_change` set before the unit went in. A fork is
//! [`fork_instance`](SoundFontUnit::fork_instance) of the template: a clone
//! with every key released. The template never rendered, so it holds no
//! voice to release, and the fork starts silent.
//!
//! **The fork follows its graph's rate**, as the live node does: the editor
//! prepares it at the render's rate, and the node's `prepare` swaps in
//! [`SoundFontUnit::with_sample_rate`], keeping the preset. (Under `Legacy`
//! this took a `RateFollowing` wrapper whose `set_sample_rate` re-rated; a
//! native node's `prepare` runs on the control thread, where rebuilding is
//! allowed, so the node does it itself.)
//!
//! What the fork does **not** carry: sounding voices, and channel state the
//! live unit reached through MIDI after the template was taken (a program
//! change, a CC) — the clip replays whatever of that it holds.

use tutti_graph::{ForkCause, ForkMode, ForkSource, Forked};

use crate::SoundFontUnit;

/// The fork source a [`SoundFontUnit`] is inserted with: the template in the
/// module docs.
pub(crate) struct SoundFontFork {
    template: SoundFontUnit,
}

impl ForkSource for SoundFontFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(self.template.fork_instance())))
    }
}

impl SoundFontUnit {
    /// A fresh unit for a fork of the graph this one plays in: the same
    /// SoundFont (shared, not reloaded), settings, rate and preset, and no
    /// sounding voice. See the `fork` module docs (`src/fork.rs`); its graph
    /// re-rates it when it is prepared.
    pub fn fork_instance(&self) -> SoundFontUnit {
        let mut fork = self.clone();
        fork.release_all();
        fork
    }

    /// The fork source of a unit inserted as a graph node: a template
    /// clone, costing a second copy of the synthesizer's voice and effect
    /// state — not of the SoundFont — for as long as the node is in the
    /// graph.
    pub(crate) fn fork_template(&self) -> SoundFontFork {
        SoundFontFork {
            template: self.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tutti_core::{SampleRate, Samples};
    use tutti_graph::contract::drive_in;
    use tutti_graph::{
        Env, Event, ForkMode, ForkSource, Node, Offset, Prepare, Transport, TransportChanges,
    };
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

    /// Render `frames` of channel 0 of `node`, prepared at `rate` (as a
    /// graph prepares a fork at its render's), in 64-frame blocks, a note-on
    /// at frame 0.
    fn render(mut node: Box<dyn Node>, rate: SampleRate, frames: usize) -> Vec<f32> {
        node.prepare(&Prepare::new(rate, Samples(64)));
        let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF);
        let at = Offset::new(0, Samples(64)).expect("inside");
        let first = [Event::midi(at, note.data)];
        let mut out = Vec::with_capacity(frames);
        let mut frame = 0u64;
        while out.len() < frames {
            let env = Env {
                frame: tutti_core::Frame(frame),
                sample_rate: rate,
                block_len: Samples(64),
                transport: Transport::default(),
                changes: TransportChanges::NONE,
            };
            let events: &[Event] = if frame == 0 { &first } else { &[] };
            out.extend(
                drive_in(&mut *node, &env, &[], &[], &[events])
                    .audio
                    .swap_remove(0),
            );
            frame += 64;
        }
        out.truncate(frames);
        out
    }

    /// **A fork plays at the render's rate, on the live unit's preset, and
    /// shares the decoded SoundFont.** A 48 kHz unit on preset 24 (a guitar,
    /// not the default piano), forked for a render at 48 and at 96 kHz: a
    /// note renders sample for sample what a fresh unit built at the render's
    /// rate on that preset renders.
    ///
    /// Mutation (run): the node's `prepare` not re-rating → at 96 kHz the
    /// fork renders the 48 kHz unit's note, not the reference. Mutation
    /// (run): `Synthesizer::with_sample_rate` not copying `channels` → the
    /// 96 kHz fork plays the piano.
    #[test]
    fn a_fork_plays_at_the_render_rate_on_the_live_preset() {
        let font = soundfont();
        let live = unit(&font, LIVE, 24);
        let source = live.fork_template();

        for rate in [LIVE, SampleRate(96_000.0)] {
            let held = Arc::strong_count(&font);
            let fork = source.fork(ForkMode::Live).expect("forks").node;
            assert_eq!(
                Arc::strong_count(&font),
                held + 1,
                "the fork shares the decoded SoundFont"
            );
            let out = render(fork, rate, 4_096);
            let reference = render(Box::new(unit(&font, rate, 24)), rate, 4_096);
            assert!(reference.iter().any(|&s| s != 0.0), "the note sounds");
            assert_eq!(out, reference, "{rate:?}: the note at the render's rate");
        }
        assert_eq!(live.sample_rate(), LIVE, "the live unit keeps its rate");
    }

    /// **A fork releases every key**: a note held on the unit when it went
    /// in (a `note_on` before insert, so the template carries the voice)
    /// is released in the fork, and dies away there, while the live unit
    /// holds it. An organ preset (16), which sustains while its key is down,
    /// tells the two apart.
    ///
    /// Mutation (run): `fork_instance` not calling `release_all` → the fork
    /// sustains the held organ note → fails.
    #[test]
    fn a_fork_releases_every_held_key() {
        let font = soundfont();
        let mut live = unit(&font, LIVE, 16);
        live.note_on(0, 60, 100);
        let fork = live
            .fork_template()
            .fork(ForkMode::Live)
            .expect("forks")
            .node;
        // No new events: the tail of each render, two seconds on.
        let tail = |out: &[f32]| {
            let t = &out[out.len() - 4_096..];
            (t.iter().map(|s| s * s).sum::<f32>() / t.len() as f32).sqrt()
        };
        let held = tail(&render_quiet(Box::new(live), LIVE, 96_000));
        let forked = tail(&render_quiet(fork, LIVE, 96_000));
        assert!(held > 1e-3, "the live organ note sustains ({held})");
        assert!(
            forked < held * 0.01,
            "the fork released it ({forked} vs {held})"
        );
    }

    /// `render` with no events at all.
    fn render_quiet(mut node: Box<dyn Node>, rate: SampleRate, frames: usize) -> Vec<f32> {
        node.prepare(&Prepare::new(rate, Samples(64)));
        let mut out = Vec::with_capacity(frames);
        let mut frame = 0u64;
        while out.len() < frames {
            let env = Env {
                frame: tutti_core::Frame(frame),
                sample_rate: rate,
                block_len: Samples(64),
                transport: Transport::default(),
                changes: TransportChanges::NONE,
            };
            out.extend(
                drive_in(&mut *node, &env, &[], &[], &[])
                    .audio
                    .swap_remove(0),
            );
            frame += 64;
        }
        out
    }
}
