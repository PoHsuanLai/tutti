//! Beat-scheduled chord / scale playback as a harmony producer.
//!
//! The VST3 counterpart of [`tutti_midi_runtime::MidiClipSource`]: a
//! [`HarmonySource`] holds sorted, beat-tagged [`ChordValue`] / [`ScaleValue`]
//! changes and a [`TransportReader`]. Each block it reads the transport beat,
//! computes the beat range the upcoming block covers, and fills a reused
//! [`HarmonyInputs`] with the changes whose beat falls in that range — their
//! `sample_offset` stamped to the sample-accurate position inside the block.
//!
//! Chord / scale are *stepwise context*, not discrete note events: a change
//! holds until the next one. So unlike MIDI we don't re-emit on every poll —
//! only when a change boundary actually falls in the block. (A plugin that
//! joins mid-clip and wants the *current* context can be re-primed by a seek;
//! priming-on-join is out of scope for this first cut, matching how the host
//! only feeds harmony "when a producer supplies it.")

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use atomic_float::AtomicF64;
use tutti_core::transport::TransportReader;

use crate::host::ipc_client::audio::HarmonyInputs;
use crate::protocol::{ChordValue, ScaleValue};

/// A chord change scheduled at an absolute beat. The `value`'s `sample_offset`
/// is filled per block by [`HarmonySource::fill`].
#[derive(Clone, Debug)]
pub struct TimedChord {
    pub beat: f64,
    pub value: ChordValue,
}

/// A scale change scheduled at an absolute beat.
#[derive(Clone, Debug)]
pub struct TimedScale {
    pub beat: f64,
    pub value: ScaleValue,
}

/// Beat-scheduled chord/scale producer. Cheap to clone (atomics shared) so the
/// fundsp graph-commit clone of the parent node doesn't restart playback.
#[derive(Clone)]
pub struct HarmonySource {
    chords: Arc<[TimedChord]>,
    scales: Arc<[TimedScale]>,
    transport: Arc<dyn TransportReader>,
    sample_rate: f64,
    chord_cursor: Arc<AtomicU64>,
    scale_cursor: Arc<AtomicU64>,
    last_beat: Arc<AtomicF64>,
}

