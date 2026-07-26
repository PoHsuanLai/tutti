//! Live transport as a per-block [`BlockInput`].
//!
//! Reads the project transport each block and produces the [`TransportInfo`]
//! snapshot (tempo, playhead, loop) the plugin consumes — the transport
//! counterpart of [`super::harmony_source::HarmonySource`] /
//! [`super::param_automation_source::ParamAutomationSource`]. Unlike those two,
//! it computes a fresh value rather than accumulating into scratch, so its
//! `fill` overwrites `out` wholesale.
//!
//! The `sample_rate` stamped onto the snapshot (CLAP reads it) can change after
//! the source is installed (device / rate switch), so it lives in an
//! `Arc<AtomicF64>` the running box reads live — the reader/rate are installed
//! once at plugin-add and never rebuilt, unlike harmony/params which are rebuilt
//! on every clip/automation edit (so a plain `f64` self-heals there).

use std::sync::Arc;

use arc_swap::ArcSwap;
use atomic_float::AtomicF64;
use std::sync::atomic::Ordering;
use tutti_core::meter::{Meter, MeterMap};
use tutti_core::transport::TransportState;

use crate::host::node::input_slot::{BlockCtx, BlockInput};
use crate::protocol::TransportInfo;

/// Beat-scheduled transport-info producer for the plugin ABI.
///
/// Takes a [`TransportState`](tutti_core::transport::TransportState) rather than
/// a bare [`Timeline`](tutti_core::transport::Timeline): the `TransportInfo` it
/// fills for hosted plugins carries `recording` and loop state, which are
/// live-session facts on the `TransportState` supertrait and outside a plain
/// timeline's vocabulary. An offline render — a `Timeline`-only implementor —
/// correctly cannot be plugged here. Cheap to clone (all state shared via
/// `Arc`).
#[derive(Clone)]
pub struct TransportSource {
    reader: Arc<dyn TransportState>,
    /// The project meter, for the time-signature and bar fields.
    ///
    /// A **separate** handle from `reader`, deliberately not a method on the
    /// `TransportState` trait: meter is a layer over the timeline, not transport
    /// state. An offline render and a live transport share one meter, and the
    /// transport does not change when the meter does. Shared like `sample_rate`
    /// so an edit reaches the running box without a re-install.
    meter: Arc<ArcSwap<MeterMap>>,
    sample_rate: Arc<AtomicF64>,
}

impl TransportSource {
    pub fn new(
        reader: Arc<dyn TransportState>,
        meter: Arc<ArcSwap<MeterMap>>,
        sample_rate: f64,
    ) -> Self {
        Self {
            reader,
            meter,
            sample_rate: Arc::new(AtomicF64::new(sample_rate)),
        }
    }

    /// Update the stamped sample rate live (device / rate switch). Reaches the
    /// running box because the atomic is shared across clones.
    pub fn set_sample_rate(&self, sample_rate: f64) {
        self.sample_rate.store(sample_rate, Ordering::Release);
    }

    /// Read the live transport into `out`, overwriting it. Mirrors the former
    /// `Transport::snapshot`.
    fn snapshot_into(&self, out: &mut TransportInfo) {
        let sample_rate = self.sample_rate.load(Ordering::Acquire);
        let reader = &self.reader;
        let tempo = reader.tempo().get();
        let beat = reader.beat();

        // One meter read per block — this runs in `fill`, not per sample.
        let meter = self.meter.load();
        let position = meter.bar_at(beat);

        let mut info = TransportInfo::new()
            .with_tempo(tempo)
            .with_playing(reader.is_rolling())
            .with_recording(reader.is_recording())
            .with_time_signature(position.signature)
            .with_bar(position.bar_start.get(), position.bar)
            .with_sample_rate(sample_rate);

        // CLAP-style beats position; seconds derived from beats + tempo.
        let beats = beat.get();
        let seconds = if tempo > 0.0 {
            beats * 60.0 / tempo
        } else {
            0.0
        };
        info = info.with_position_beats(beats, seconds);

        // `beat()` is already quarter notes, which is exactly what VST2's
        // `ppqPos` and VST3's `projectTimeMusic` want. These were left at zero
        // before, so every VST plugin saw a playhead frozen at the song start
        // while CLAP (which reads `position.beats`) tracked correctly.
        //
        // `samples` is project time, which jumps on a loop or seek; the clock's
        // free-running counter is the *continuous* one, so they go to different
        // fields. Deriving project-time samples from beats and tempo would be
        // wrong the moment tempo moves, so it stays `None` — the type now says
        // so, and each format host decides what to do with the absence rather
        // than forwarding a placeholder 0 as fact.
        info = info
            .with_position_quarters(beats)
            .with_continuous_samples(reader.steady_time());

        if let Some(region) = reader.loop_range() {
            info = info.with_loop(true, region.start().get(), region.end().get());
        }
        *out = info;
    }
}

