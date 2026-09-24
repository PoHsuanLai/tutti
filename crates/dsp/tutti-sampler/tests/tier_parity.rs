//! The two playback tiers must sound the same.
//!
//! `VoiceSource` has two variants: `Memory` indexes an `Arc<Wave>` resident in
//! RAM, `Disk` pops a ring the butler thread refills from a file. The crate's
//! own design invariant says they "differ ONLY in the *essential* per-sample
//! read" — same interpolation kernel, same channel policy, same placement gate.
//! Nothing tested that. The butler is ~4,000 lines whose only end-to-end
//! coverage was a smoke test asserting a fresh streamer has an empty plan map.
//!
//! # Why this specific comparison
//!
//! The two tiers reach the shared placement gate with *differently split*
//! arguments — memory passes `(wave.sample_rate(), speed)`, disk passes
//! `(session_rate * src_ratio, speed)`. Those are algebraically equal, and
//! `interp.rs` already pins that they agree at the kernel. But agreeing on a
//! computed position is not the same as producing the same audio: the disk tier
//! feeds that kernel from its own 4-tap ring history rather than an indexable
//! `Wave`, and that fetch path is what has no coverage.
//!
//! The divergence is not hypothetical. `disk_voice.rs` carries two comments
//! recording times these paths drifted: a hand-rolled `speed * src_ratio` that
//! bypassed the one composition point, and a `tick` path that drained its ring
//! at full speed while `process` did not. Both were live bugs. A file that
//! *sounds different* depending on whether it fit in RAM is the class of defect
//! this test exists to catch.
//!
//! # How readiness is handled: by counting, not by waiting
//!
//! The butler is driven **by hand** here — [`DiskStreamer::manual`] plus
//! [`step_once`](DiskStreamer::step_once) — so there is no butler thread, no
//! sleep, no `Instant`, and no timeout anywhere in this file.
//!
//! That is not cosmetic. The threaded butler parks 1 ms when idle and 3 ms when
//! its rings are healthy, and the earlier version of this file polled at 5–10 ms
//! against a 5 s liveness ceiling — so every readiness check was a race against
//! a producer the test could not see, and the ceiling was really a guess about
//! the machine. Worse, the polling was itself made of *renders*: each attempt
//! advanced the clock, so a warm-up that needed several attempts walked the
//! transport deep into the file and the subsequent measurement was taken
//! somewhere else entirely.
//!
//! Stepping removes both. [`prime`] runs cycles until the butler stops making
//! progress ([`StepOutcome`] is no longer `Busy`), which is exactly the point
//! the threaded butler would park — and it takes single-digit cycles. Where a
//! test still needs the butler to keep up with a long render, [`render_streamed`]
//! interleaves a step per block, which is the same relationship the thread has
//! to the audio callback with the timing indeterminacy removed.
//!
//! The step is the shipped one: `butler_loop_async` is written in terms of the
//! same `ButlerCycle::step`, so what this file drives and what a host runs
//! cannot diverge.

use std::path::Path;
use std::sync::Arc;

use tutti_core::BufferVec;
use tutti_core::{
    AudioUnit, Beat, Bpm, ChannelLayout, PlaybackRate, SamplePosition, SampleRate, Timeline, Wave,
};
use tutti_sampler::{Command, DiskStreamer, DiskStreamerConfig, StepOutcome};
use tutti_sampler::{DiskVoice, MemorySource, VoiceWindow};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
/// Blocks rendered for the actual comparison, after warm-up.
const COMPARE_BLOCKS: usize = 200;

/// Ceiling on butler cycles spent priming a ring before the test calls the
/// butler broken.
///
/// A liveness bound, not a pacing knob. Priming takes single-digit cycles in
/// practice; a butler still reporting [`StepOutcome::Busy`] after this many is
/// one that refills forever without ever catching up, and exhausting the budget
/// is the failure — never a reason to proceed and measure silence.
const PRIME_BUDGET: usize = 512;

/// How far a measured chirp frequency may sit from the one its target position
/// implies.
///
/// The measured span covers 64 blocks (~85 ms), over which the chirp itself
/// sweeps ~8.5 Hz, so the peak lands near the span's midpoint rather than exactly
/// at the seek point. 40 Hz absorbs that while still pinning position to under
/// half a second — and a mis-seek of even one second is 100 Hz off, so this
/// cannot admit a real failure.
const SEEK_TOL_HZ: f64 = 40.0;

/// Length of the file the *live-seek* tests stream, in seconds.
///
/// **Must exceed 30 s**, and that is the whole point. `buffer_size_for_file`
/// sizes the ring to hold the entire file when it is under 50 MB, capped at 30 s
/// — so a shorter file is prefilled whole, a seek repositions the *writer* into
/// a ring that already holds everything, and the audio never changes. These
/// tests originally used a 20 s file and every seek "landed" at the file's end,
/// which read as an engine bug and was not one.
///
/// 31 s rather than the 60 s this was: the cap is 30 s, so one second past it
/// makes a seek real work, and the extra 29 s bought nothing but a 23 MB file
/// written on every run.
///
/// Only the two tests that move an *already running* stream pay this cost at
/// all. [`streaming_from_an_offset_delivers_that_part_of_the_file`] does not: it
/// checks where a stream *starts*, and the butler seeks the writer to the offset
/// before the first refill, so a short file is prefilled from the right place —
/// it takes [`OFFSET_FILE_SECS`] instead.
///
/// Across the file the fixtures went from 220 s of runtime-generated WAV (~84 MB)
/// to 90 s (~35 MB).
const SEEK_FILE_SECS: f64 = 31.0;