impl HarmonySource {
    /// Build a harmony source. `chords` / `scales` need not be pre-sorted —
    /// the constructor sorts them ascending by beat.
    pub fn new(
        chords: impl IntoIterator<Item = TimedChord>,
        scales: impl IntoIterator<Item = TimedScale>,
        transport: Arc<dyn TransportReader>,
        sample_rate: f64,
    ) -> Self {
        let mut c: Vec<TimedChord> = chords.into_iter().collect();
        c.sort_by(|a, b| a.beat.partial_cmp(&b.beat).unwrap_or(std::cmp::Ordering::Equal));
        let mut s: Vec<TimedScale> = scales.into_iter().collect();
        s.sort_by(|a, b| a.beat.partial_cmp(&b.beat).unwrap_or(std::cmp::Ordering::Equal));
        Self {
            chords: c.into(),
            scales: s.into(),
            transport,
            sample_rate,
            chord_cursor: Arc::new(AtomicU64::new(0)),
            scale_cursor: Arc::new(AtomicU64::new(0)),
            last_beat: Arc::new(AtomicF64::new(f64::NEG_INFINITY)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.chords.is_empty() && self.scales.is_empty()
    }

    /// Reconcile against the live transport and compute this block's beat
    /// window, mirroring `MidiClipSource::sync_to_transport`. Returns `None`
    /// when nothing should be emitted (paused, or non-positive tempo / rate).
    fn window(&self, block_size: usize) -> Option<HarmonyWindow> {
        if !self.transport.is_playing() {
            self.last_beat
                .store(self.transport.current_beat(), Ordering::Release);
            return None;
        }
        let start_beat = self.transport.current_beat();
        let last_beat = self.last_beat.load(Ordering::Acquire);
        if start_beat + 1e-9 < last_beat {
            // Backward seek: rewind both cursors to the new position.
            self.rewind(&self.chord_cursor, self.chords.iter().map(|c| c.beat), start_beat);
            self.rewind(&self.scale_cursor, self.scales.iter().map(|s| s.beat), start_beat);
        }
        self.last_beat.store(start_beat, Ordering::Release);

        let tempo_bpm = self.transport.tempo().get();
        if tempo_bpm <= 0.0 || self.sample_rate <= 0.0 {
            return None;
        }
        let beats_per_sample = tempo_bpm / 60.0 / self.sample_rate;
        Some(HarmonyWindow {
            start_beat,
            end_beat: start_beat + (block_size as f64) * beats_per_sample,
            beats_per_sample,
            max_offset: (block_size.saturating_sub(1)) as i32,
        })
    }

    fn rewind(&self, cursor: &AtomicU64, beats: impl Iterator<Item = f64>, beat: f64) {
        let mut idx = 0usize;
        for b in beats {
            if b < beat {
                idx += 1;
            } else {
                break;
            }
        }
        cursor.store(idx as u64, Ordering::Release);
    }

    /// Fill `out` (cleared first) with the chord/scale changes whose beat falls
    /// in this block's window. Returns the number of (chord + scale) changes
    /// written. RT-safe: no allocation beyond the `SmallVec` push, which only
    /// grows past inline capacity if many changes land in one block.
    pub fn fill(&self, block_size: usize, out: &mut HarmonyInputs) {
        out.chords.changes.clear();
        out.scales.changes.clear();
        if block_size == 0 {
            return;
        }
        let Some(window) = self.window(block_size) else {
            return;
        };
        Self::emit(
            &self.chords,
            &self.chord_cursor,
            &window,
            |tc, off| {
                let mut v = tc.value.clone();
                v.sample_offset = off;
                out.chords.changes.push(v);
            },
        );
        Self::emit(
            &self.scales,
            &self.scale_cursor,
            &window,
            |ts, off| {
                let mut v = ts.value.clone();
                v.sample_offset = off;
                out.scales.changes.push(v);
            },
        );
    }

    /// Walk a sorted change list, emitting every entry in the window with a
    /// sample-accurate offset; advances the persisted cursor past what it
    /// emitted. Generic over chord/scale via the `beat_of` + `push` closures.
    fn emit<T: HasBeat>(
        items: &[T],
        cursor: &AtomicU64,
        window: &HarmonyWindow,
        mut push: impl FnMut(&T, i32),
    ) {
        let mut idx = cursor.load(Ordering::Relaxed) as usize;
        while idx < items.len() && items[idx].beat() < window.start_beat {
            idx += 1;
        }
        while idx < items.len() && items[idx].beat() < window.end_beat {
            let beat_delta = (items[idx].beat() - window.start_beat).max(0.0);
            let off = ((beat_delta / window.beats_per_sample) as i32).min(window.max_offset);
            push(&items[idx], off);
            idx += 1;
        }
        cursor.store(idx as u64, Ordering::Release);
    }
}

/// Beat range one audio block covers + conversion factors. Mirrors
/// `MidiClipSource`'s `PollWindow`.
struct HarmonyWindow {
    start_beat: f64,
    end_beat: f64,
    beats_per_sample: f64,
    max_offset: i32,
}

/// Lets [`HarmonySource::emit`] read the beat of either change kind.
trait HasBeat {
    fn beat(&self) -> f64;
}
impl HasBeat for TimedChord {
    fn beat(&self) -> f64 {
        self.beat
    }
}
impl HasBeat for TimedScale {
    fn beat(&self) -> f64 {
        self.beat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tutti_core::params::Bpm;

    struct TestTransport {
        beat: AtomicF64,
        tempo: f64,
        playing: AtomicBool,
    }
    impl TestTransport {
        fn new(tempo: f64) -> Self {
            Self {
                beat: AtomicF64::new(0.0),
                tempo,
                playing: AtomicBool::new(true),
            }
        }
        fn set_beat(&self, b: f64) {
            self.beat.store(b, Ordering::Release);
        }
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

    fn chord(beat: f64, root: i16, name: &str) -> TimedChord {
        TimedChord {
            beat,
            value: ChordValue {
                sample_offset: 0,
                root,
                bass_note: root,
                mask: 0,
                text: name.to_string(),
            },
        }
    }
    fn scale(beat: f64, root: i16, name: &str) -> TimedScale {
        TimedScale {
            beat,
            value: ScaleValue {
                sample_offset: 0,
                root,
                mask: 0,
                text: name.to_string(),
            },
        }
    }

    #[test]
    fn emits_change_in_window_with_offset() {
        let transport = Arc::new(TestTransport::new(120.0)); // 22050 samples/beat @ 44.1k
        let src = HarmonySource::new(
            vec![chord(0.0, 0, "C"), chord(0.5, 7, "G")],
            vec![scale(0.0, 0, "C major")],
            Arc::clone(&transport) as Arc<dyn TransportReader>,
            44100.0,
        );
        let mut out = HarmonyInputs::default();
        // Block covering [0.0, 1.0) beats = [0, 22050) samples → both chords.
        src.fill(22050, &mut out);
        assert_eq!(out.chords.changes.len(), 2);
        assert_eq!(out.scales.changes.len(), 1);
        assert_eq!(out.chords.changes[0].sample_offset, 0); // beat 0.0
        // beat 0.5 → ~11025 samples.
        assert!((out.chords.changes[1].sample_offset - 11025).abs() < 4);
    }

    #[test]
    fn no_changes_after_window_passes() {
        let transport = Arc::new(TestTransport::new(120.0));
        let src = HarmonySource::new(
            vec![chord(0.0, 0, "C")],
            Vec::new(),
            Arc::clone(&transport) as Arc<dyn TransportReader>,
            44100.0,
        );
        let mut out = HarmonyInputs::default();
        src.fill(22050, &mut out);
        assert_eq!(out.chords.changes.len(), 1);
        // Advance the transport past the only change — nothing more.
        transport.set_beat(2.0);
        src.fill(22050, &mut out);
        assert_eq!(out.chords.changes.len(), 0);
    }

    #[test]
    fn paused_emits_nothing() {
        let transport = Arc::new(TestTransport::new(120.0));
        transport.playing.store(false, Ordering::Release);
        let src = HarmonySource::new(
            vec![chord(0.0, 0, "C")],
            Vec::new(),
            Arc::clone(&transport) as Arc<dyn TransportReader>,
            44100.0,
        );
        let mut out = HarmonyInputs::default();
        src.fill(512, &mut out);
        assert!(out.chords.changes.is_empty());
    }

    #[test]
    fn backward_seek_replays() {
        let transport = Arc::new(TestTransport::new(120.0));
        let src = HarmonySource::new(
            vec![chord(0.0, 0, "C"), chord(0.25, 7, "G")],
            Vec::new(),
            Arc::clone(&transport) as Arc<dyn TransportReader>,
            44100.0,
        );
        let mut out = HarmonyInputs::default();
        src.fill(22050, &mut out); // both
        assert_eq!(out.chords.changes.len(), 2);
        transport.set_beat(2.0);
        src.fill(22050, &mut out); // none
        assert_eq!(out.chords.changes.len(), 0);
        transport.set_beat(0.0); // seek back
        src.fill(22050, &mut out);
        assert_eq!(out.chords.changes.len(), 2);
    }
}