impl BlockInput for TransportSource {
    type Out = TransportInfo;
    fn fill(&self, _ctx: BlockCtx, out: &mut TransportInfo) {
        self.snapshot_into(out);
    }
}

// `BlockReset for TransportInfo` lives next to its definition's consumers; a
// reset is a full default snapshot (the "no transport installed" state).
impl crate::host::node::input_slot::BlockReset for TransportInfo {
    fn reset(&mut self) {
        *self = TransportInfo::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::node::input_slot::BlockCtx;
    use tutti_core::meter::{BarNumber, BeatsPerBar, MeterChange, NoteValue, TimeSignature};
    use tutti_core::params::Beat;
    use tutti_core::transport::Transport;

    /// Drive the real `Transport` — TransportSource is a live-only ABI
    /// bridge, so a mock would only restate its fields.
    fn source(tempo: f64, rate: f64) -> (Transport, TransportSource) {
        let (t, src, _) = source_with_meter(tempo, rate, MeterMap::default());
        (t, src)
    }

    /// As [`source`], but keeps the meter handle so a test can republish it.
    fn source_with_meter(
        tempo: f64,
        rate: f64,
        meter: MeterMap,
    ) -> (Transport, TransportSource, Arc<ArcSwap<MeterMap>>) {
        let t = Transport::new(rate);
        t.settings.set_tempo(tempo);
        let _ = t.motion.try_send(tutti_core::MotionEvent::Play);
        t.motion.drain();
        let meter = Arc::new(ArcSwap::from_pointee(meter));
        let src = TransportSource::new(Arc::new(t.clone()), Arc::clone(&meter), rate);
        (t, src, meter)
    }

    const CTX: BlockCtx = BlockCtx { block_size: 64 };

    #[test]
    fn snapshot_reflects_transport() {
        let (t, src) = source(120.0, 44100.0);
        t.settings.set_beat(2.0);
        let mut out = TransportInfo::default();
        src.fill(CTX, &mut out);
        assert!((out.timing.tempo - 120.0).abs() < 1e-9);
        assert!(out.state.playing);
        assert!((out.sample_rate - 44100.0).abs() < 1e-9);
        // seconds = beats * 60 / tempo = 2 * 60 / 120 = 1.0
        assert!((out.position.seconds - 1.0).abs() < 1e-6);
    }

    #[test]
    fn sample_rate_change_is_live() {
        let (_t, src) = source(120.0, 44100.0);
        let mut out = TransportInfo::default();
        src.fill(CTX, &mut out);
        assert!((out.sample_rate - 44100.0).abs() < 1e-9);
        // Change the rate after "install" — the shared atomic reaches fill().
        src.set_sample_rate(48000.0);
        src.fill(CTX, &mut out);
        assert!((out.sample_rate - 48000.0).abs() < 1e-9);
    }

    /// The signature and bar fields used to be left at their 4/4 / zero
    /// defaults, so every hosted plugin was told the song was in 4/4 at bar 0.
    #[test]
    fn snapshot_carries_meter_and_bar() {
        let seven_eight = TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH);
        let (t, src, _) = source_with_meter(
            120.0,
            44100.0,
            MeterMap::new([MeterChange::new(Beat(0.0), seven_eight)]),
        );

        // Bar 2 of 7/8 starts at 3.5 quarter notes, not 7.
        t.settings.set_beat(3.5);
        let mut out = TransportInfo::default();
        src.fill(CTX, &mut out);

        assert_eq!(out.timing.signature, seven_eight);
        assert_eq!(out.bar.number, BarNumber(2));
        assert!((out.bar.start_beats - 3.5).abs() < 1e-9);
        // VST2's `bar_start_pos` / VST3's `barPositionMusic` read this one; it
        // was previously always zero because `with_bar` never set it.
        assert!((out.bar.position_quarters - 3.5).abs() < 1e-9);
        // And the VST playhead, likewise previously frozen at zero.
        assert!((out.position.quarters - 3.5).abs() < 1e-9);
    }

    /// The meter handle is shared, so an edit reaches an already-installed
    /// source without re-installing it — same contract as `sample_rate`.
    #[test]
    fn meter_change_is_live() {
        let (t, src, meter) = source_with_meter(120.0, 44100.0, MeterMap::default());
        t.settings.set_beat(0.0);

        let mut out = TransportInfo::default();
        src.fill(CTX, &mut out);
        assert_eq!(out.timing.signature, TimeSignature::default());

        let three_four = TimeSignature::new(BeatsPerBar::new(3), NoteValue::QUARTER);
        meter.store(Arc::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            three_four,
        )])));
        src.fill(CTX, &mut out);
        assert_eq!(out.timing.signature, three_four);
    }
}