/// Length of the file the *start-offset* test streams, in seconds.
///
/// The largest offset it asks for plus enough material to measure a settled
/// span. No ring-cap requirement applies (see [`SEEK_FILE_SECS`]), so this is
/// sized by what the assertions need and nothing else.
const OFFSET_FILE_SECS: f64 = 12.0;

/// A rolling transport the test advances by hand, once per block.
///
/// `MockTransport` is `#[cfg(test)]` inside the crate, so an integration test
/// cannot reach it. Reimplemented here for the same reason `render_cases.rs`
/// does: `Timeline` is three methods, and widening a test-only surface so an
/// integration test can borrow it would make the production API answer to this
/// file.
struct Clock {
    beat: std::sync::atomic::AtomicU64,
    tempo: f64,
}

impl Clock {
    fn new(tempo: f64) -> Arc<Self> {
        Arc::new(Self {
            beat: std::sync::atomic::AtomicU64::new(0f64.to_bits()),
            tempo,
        })
    }

    /// Jump the playhead to an absolute position in **seconds** of file time.
    ///
    /// This is the seek verb for a placed voice: the transport owns position, so
    /// moving the playhead is what repositions the clip. Seconds rather than
    /// beats at the call site because the material's frequency encodes seconds.
    fn seek_seconds(&self, sec: f64) {
        let beats = sec * self.tempo / 60.0;
        self.beat
            .store(beats.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Move by `samples`, the way a block-driven transport does after `process`.
    fn advance(&self, samples: usize) {
        let beats = samples as f64 * self.tempo / 60.0 / SR;
        let now = f64::from_bits(self.beat.load(std::sync::atomic::Ordering::Relaxed));
        self.beat.store(
            (now + beats).to_bits(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

impl Timeline for Clock {
    fn beat(&self) -> Beat {
        Beat::new(f64::from_bits(
            self.beat.load(std::sync::atomic::Ordering::Relaxed),
        ))
    }
    fn tempo(&self) -> Bpm {
        Bpm::new(self.tempo)
    }
    fn is_rolling(&self) -> bool {
        true
    }
}

/// Distinguishable test material: a tone whose two channels differ.
///
/// Channel 1 is a different frequency rather than a copy or an inversion, so a
/// channel swap, a mono fold, or a dropped channel each produce a *different*
/// signal instead of a plausible one. Deliberately not a pure single tone: the
/// sum of two partials makes interpolation error visible as a changed waveform
/// rather than only as a level shift.
fn write_test_wav(path: &Path, frames: usize) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("create test wav");
    for i in 0..frames {
        let t = i as f32 / SR as f32;
        let l = (std::f32::consts::TAU * 440.0 * t).sin() * 0.4
            + (std::f32::consts::TAU * 1320.0 * t).sin() * 0.2;
        let r = (std::f32::consts::TAU * 660.0 * t).sin() * 0.4;
        w.write_sample(l).unwrap();
        w.write_sample(r).unwrap();
    }
    w.finalize().expect("finalize test wav");
}

/// A single steady tone on both channels.
///
/// For the varispeed cases, where the question is "what rate is the ring
/// draining at" and any second partial only gives the peak somewhere else to
/// land once it has been transposed.
fn write_tone_wav(path: &Path, frames: usize, hz: f32) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("create tone wav");
    for i in 0..frames {
        let s = (std::f32::consts::TAU * hz * i as f32 / SR as f32).sin() * 0.4;
        w.write_sample(s).unwrap();
        w.write_sample(s).unwrap();
    }
    w.finalize().expect("finalize tone wav");
}

/// Frequency at file position `sec`, for the position-encoding material below.
///
/// A linear ramp from 300 Hz upward at 100 Hz/s. Chosen so the mapping is
/// invertible and generous: adjacent seek targets in these tests are seconds
/// apart, i.e. hundreds of Hz, far outside any measurement error.
fn chirp_hz_at(sec: f64) -> f64 {
    300.0 + 100.0 * sec
}

/// Material whose instantaneous frequency **encodes its own file position**.
///
/// This is what makes a seek verifiable. A steady tone sounds identical
/// everywhere, so a seek to the wrong offset — or one silently ignored — is
/// indistinguishable from a correct one. With a chirp, measuring the pitch after
/// a seek says exactly *where in the file* the reader actually landed, and the
/// expected answer comes from [`chirp_hz_at`] rather than from the engine.
///
/// Phase is integrated (`phase += TAU * f(t) / SR`), not computed as
/// `sin(TAU*f(t)*t)`. The latter is the classic chirp bug: it doubles the
/// apparent sweep rate, because differentiating `f(t)*t` gives `2*f(t)` for a
/// linear ramp. That would make every position assertion here wrong by 2x while
/// still looking like a plausible sweep.
fn write_chirp_wav(path: &Path, frames: usize) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("create chirp wav");
    let mut phase = 0.0f64;
    for i in 0..frames {
        let f = chirp_hz_at(i as f64 / SR);
        phase += std::f64::consts::TAU * f / SR;
        let s = (phase.sin() * 0.4) as f32;
        // Both channels carry the same sweep here: this file exists to locate a
        // position, and `write_test_wav` already covers channel identity.
        w.write_sample(s).unwrap();
        w.write_sample(s).unwrap();
    }
    w.finalize().expect("finalize chirp wav");
}

/// The same material as an in-memory `Wave`, read from the file just written so
/// the two tiers are provably fed identical bytes.
fn load_wave(path: &Path) -> Arc<Wave> {
    let mut r = hound::WavReader::open(path).expect("open test wav");
    let mut wave = Wave::new(2, SR);
    let samples: Vec<f32> = r.samples::<f32>().map(|s| s.unwrap()).collect();
    for frame in samples.as_chunks::<2>().0 {
        wave.push((frame[0], frame[1]));
    }
    Arc::new(wave)
}

/// Render `blocks` blocks of a unit into interleaved stereo, advancing `clock`
/// once per block.
///
/// Block-driven via `process`, never `tick`. A placed voice derives its position
/// from the playhead, which advances once per *block*; `tick` has no
/// `offset_in_block`, so calling it BLOCK times against one transport reading
/// emits the same sample BLOCK times — a staircase that resamples the source
/// downward and makes every case fail. That trap is documented in
/// `examples/README.md` and it cost a full debugging session there.
fn render(unit: &mut dyn AudioUnit, clock: &Clock, blocks: usize) -> Vec<(f32, f32)> {
    let input = BufferVec::new(2);
    let mut output = BufferVec::new(2);
    let mut out = Vec::with_capacity(blocks * BLOCK);

    for _ in 0..blocks {
        unit.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        let b = output.buffer_ref();
        for i in 0..BLOCK {
            out.push((b.at_f32(0, i), b.at_f32(1, i)));
        }
        clock.advance(BLOCK);
    }
    out
}

/// [`render`], with one butler cycle run per block.
///
/// The step stands where the butler thread's own cycle would: the reader has
/// just drained a block, so the butler gets its chance to refill before the next
/// one. A long render against a hand-driven butler needs this — otherwise the
/// ring drains to empty partway through and the tail of the measurement is
/// silence that no amount of stepping afterwards can put back.
///
/// This is only used where a render outruns what one priming can cover; short
/// spans take plain [`render`], which keeps the butler provably out of the
/// measurement.
fn render_streamed(
    voice: &mut DiskVoice,
    streamer: &mut DiskStreamer,
    clock: &Clock,
    blocks: usize,
) -> Vec<(f32, f32)> {
    let input = BufferVec::new(2);
    let mut output = BufferVec::new(2);
    let mut out = Vec::with_capacity(blocks * BLOCK);

    for _ in 0..blocks {
        voice.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        let b = output.buffer_ref();
        for i in 0..BLOCK {
            out.push((b.at_f32(0, i), b.at_f32(1, i)));
        }
        clock.advance(BLOCK);
        let _ = streamer.step_once();
    }
    out
}

fn peak(frames: &[(f32, f32)]) -> f32 {
    frames
        .iter()
        .fold(0.0f32, |a, &(l, r)| a.max(l.abs()).max(r.abs()))
}

/// Run butler cycles until it stops making progress, and fail if it never does
/// inside [`PRIME_BUDGET`].
///
/// [`StepOutcome::Busy`] is precisely "a ring is below its refill threshold and
/// this cycle put frames into it" — the condition the threaded butler declines
/// to park on. Stopping when it clears leaves the rings exactly as full as the
/// threaded butler would leave them before its first park, so this is the
/// shipped readiness point and not a test-chosen approximation of one.
///
/// Note it is `!= Busy` and not `== Healthy`. `Healthy` is a fraction of ring
/// *capacity*, and capacity is sized from the whole file, so a stream reading
/// its last seconds is permanently below threshold with nothing left to load —
/// [`StepOutcome::Stalled`]. Waiting for `Healthy` there would hang; a test that
/// streams near the end of a file (this file has several) needs the fixed point,
/// not the threshold.
fn prime(streamer: &mut DiskStreamer) {
    for _ in 0..PRIME_BUDGET {
        if streamer.step_once() != StepOutcome::Busy {
            return;
        }
    }
    panic!(
        "the butler refilled for {PRIME_BUDGET} cycles without ever catching up or running out \
         of material -- measuring past this point would say nothing about the engine"
    );
}

/// Build a hand-driven streamer plus a disk voice on `channel`, streaming
/// `path` from `at_sec`, with its ring already primed.
///
/// Two properties this shape buys over the polling version it replaces. The
/// clock is placed at `at_sec` and **stays** there through priming, because
/// priming is stepping rather than rendering — the old warm-up rendered on every
/// poll and walked the transport forward, so the measurement that followed was
/// taken somewhere the caller had not asked for. And `take_disk_voice` is called
/// exactly once, after the `Stream` command has demonstrably been applied,
/// rather than in a retry loop that cannot tell "not yet" from "never".
fn stream_at(
    streamer: &mut DiskStreamer,
    path: &Path,
    at_sec: f64,
    channel: usize,
) -> (DiskVoice, Arc<Clock>) {
    let clock = Clock::new(120.0);
    clock.seek_seconds(at_sec);

    streamer
        .commands()
        .send(Command::Stream {
            channel_index: channel,
            file_path: path.to_path_buf(),
            offset: SamplePosition(at_sec * SR),
        })
        .expect("the butler is alive in this test");

    prime(streamer);

    let mut voice = streamer
        .status()
        .take_disk_voice(
            channel,
            clock.clone() as Arc<dyn Timeline>,
            Beat::new(0.0),
            None,
        )
        .unwrap_or_else(|| {
            panic!(
                "the butler applied its commands and reported its rings full, but installed no \
                 link for channel {channel} streaming from {at_sec}s"
            )
        });
    voice.set_sample_rate(SampleRate(SR));
    clock.seek_seconds(at_sec);

    (voice, clock)
}

/// The two tiers must produce the same audio from the same file.
///
/// Compared statistically rather than sample-for-sample: the tiers start from
/// different ring/warm-up states, so their absolute alignment differs by a few
/// samples even when both are correct. What must match is the *content* — level
/// and spectrum. A tier reading at the wrong rate, folding channels
/// differently, or interpolating differently changes both, by far more than the
/// tolerance here.
#[test]
fn the_disk_and_memory_tiers_render_the_same_material() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("parity.wav");
    // 8 seconds: long enough that neither tier runs dry during the comparison,
    // at unity rate.
    write_test_wav(&path, (SR * 8.0) as usize);

    // ---- memory tier -----------------------------------------------------
    let mem_clock = Clock::new(120.0);
    let wave = load_wave(&path);
    let mut mem = MemorySource::with_config(
        wave,
        tutti_sampler::MemorySourceConfig {
            timeline: Some(mem_clock.clone() as Arc<dyn Timeline>),
            window: VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            },
            channels: ChannelLayout::STEREO,
            ..Default::default()
        },
    );
    mem.set_sample_rate(SampleRate(SR));

    // ---- disk tier -------------------------------------------------------
    let mut streamer =
        DiskStreamer::manual(SR, DiskStreamerConfig::default()).expect("streamer builds");
    let (mut disk, disk_clock) = stream_at(&mut streamer, &path, 0.0, 0);

    // Both tiers start at the playhead's origin, so no warm-up realignment is
    // needed — the old version had to advance the memory tier to wherever its
    // polling warm-up had left the disk clock, which is exactly the coupling
    // stepping removes.
    let mem_out = render(&mut mem, &mem_clock, COMPARE_BLOCKS);
    let disk_out = render_streamed(&mut disk, &mut streamer, &disk_clock, COMPARE_BLOCKS);

    // Both must actually be playing. Two silent tiers agree perfectly and prove
    // nothing -- this is the assertion that stops the whole test being vacuous.
    let (mp, dp) = (peak(&mem_out), peak(&disk_out));
    assert!(
        mp > 0.1,
        "memory tier is silent (peak {mp:.5}) -- nothing was compared"
    );
    assert!(
        dp > 0.1,
        "disk tier is silent (peak {dp:.5}) -- nothing was compared"
    );

    // Level parity, per channel. A wrong read rate, a dropped channel, or a
    // mono fold all move this well past 1 dB.
    for (ch, name) in [(0usize, "left"), (1, "right")] {
        let pick = |v: &Vec<(f32, f32)>| -> Vec<f32> {
            v.iter()
                .map(|&(l, r)| if ch == 0 { l } else { r })
                .collect()
        };
        let m = rms(&pick(&mem_out));
        let k = rms(&pick(&disk_out));
        let db = 20.0 * (k / m).log10();
        assert!(
            db.abs() < 1.0,
            "{name}: disk tier is {db:+.2} dB from memory (mem rms {m:.4}, disk rms {k:.4}) \
             -- the tiers are not reading the same material"
        );
    }

    // Spectral parity: the dominant frequency must match. This is what catches
    // a rate divergence, which changes pitch while leaving level intact -- the
    // exact shape of the 8.8% bug the two tiers' rate splits already caused once.
    for (ch, name, want) in [(0usize, "left", 440.0f64), (1, "right", 660.0)] {
        let pick = |v: &Vec<(f32, f32)>| -> Vec<f32> {
            v.iter()
                .map(|&(l, r)| if ch == 0 { l } else { r })
                .collect()
        };
        let mf = dominant_hz(&pick(&mem_out));
        let df = dominant_hz(&pick(&disk_out));

        assert!(
            (mf - want).abs() / want < 0.03,
            "{name}: memory tier reads {mf:.1} Hz, source is {want} Hz \
             -- the harness or the memory tier is wrong, so the comparison is meaningless"
        );
        assert!(
            (df - mf).abs() / mf < 0.03,
            "{name}: disk tier reads {df:.1} Hz but memory reads {mf:.1} Hz \
             -- the tiers disagree on playback rate"
        );
    }
}

/// A disk voice must reach audio at all, in a bounded number of butler cycles.
///
/// Split out from the parity test so a butler that never primes its ring is
/// reported as its own failure rather than as "the tiers disagree" — the
/// minimum end-to-end claim the streaming path has to satisfy.
#[test]
fn a_disk_voice_produces_audio_from_a_real_file() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("stream.wav");
    write_test_wav(&path, (SR * 4.0) as usize);

