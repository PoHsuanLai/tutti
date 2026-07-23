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
use tutti_core::{AudioUnit, Beat, Bpm, BufferVec, Cents, Ratio, SampleRate, Timeline, Wave};
use tutti_sampler::stretch::{Algorithm, Unit as TimeStretchUnit};
use tutti_sampler::{
    ClipCommand, ClipSpec, Direction, Playback, SamplerUnit, SlotId, TrackClipReaderUnit, Voice,
    VoiceSource,
};

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

impl Timeline for MockTransport {
    fn is_rolling(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }
    fn beat(&self) -> Beat {
        Beat::new(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }
    fn tempo(&self) -> Bpm {
        Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
    }
    fn loop_range(&self) -> Option<tutti_core::transport::LoopRange> {
        None
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
        let sampler =
            SamplerUnit::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
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
        let sampler =
            SamplerUnit::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
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

/// The `ClipCommand::UpdateStretch` drain is allocation-free.
///
/// The resident time-stretch processor is built once at slot creation, off the
/// hot path. Draining an `UpdateStretch` calls `ClipSlot::set_stretch`, which
/// only flips the processor's lock-free `stretch_factor` / `pitch_cents` atomics
/// and mirrors them into the routing-gate fields — it allocates nothing and
/// (re)builds nothing. So the per-buffer `tick`/`process` command drain stays
/// alloc-free even when a stretch update lands on it. (Earlier, the drain
/// rebuilt the stretch unit inline and allocated; batch-1's resident-stretch
/// moved that construction to slot creation, closing the hole this test guards.)
///
/// The `AllocDisabler` global allocator *aborts* the process (SIGABRT via
/// `handle_alloc_error`) on a violation — it cannot be caught with
/// `catch_unwind`. An allocation here would take the whole test binary down,
/// hiding every other test's result. So the actual `assert_no_alloc`-guarded
/// drain runs in a re-exec'd CHILD of this test binary
/// (`RT_NO_ALLOC_STRETCH_CHILD=1`), and the parent asserts the child exited
/// cleanly. A regression that reintroduces allocation on the drain aborts the
/// child → this test FAILS.
#[test]
fn track_clip_reader_update_stretch_drain_is_allocation_free() {
    // Child arm: run the guarded drain. Aborts only on a regression.
    if std::env::var_os("RT_NO_ALLOC_STRETCH_CHILD").is_some() {
        run_stretch_drain_under_guard();
        // Reached because the guard did NOT abort — the drain is alloc-free.
        std::process::exit(0);
    }

    // Parent arm: re-exec ourselves running only this one test, in the child
    // role, and inspect how it exited.
    let exe = std::env::current_exe().expect("current_exe");
    let status = std::process::Command::new(exe)
        .arg("--exact")
        .arg("track_clip_reader_update_stretch_drain_is_allocation_free")
        .arg("--nocapture")
        .env("RT_NO_ALLOC_STRETCH_CHILD", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn child test process");

    assert!(
        status.success(),
        "UpdateStretch drain allocated on the audio thread (child exited {status}). \
         The drain must only flip the resident stretch unit's atomics — never allocate."
    );
}

/// The body that runs inside the re-exec'd child: enqueue an `UpdateStretch`
/// and drain it inside `assert_no_alloc`. Exits cleanly today (drain is
/// alloc-free); a regression that allocates on the drain aborts it.
fn run_stretch_drain_under_guard() {
    let transport = MockTransport::new(120.0, 0.0, true);
    let wave = sine_wave(2.0, 48_000.0);

    let (mut unit, handle) = TrackClipReaderUnit::with_transport(transport.clone(), None);
    unit.set_sample_rate(SampleRate(48_000.0));

    let sampler =
        SamplerUnit::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
    handle.send(ClipCommand::AddVoice {
        id: SlotId(1),
        voice: Box::new(Voice {
            source: VoiceSource::Ram(sampler),
            play: Playback::default(),
            channel_index: None,
        }),
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

// ---------------------------------------------------------------------------
// MicMonitorNode — live-input monitoring. Drains a shared capture ring on the
// audio thread; the pop goes through `AudioThreadCell::borrow_mut` (no lock, no
// alloc) and underruns emit silence. Both `tick` and `process` must be
// per-buffer allocation-free just like the playback units.
// ---------------------------------------------------------------------------

#[test]
fn mic_monitor_tick_is_allocation_free() {
    use ringbuf::{
        traits::{Producer, Split},
        HeapRb,
    };
    use tutti_sampler::{share_mic_ring, MicMonitorNode};

    let rb = HeapRb::<[f32; 2]>::new(1024);
    let (mut prod, cons) = rb.split();
    let mut node = MicMonitorNode::new(share_mic_ring(cons));

    // Prime the ring, then warm up.
    for _ in 0..512 {
        let _ = prod.try_push([0.1, -0.1]);
    }
    let mut output = [0.0f32; 2];
    for _ in 0..64 {
        node.tick(&[], &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        // Exercise both the ring-has-data and the underrun (empty) branches:
        // the loop far outlasts the 512 primed frames, so it goes silent.
        for _ in 0..100_000 {
            node.tick(&[], &mut output);
        }
    });
}

#[test]
fn mic_monitor_process_is_allocation_free() {
    use ringbuf::{
        traits::{Producer, Split},
        HeapRb,
    };
    use tutti_sampler::{share_mic_ring, MicMonitorNode};

    let rb = HeapRb::<[f32; 2]>::new(4096);
    let (mut prod, cons) = rb.split();
    let mut node = MicMonitorNode::new(share_mic_ring(cons));
    node.set_sample_rate(SampleRate(48_000.0));

    for _ in 0..2048 {
        let _ = prod.try_push([0.2, 0.2]);
    }

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    for _ in 0..8 {
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
