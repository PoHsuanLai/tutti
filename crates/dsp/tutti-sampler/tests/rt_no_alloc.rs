//! Regression gate: sampler hot paths must not allocate per-buffer.
//!
//! Covers in-memory `SamplerUnit::process` + `tick` (the workhorse
//! playback unit).
//!
//! `StreamingSamplerUnit` is not covered here — it requires a real
//! `RegionReader` from the butler. Its non-alloc safety is guarded by
//! the streaming-buffer regression tests in `tutti-sampler/butler`
//! instead.
//!
//! `TimeStretchUnit` (`stretch::Unit`) is covered here too: its
//! fixed-capacity `RtScratch` scratch buffers make `process` non-allocating
//! for any block size up to the preallocated maximum.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use assert_no_alloc::AllocDisabler;
use tutti_core::{
    AudioUnit, BeatPosition, Bpm, BufferVec, Cents, Ratio, SampleRate, TransportReader, Wave,
};
use tutti_sampler::stretch::{Algorithm, Unit as TimeStretchUnit};
use tutti_sampler::{ClipCommand, ClipSpec, Direction, SamplerUnit, SlotId, TrackClipReaderUnit};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Build a stereo `Wave` of `duration_secs` filled with a 440 Hz sine.
fn sine_wave(duration_secs: f64, sample_rate: f64) -> Arc<Wave> {
    let mut wave = Wave::zero(2, sample_rate, duration_secs);
    let len = wave.len();
    for i in 0..len {
        let t = i as f64 / sample_rate;
        let s = (t * 440.0 * core::f64::consts::TAU).sin() as f32 * 0.5;
        wave.set(0, i, s);
        wave.set(1, i, s);
    }
    Arc::new(wave)
}

#[test]
fn sampler_unit_process_is_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);
    let mut node = SamplerUnit::new(wave);
    node.set_sample_rate(SampleRate(48_000.0));

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    // Warm-up — drains the position update branch.
    for _ in 0..16 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

#[test]
fn sampler_unit_tick_is_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);
    let mut node = SamplerUnit::new(wave);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut output = [0.0f32; 2];
    for _ in 0..256 {
        node.tick(&[], &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            node.tick(&[], &mut output);
        }
    });
}

#[test]
fn time_stretch_process_is_allocation_free() {
    // The stretcher is now a pure filter: it owns no source and reads the stereo
    // frames the caller feeds in. Feed a 2-channel input buffer (matching its
    // `inputs() == 2`) primed with a constant source signal.
    let mut node = TimeStretchUnit::new(48_000.0);
    node.set_sample_rate(SampleRate(48_000.0));
    node.set_stretch_factor(Ratio::new(1.5));
    assert_eq!(node.algorithm(), Algorithm::PhaseVocoder);

    let mut input_vec = BufferVec::new(2);
    for i in 0..64 {
        input_vec.buffer_mut().set_f32(0, i, 0.25);
        input_vec.buffer_mut().set_f32(1, i, 0.25);
    }
    let mut output_vec = BufferVec::new(2);

    // Warm up past the phase-vocoder fill-up latency so `process` is on its
    // steady-state path inside the guarded loop.
    for _ in 0..64 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

// ---------------------------------------------------------------------------
// TrackClipReaderUnit — the live-graph workhorse. One node per track, mixing
// down every clip's `SamplerUnit` each buffer. Mirrors the mock transport from
// the crate's in-file tests so clips see a running playhead.
// ---------------------------------------------------------------------------

struct MockTransport {
    playing: AtomicBool,
    beat: AtomicU64,
    tempo: AtomicU64,
}

impl MockTransport {
    fn new(tempo: f64, beat: f64, playing: bool) -> Arc<Self> {
        Arc::new(Self {
            playing: AtomicBool::new(playing),
            beat: AtomicU64::new(beat.to_bits()),
            tempo: AtomicU64::new(tempo.to_bits()),
        })
    }
}

impl TransportReader for MockTransport {
    fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }
    fn current_beat(&self) -> f64 {
        f64::from_bits(self.beat.load(Ordering::Relaxed))
    }
    fn tempo(&self) -> Bpm {
        Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
    }
    fn is_loop_enabled(&self) -> bool {
        false
    }
    fn get_loop_range(&self) -> Option<(f64, f64)> {
        None
    }
    fn is_recording(&self) -> bool {
        false
    }
    fn is_in_preroll(&self) -> bool {
        false
    }
}

/// Steady-state mixdown must be allocation-free: two clips already present in
/// the slot list, the per-buffer `process()` sums them with no heap traffic.
///
/// Clips are seeded via `insert_clip` (the synchronous, channel-less Populate
/// path) so the guarded loop measures ONLY the per-buffer mixdown — no `Add`
/// command drain, which is a separate concern below.
#[test]
fn track_clip_reader_process_steady_state_is_allocation_free() {
    let transport = MockTransport::new(120.0, 0.0, true);
    let wave = sine_wave(2.0, 48_000.0);

    let (mut unit, _handle) = TrackClipReaderUnit::with_transport(transport.clone(), None);
    unit.set_sample_rate(SampleRate(48_000.0));

    for i in 0..2u128 {
        let sampler = SamplerUnit::with_transport(
            wave.clone(),
            transport.clone(),
            BeatPosition::new(0.0),
            None,
        );
        unit.insert_clip(ClipSpec {
            id: SlotId(i),
            sampler,
            direction: Direction::Forward,
            stretch_factor: Ratio::new(1.0),
            pitch_cents: Cents::new(0.0),
        });
    }
    assert_eq!(unit.clip_count(), 2);

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    // Warm-up — the empty command drain + first position reads.
    for _ in 0..16 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        unit.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            unit.process(64, &input, &mut output);
        }
    });
}