    let mut streamer =
        DiskStreamer::manual(SR, DiskStreamerConfig::default()).expect("streamer builds");
    let (mut voice, clock) = stream_at(&mut streamer, &path, 0.0, 0);

    let out = render_streamed(&mut voice, &mut streamer, &clock, 100);

    assert!(
        peak(&out) > 0.1,
        "disk voice went silent after priming (peak {:.5})",
        peak(&out)
    );
    // Both channels carry material -- a tier that dropped one would still pass a
    // bare peak check on the other.
    let lp = out.iter().fold(0.0f32, |a, &(l, _)| a.max(l.abs()));
    let rp = out.iter().fold(0.0f32, |a, &(_, r)| a.max(r.abs()));
    assert!(
        lp > 0.1 && rp > 0.1,
        "a channel is silent: L {lp:.4} R {rp:.4}"
    );
}

// A test asserting "one priming pass leaves a *deeply* filled ring" was written
// here and then removed, because it could not fail.
//
// `varifill_chunk` scales the refill chunk by how empty the ring is, so the
// first cycle after a `Stream` sizes its read to the whole ring. Priming depth
// is therefore not a property with a shallow failure mode to catch: reporting
// `Healthy` unconditionally from `ButlerCycle::step` — i.e. stopping `prime`
// after exactly one cycle — still leaves 8 s of audio resident, and a 6 s
// butler-free render still passes.
//
// Recorded rather than deleted silently: the next person to notice the gap
// should know it was looked at, and that the assertion an obvious test would
// make is one the engine satisfies by construction rather than by the code path
// the test would be aimed at. `a_disk_voice_produces_audio_from_a_real_file`
// covers what remains — that priming yields audio at all.

