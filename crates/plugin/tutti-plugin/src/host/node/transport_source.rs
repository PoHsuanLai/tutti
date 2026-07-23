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

use atomic_float::AtomicF64;
use std::sync::atomic::Ordering;
use tutti_core::transport::Transport;

use crate::host::node::input_slot::{BlockCtx, BlockInput};
use crate::protocol::TransportInfo;

/// Beat-scheduled transport-info producer for the plugin ABI.
///
/// Takes the live [`Transport`] concretely rather than a
/// [`Timeline`](tutti_core::transport::Timeline): the `TransportInfo` it fills
/// for hosted plugins carries `recording` and loop-armed state, which are
/// live-session facts outside a timeline's vocabulary. Cheap to clone (all
/// state shared via `Arc`).
#[derive(Clone)]
pub struct TransportSource {
    reader: Transport,
    sample_rate: Arc<AtomicF64>,
}

impl TransportSource {
    pub fn new(reader: Transport, sample_rate: f64) -> Self {
        Self {
            reader,
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
        let tempo = reader.settings.tempo().get();
        let mut info = TransportInfo::new()
            .with_tempo(tempo)
            .with_playing(reader.motion.is_playing())
            .with_recording(reader.settings.is_recording())
            .with_sample_rate(sample_rate);
        // CLAP-style beats position; seconds derived from beats + tempo.
        let beats = reader.settings.beat();
        let seconds = if tempo > 0.0 {
            beats * 60.0 / tempo
        } else {
            0.0
        };
        info = info.with_position_beats(beats, seconds);
        let span = &reader.settings.loop_span;
        if let Some((start, end)) = span.range() {
            info = info.with_loop(true, start, end);
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

    /// Drive the real `Transport` — TransportSource is a live-only ABI
    /// bridge, so a mock would only restate its fields.
    fn source(tempo: f64, rate: f64) -> (Transport, TransportSource) {
        let t = Transport::new(rate);
        t.settings.set_tempo(tempo);
        let _ = t.motion.try_send(tutti_core::MotionEvent::Play);
        t.motion.drain();
        let src = TransportSource::new(t.clone(), rate);
        (t, src)
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
}
