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
use tutti_core::transport::TransportReader;

use crate::host::node::input_slot::{BlockCtx, BlockInput};
use crate::protocol::TransportInfo;

/// Beat-scheduled transport-info producer. Cheap to clone (reader + rate shared
/// via `Arc`).
#[derive(Clone)]
pub struct TransportSource {
    reader: Arc<dyn TransportReader>,
    sample_rate: Arc<AtomicF64>,
}

impl TransportSource {
    pub fn new(reader: Arc<dyn TransportReader>, sample_rate: f64) -> Self {
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
        let mut info = TransportInfo::new()
            .with_tempo(reader.tempo().get())
            .with_playing(reader.is_playing())
            .with_recording(reader.is_recording())
            .with_sample_rate(sample_rate);
        // CLAP-style beats position; seconds derived from beats + tempo.
        let beats = reader.current_beat_f64();
        let tempo = reader.tempo().get();
        let seconds = if tempo > 0.0 { beats * 60.0 / tempo } else { 0.0 };
        info = info.with_position_beats(beats, seconds);
        if let Some((start, end)) = reader.get_loop_range() {
            info = info.with_loop(reader.is_loop_enabled(), start, end);
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use tutti_core::params::Bpm;

    struct TestTransport {
        beat: AtomicF64,
        tempo: f64,
        playing: AtomicBool,
    }
    impl TransportReader for TestTransport {
        fn current_beat(&self) -> f64 {
            self.beat.load(Ordering::Acquire)
        }
        fn is_loop_enabled(&self) -> bool {
            false
        }
        fn get_loop_range(&self) -> Option<(f64, f64)> {
            None
        }
        fn is_playing(&self) -> bool {
            self.playing.load(Ordering::Acquire)
        }
        fn is_recording(&self) -> bool {
            false
        }
        fn is_in_preroll(&self) -> bool {
            false
        }
        fn tempo(&self) -> Bpm {
            Bpm(self.tempo)
        }
    }

    fn source(tempo: f64, rate: f64) -> (Arc<TestTransport>, TransportSource) {
        let t = Arc::new(TestTransport {
            beat: AtomicF64::new(0.0),
            tempo,
            playing: AtomicBool::new(true),
        });
        let src = TransportSource::new(Arc::clone(&t) as Arc<dyn TransportReader>, rate);
        (t, src)
    }

    const CTX: BlockCtx = BlockCtx { block_size: 64 };

    #[test]
    fn snapshot_reflects_transport() {
        let (t, src) = source(120.0, 44100.0);
        t.beat.store(2.0, Ordering::Release);
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