// ---------------------------------------------------------------------------
// Seek
// ---------------------------------------------------------------------------

/// Streaming from a given offset must deliver the material at that offset.
///
/// Verified against position-encoding material ([`write_chirp_wav`]): the
/// frequency the reader emits says exactly where in the file it landed, and
/// [`chirp_hz_at`] states the expected answer independently of the engine. A
/// steady tone cannot make this claim — it sounds the same everywhere, so an
/// offset that was ignored, clamped, or scaled would be invisible.
///
/// This is the property a seek rests on: repositioning is only meaningful if
/// "start from sample N" actually yields sample N.
#[test]
fn streaming_from_an_offset_delivers_that_part_of_the_file() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("seek.wav");
    write_chirp_wav(&path, (SR * OFFSET_FILE_SECS) as usize);

    let mut streamer =
        DiskStreamer::manual(SR, DiskStreamerConfig::default()).expect("streamer builds");

    // Spread across the file. A distinct channel per case: the butler keys its
    // plans by channel index, so re-streaming the same channel reuses that
    // channel's live region rather than starting cleanly from the new offset —
    // which made every case after the first report the first one's position.
    for (i, at) in [0.0f64, 3.0, 8.0, 10.0].into_iter().enumerate() {
        let (mut voice, clock) = stream_at(&mut streamer, &path, at, i);
        clock.seek_seconds(at);
        let got = render(&mut voice, &clock, 64);

        assert!(
            peak(&got) > 0.05,
            "streaming from {at}s produced silence (peak {:.5})",
            peak(&got)
        );

        let left: Vec<f32> = got.iter().map(|&(l, _)| l).collect();
        let hz = dominant_hz_in(&left, 200.0, 6500.0);
        let want = chirp_hz_at(at);

        assert!(
            (hz - want).abs() < SEEK_TOL_HZ,
            "streaming from {at}s delivered the material at ~{:.2}s \
             (measured {hz:.1} Hz, expected {want:.1} Hz)",
            (hz - 300.0) / 100.0
        );
    }
}