/// Same steady-state guard for the sample-accurate `tick()` mixdown path.
#[test]
fn track_clip_reader_tick_steady_state_is_allocation_free() {
    let transport = MockTransport::new(120.0, 0.0, true);
    let wave = sine_wave(2.0, 48_000.0);

    let (mut unit, _handle) = TrackClipReaderUnit::with_transport(transport.clone(), None);
    unit.set_sample_rate(SampleRate(48_000.0));

    for i in 0..2u128 {
        let sampler = SamplerUnit::with_transport(
            wave.clone(),
            transport.clone(),
            BeatPosition::new(0.0),
            None,
        );
        unit.insert_clip(ClipSpec {
            id: SlotId(i),
            sampler,
            direction: Direction::Forward,
            stretch_factor: Ratio::new(1.0),
            pitch_cents: Cents::new(0.0),
        });
    }

    let mut output = [0.0f32; 2];
    for _ in 0..256 {
        unit.tick(&[], &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            unit.tick(&[], &mut output);
        }
    });
}

/// KNOWN ALLOCATION — expected to FAIL today; Wave 5e fixes it.
///
/// Draining a `ClipCommand::UpdateStretch` on the audio thread calls
/// `ClipSlot::set_stretch` → `rebuild_stretch`, which `Box::new`s a cloned
/// `SamplerUnit` and builds a fresh `stretch::Unit` (heap-allocating its
/// scratch buffers). That command-drain happens inside `tick`/`process`, so
/// the per-buffer hot path allocates whenever a stretch update lands. Wave 5e
/// moves the rebuild off the audio thread; until then this path allocates.
///
/// The `AllocDisabler` global allocator *aborts* the process (SIGABRT via
/// `handle_alloc_error`) on a violation — it cannot be caught with
/// `catch_unwind`. Running the allocating body inline would take the whole
/// test binary down with it, hiding every other test's result. So the actual
/// `assert_no_alloc`-guarded drain runs in a re-exec'd CHILD of this test
/// binary (`RT_NO_ALLOC_STRETCH_CHILD=1`), and the parent asserts on the
/// child's exit status. TODAY the child aborts → this test FAILS, as intended.
/// After Wave 5e the child exits 0 → this test turns GREEN, with no code change
/// here.
#[test]
fn track_clip_reader_update_stretch_drain_allocates_known_wave5e() {
    // Child arm: run the guarded, allocating drain. Aborts today.
    if std::env::var_os("RT_NO_ALLOC_STRETCH_CHILD").is_some() {
        run_stretch_drain_under_guard();
        // Reached only if the guard did NOT abort (i.e. Wave 5e landed).
        std::process::exit(0);
    }

    // Parent arm: re-exec ourselves running only this one test, in the child
    // role, and inspect how it exited.
    let exe = std::env::current_exe().expect("current_exe");
    let status = std::process::Command::new(exe)
        .arg("--exact")
        .arg("track_clip_reader_update_stretch_drain_allocates_known_wave5e")
        .arg("--nocapture")
        .env("RT_NO_ALLOC_STRETCH_CHILD", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn child test process");

    assert!(
        status.success(),
        "UpdateStretch drain allocated on the audio thread (child exited {status}). \
         Expected to FAIL until Wave 5e moves the stretch rebuild off-thread."
    );
}

/// The body that runs inside the re-exec'd child: enqueue an `UpdateStretch`
/// and drain it inside `assert_no_alloc`. Aborts today via `AllocDisabler`.
fn run_stretch_drain_under_guard() {
    let transport = MockTransport::new(120.0, 0.0, true);
    let wave = sine_wave(2.0, 48_000.0);

    let (mut unit, handle) = TrackClipReaderUnit::with_transport(transport.clone(), None);
    unit.set_sample_rate(SampleRate(48_000.0));

    let sampler = SamplerUnit::with_transport(
        wave.clone(),
        transport.clone(),
        BeatPosition::new(0.0),
        None,
    );
    handle.send(ClipCommand::Add {
        id: SlotId(1),
        sampler,
        direction: Direction::Forward,
    });

    let mut output = [0.0f32; 2];
    // Drain the Add first, outside the guard.
    for _ in 0..16 {
        unit.tick(&[], &mut output);
    }

    // Enqueue a stretch update; draining it inside the guarded tick rebuilds
    // the stretch unit and allocates. Trips assert_no_alloc TODAY.
    handle.send(ClipCommand::UpdateStretch {
        id: SlotId(1),
        stretch_factor: Ratio::new(2.0),
        pitch_cents: Cents::new(0.0),
    });

    assert_no_alloc::assert_no_alloc(|| {
        unit.tick(&[], &mut output);
    });
}