/// A seek on a live stream repositions it, and the output says where to.
///
/// The companion to [`streaming_from_an_offset_delivers_that_part_of_the_file`]:
/// that one checks where a stream *starts*, this one checks that an
/// already-running stream can be *moved*. Both directions are covered — a
/// backward seek is the one a naive implementation is most likely to get wrong,
/// since "refill forward from here" is the common path.
///
/// # This was `#[ignore]`d, and stepping is what un-ignored it
///
/// The engine was never at fault. `reposition_click_free` flushes the ring and
/// moves the writer, and the in-crate
/// `butler::streamer::tests::a_backward_seek_repositions_the_live_stream` has
/// always shown that happening. What failed was the *harness*: it polled for the
/// new material by rendering, each poll advanced the clock, a placed voice
/// follows the clock, and the read head chased its own tail — so the target moved
/// while the test waited for it.
///
/// Stepping ends that. `step_once` applies the queued `Seek` and refills from the
/// new position before returning, with the clock held still, so the very next
/// render is the post-seek material. There is nothing left to poll for.
#[test]
fn seeking_a_live_stream_repositions_it_in_both_directions() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live_seek.wav");
    write_chirp_wav(&path, (SR * SEEK_FILE_SECS) as usize);

    let mut streamer =
        DiskStreamer::manual(SR, DiskStreamerConfig::default()).expect("streamer builds");

    // Start at 20 s so both a forward (to 28 s) and a backward (to 5 s) seek are
    // real moves, and neither is reachable by simply playing on. A distinct
    // channel per case: the butler keys its plans by channel index.
    for (i, target) in [28.0f64, 5.0].into_iter().enumerate() {
        let (mut voice, clock) = stream_at(&mut streamer, &path, 20.0, i);

        // Confirm the pre-seek position, so a "seek landed" verdict cannot be
        // satisfied by a stream that was already there.
        clock.seek_seconds(20.0);
        let before = render(&mut voice, &clock, 64);
        let before_hz = dominant_hz_in(
            &before.iter().map(|&(l, _)| l).collect::<Vec<_>>(),
            200.0,
            6500.0,
        );
        assert!(
            (before_hz - chirp_hz_at(20.0)).abs() < SEEK_TOL_HZ,
            "the stream was not at 20s before the seek (measured {before_hz:.1} Hz) \
             -- the seek assertion below would prove nothing"
        );

        clock.seek_seconds(target);
        streamer
            .commands()
            .send(Command::Seek {
                channel_index: i,
                file_position: SamplePosition(target * SR),
            })
            .expect("the butler is alive in this test");

        // Render *with* the butler cycling. Two seek requests converge here and
        // both need a cycle to be applied: the `Command::Seek` just queued, and
        // the one `DiskVoice`'s own placement gate raises on the next block
        // because the playhead jumped past `SEEK_EPSILON_SAMPLES`. Priming
        // before rendering would apply only the first — the gate has not run
        // yet — and the reader would drain the pre-seek ring.
        //
        // The discarded first span is the transition itself: the ring is flushed
        // and refilling, and the crossfade is in it. `a_seek_transition_does_not_clip`
        // is the test that looks at that span; this one is about where the
        // stream *settles*.
        // Two discarded spans, then the measurement. The discards cover the
        // transition: the ring is flushed on reposition, the crossfade plays out
        // of it, and the refill from the new offset has to overtake both. A
        // backward seek needs more of that than a forward one — it was still
        // 0.44 s late after one span — because it discards a ring that was
        // already prefetched ahead of the old position.
        //
        // Fixed spans rather than a poll: each is a known 85 ms of transport,
        // and what is being asserted is *where the stream settles*, not how
        // quickly it gets there. `a_seek_transition_does_not_clip` is the test
        // that looks inside the transition.
        // The **first** span after the seek, and only the first.
        //
        // Re-seeking the clock does not rewind the reader — the jump is far
        // inside `SEEK_EPSILON_SAMPLES`, so the placement gate reads it as
        // contiguous playback and plays on. Each further span therefore reads
        // the *next* 85 ms of file, drifting +8.5 Hz per span at this chirp
        // rate, and a test that rendered a few "settling" spans first would be
        // measuring a position it walked to rather than the one the seek
        // reached. That drift is the harness, not the engine, and taking the
        // first span is what removes it.
        clock.seek_seconds(target);
        let got = render_streamed(&mut voice, &mut streamer, &clock, 64);
        let want = chirp_hz_at(target);
        let dir = if target > 20.0 { "forward" } else { "backward" };

        assert!(
            peak(&got) > 0.05,
            "a {dir} seek from 20s to {target}s produced silence (peak {:.5})",
            peak(&got)
        );

        let hz = dominant_hz_in(
            &got.iter().map(|&(l, _)| l).collect::<Vec<_>>(),
            200.0,
            6500.0,
        );
        assert!(
            (hz - want).abs() < SEEK_TOL_HZ,
            "a {dir} seek from 20s to {target}s landed at ~{:.2}s \
             (measured {hz:.1} Hz, expected {want:.1} Hz)",
            (hz - 300.0) / 100.0
        );
    }
}

/// A seek transition must stay bounded — no click, no summed crossfade.
///
/// The butler repositions click-free with a crossfade, so the join must not
/// overshoot. A crossfade that summed instead of fading would reach roughly twice
/// the source's own peak; a discontinuity would show as a spike.
#[test]
fn a_seek_transition_does_not_clip() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("xfade.wav");
    write_chirp_wav(&path, (SR * SEEK_FILE_SECS) as usize);

    let mut streamer =
        DiskStreamer::manual(SR, DiskStreamerConfig::default()).expect("streamer builds");
    let (mut voice, clock) = stream_at(&mut streamer, &path, 10.0, 0);

    clock.seek_seconds(30.0);
    streamer
        .commands()
        .send(Command::Seek {
            channel_index: 0,
            file_position: SamplePosition(30.0 * SR),
        })
        .expect("the butler is alive in this test");

    // Capture the whole transition, including the crossfade. One butler cycle
    // per block puts the reposition inside the captured span rather than before
    // it — the crossfade is what this test exists to look at.
    clock.seek_seconds(30.0);
    let transition = render_streamed(&mut voice, &mut streamer, &clock, 320);

    // The source peaks at 0.4. Summing two spans of it would reach ~0.8.
    let p = peak(&transition);
    assert!(
        p < 0.6,
        "the seek transition peaked at {p:.4}, above the source's own 0.4 — \
         the crossfade is summing rather than fading"
    );
    // And it must not be vacuous: a silent span cannot clip either.
    assert!(
        p > 0.05,
        "the seek transition is silent (peak {p:.5}) -- there was no transition to bound"
    );
}

// ---------------------------------------------------------------------------
// Varispeed
// ---------------------------------------------------------------------------

/// Varispeed transposes by exactly its factor, on the disk tier.
///
/// For a sampler, read rate *is* pitch: at 2x the source is consumed twice as
/// fast, so a 440 Hz tone becomes 880 Hz. That makes the measured frequency a
/// direct readout of the rate the ring is actually draining at — which is the
/// quantity the fetch-headroom bug got wrong by 6.25% while leaving level and
/// waveform untouched.
///
/// Rates are checked against the *unity* measurement rather than against 440 Hz,
/// so this asserts the ratio the engine applies rather than re-deriving what the
/// source contains.
#[test]
fn disk_varispeed_transposes_by_its_factor() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("vari.wav");
    // A single 440 Hz tone on both channels, NOT `write_test_wav`.
    //
    // That file carries 440+1320 on the left and 660 on the right, which is
    // right for a channel-identity check and wrong here: under varispeed the
    // transposed partials land on each other's bands (660 x 1.5 = 990 sits where
    // the left fundamental is looked for), and the measurement silently reports
    // the wrong peak. This test first "failed" at 1.5x reading 990 Hz for
    // exactly that reason — the harness, not the engine.
    //
    // 8 s rather than 20: the fastest factor here is 2x over 128 blocks from the
    // file's head, which reaches ~0.35 s in. The old length was sized for a
    // warm-up that walked the clock forward, and stepping removed that walk.
    write_tone_wav(&path, (SR * 8.0) as usize, 440.0);

    let mut streamer =
        DiskStreamer::manual(SR, DiskStreamerConfig::default()).expect("streamer builds");
    let (mut voice, clock) = stream_at(&mut streamer, &path, 0.0, 0);

    // The control, at unity. Everything below is measured against this, so a
    // constant offset in the harness cannot masquerade as a correct ratio.
    voice.set_speed(PlaybackRate::new(1.0));
    let base = render_streamed(&mut voice, &mut streamer, &clock, 128);
    let base_left: Vec<f32> = base.iter().map(|&(l, _)| l).collect();
    let base_hz = dominant_hz_in(&base_left, 200.0, 1000.0);
    assert!(
        (base_hz - 440.0).abs() < 15.0,
        "unity playback reads {base_hz:.1} Hz, expected ~440 — \
         the disk tier is not at unity rate and every ratio below is meaningless"
    );

    for factor in [2.0f32, 0.5, 1.5] {
        // Rewind before each factor.
        //
        // A placed voice's read position is `playhead x rate`, so a faster
        // factor races through the file: at 2x, the clock reaching 10 s puts the
        // read head at 20 s. Measuring successive factors without rewinding
        // walks off the end of the file and reads whatever is past it — which is
        // how the 0.5x case came to report 8 kHz on a 440 Hz tone, a "failure"
        // that was entirely the harness running out of material.
        clock.seek_seconds(0.0);
        voice.set_speed(PlaybackRate::new(factor));

        let want = base_hz * factor as f64;

        // A varispeed change moves the file position without moving the
        // playhead, so the gate re-seeks and the butler refills from the new
        // offset. Under the threaded butler that took an unknown amount of wall
        // clock and this loop polled for it; here the seek is applied by the
        // very next cycle, so priming once is the whole wait.
        prime(&mut streamer);
        clock.seek_seconds(0.0);
        let out = render_streamed(&mut voice, &mut streamer, &clock, 128);
        let left: Vec<f32> = out.iter().map(|&(l, _)| l).collect();
        // Brackets every factor under test (220 at 0.5x, 880 at 2x) with
        // margin; the material is a single tone so nothing else can win.
        let hz = dominant_hz_in(&left, 150.0, 1200.0);

        assert!(
            peak(&out) > 0.1,
            "speed {factor}x produced near-silence (peak {:.4})",
            peak(&out)
        );
        assert!(
            (hz - want).abs() / want < 0.03,
            "speed {factor}x reads {hz:.1} Hz, expected ~{want:.1} Hz \
             ({:.1}% off) — the ring is not draining at the requested rate",
            (hz - want).abs() / want * 100.0
        );
    }
}

/// Varispeed on the disk tier must match varispeed on the memory tier.
///
/// The two tiers apply speed through different machinery — memory scales its
/// derived read position, disk publishes into `RtState` and drains a ring — so
/// "both transpose by the factor" does not by itself mean they agree. This
/// compares them directly, which is the claim a user actually depends on: the
/// same clip at the same speed sounds the same regardless of which tier the
/// engine picked.
#[test]
fn the_tiers_agree_under_varispeed() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("vari_parity.wav");
    // Single tone, for the same reason as `disk_varispeed_transposes_by_its_factor`:
    // transposed partials of a two-tone file collide across the measurement bands.
    write_tone_wav(&path, (SR * 8.0) as usize, 440.0);

    let mut streamer =
        DiskStreamer::manual(SR, DiskStreamerConfig::default()).expect("streamer builds");
    let (mut disk, disk_clock) = stream_at(&mut streamer, &path, 0.0, 0);

    for factor in [2.0f32, 0.5] {
        // --- disk ---
        //
        // Rewind first. The memory tier below is rebuilt with a fresh clock each
        // iteration, so the disk side must start from the same place or the two
        // are compared at different points in the file.
        disk_clock.seek_seconds(0.0);
        disk.set_speed(PlaybackRate::new(factor));
        // The gate re-seeks on a varispeed change; the butler applies it on the
        // next cycle. This replaced a fixed 30 ms sleep whose adequacy was a
        // property of the machine.
        prime(&mut streamer);
        disk_clock.seek_seconds(0.0);
        let disk_out = render_streamed(&mut disk, &mut streamer, &disk_clock, 128);
        let disk_left: Vec<f32> = disk_out.iter().map(|&(l, _)| l).collect();
        let disk_hz = dominant_hz_in(&disk_left, 150.0, 1200.0);

        // --- memory, same speed, fresh so neither tier's history leaks in ---
        let mem_clock = Clock::new(120.0);
        let mut mem = MemorySource::with_config(
            load_wave(&path),
            tutti_sampler::MemorySourceConfig {
                timeline: Some(mem_clock.clone() as Arc<dyn Timeline>),
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                channels: ChannelLayout::STEREO,
                speed: PlaybackRate::new(factor),
                ..Default::default()
            },
        );
        mem.set_sample_rate(SampleRate(SR));
        let mem_out = render(&mut mem, &mem_clock, 128);
        let mem_left: Vec<f32> = mem_out.iter().map(|&(l, _)| l).collect();
        let mem_hz = dominant_hz_in(&mem_left, 150.0, 1200.0);

        assert!(
            peak(&disk_out) > 0.1 && peak(&mem_out) > 0.1,
            "at {factor}x a tier is silent (disk {:.4}, memory {:.4}) -- nothing was compared",
            peak(&disk_out),
            peak(&mem_out)
        );
        assert!(
            (disk_hz - mem_hz).abs() / mem_hz < 0.03,
            "at {factor}x the disk tier reads {disk_hz:.1} Hz but memory reads \
             {mem_hz:.1} Hz — the tiers apply varispeed differently"
        );
    }
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
}

/// Dominant frequency by DFT peak with parabolic interpolation.
///
/// Hand-rolled rather than pulled from `tutti-analysis`: this test exists to
/// second-guess the engine's DSP, and measuring it with the engine's own FFT
/// would share whatever blind spot that code has.
fn dominant_hz(x: &[f32]) -> f64 {
    // 200..1000 Hz brackets both partials of `write_test_wav` (440, 660) without
    // admitting its 1320 Hz upper partial, which would otherwise win the peak on
    // the left channel.
    dominant_hz_in(x, 200.0, 1000.0)
}

/// [`dominant_hz`] over an explicit band.
///
/// The chirp sweeps well past the default window, and a search band that
/// excludes the true peak reports a sidelobe with total confidence — so the band
/// is a parameter rather than a constant shared by materials with different
/// ranges.
fn dominant_hz_in(x: &[f32], band_lo: f64, band_hi: f64) -> f64 {
    let n = x.len();
    let windowed: Vec<f64> = x
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos();
            s as f64 * w
        })
        .collect();

    let mag = |k: usize| -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        let w = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
        for (i, &s) in windowed.iter().enumerate() {
            let (sin, cos) = (w * i as f64).sin_cos();
            re += s * cos;
            im += s * sin;
        }
        (re * re + im * im).sqrt()
    };

    let lo = (band_lo * n as f64 / SR).floor() as usize;
    let hi = ((band_hi * n as f64 / SR).ceil() as usize).min(n / 2 - 2);
    let mags: Vec<f64> = (lo..=hi).map(mag).collect();

    let rel = mags
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(i, _)| i)
        .expect("non-empty spectrum");
    let k = lo + rel;
    if rel == 0 || rel + 1 >= mags.len() {
        return k as f64 * SR / n as f64;
    }
    let (a, b, c) = (mags[rel - 1], mags[rel], mags[rel + 1]);
    let denom = a - 2.0 * b + c;
    let delta = if denom != 0.0 {
        0.5 * (a - c) / denom
    } else {
        0.0
    };
    (k as f64 + delta) * SR / n as f64
}
