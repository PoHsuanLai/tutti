//! The pump loop, stepped by hand: counts are exact because the passes are the
//! test's, never a sleep. What only a thread runs (the stop handshake, the
//! park, the join) is `tests/recorder_thread.rs`.

use super::*;
use tutti_codec::{FileOut, Format, Pcm, Spec};
use tutti_types::SampleRate;

/// A WAV sink `channels` wide at 48 kHz.
fn sink(path: &std::path::Path, channels: u16, pcm: Pcm) -> FileOut {
    FileOut::create(
        path,
        &Spec::new(
            Format::Wav(pcm),
            ChannelLayout::from(channels),
            SampleRate(48_000.0),
        ),
    )
    .expect("sink opens")
}

/// A finite in-memory [`AudioIn`] standing in for a live mic: hands out its
/// frames in bounded chunks, returning a short-then-zero count at
/// end-of-stream. Has a runtime width, like the `SliceSource` fixture in
/// `tutti_types::io`'s own tests.
struct SliceSource {
    /// Flat interleaved at `layout`'s width.
    samples: Vec<f32>,
    /// Read cursor, in FRAMES.
    pos: usize,
    layout: ChannelLayout,
}

impl AudioIn for SliceSource {
    const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

    fn layout(&self) -> ChannelLayout {
        self.layout
    }

    fn poll_into(&mut self, out: &mut [f32]) -> Samples {
        let ch = self.layout.count() as usize;
        let total = self.samples.len() / ch;
        let n = (total - self.pos).min(out.len() / ch);
        out[..n * ch].copy_from_slice(&self.samples[self.pos * ch..(self.pos + n) * ch]);
        self.pos += n;
        Samples(n)
    }
}

/// Runs passes until one writes nothing, or `budget` passes have run, and
/// returns the total frames written. The budget bounds a source that never
/// runs dry.
fn pump_until_dry<I, O, S>(pump_loop: &mut PumpLoop<I, O, S>, budget: usize) -> Samples
where
    S: Copy + Default,
    I: AudioIn<S>,
    O: AudioOut<S>,
{
    let mut total = Samples::ZERO;
    for _ in 0..budget {
        match pump_loop.pump_once() {
            PumpPass::Wrote(n) => total += n,
            _ => break,
        }
    }
    total
}

/// Pumping a fake source into a real [`FileOut`] and finalizing yields a WAV
/// whose frame count matches what was fed: the round trip a recorder runs,
/// minus only the thread.
#[test]
fn pump_into_wav_sink_round_trips_frame_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recorded.wav");

    const FRAMES: usize = 3000;
    let samples: Vec<f32> = (0..FRAMES)
        .flat_map(|i| [i as f32 / 3000.0, -(i as f32) / 3000.0])
        .collect();
    let src = SliceSource {
        samples,
        pos: 0,
        layout: ChannelLayout::STEREO,
    };
    let mut pump_loop = PumpLoop::new(src, sink(&path, 2, Pcm::Float32), SCRATCH_FRAMES).unwrap();
    assert_eq!(pump_until_dry(&mut pump_loop, 16), Samples(FRAMES));
    pump_loop
        .finalize()
        .expect("finalize should back-patch the header");

    let reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.spec().sample_rate, 48_000);
    // Two samples (L, R) per stereo frame.
    assert_eq!(reader.len() as usize, FRAMES * 2);
}

/// **The replacement for the lost compile error, outer half.**
///
/// A width mismatch is a returned error, checked BEFORE any frame moves (and,
/// for a recorder, before the thread spawns), so no frame is ever written to a
/// file whose channels would rotate. This is the guarantee the runtime
/// `ChannelLayout` gave up at the type level.
///
/// The matching pair must still be accepted, which is the half that stops
/// this from passing vacuously.
///
/// Mutation (run): deleting the `layout != sink.layout()` check in
/// `PumpLoop::new` fails every refusal below.
#[test]
fn start_rejects_a_source_sink_width_mismatch() {
    let dir = tempfile::tempdir().unwrap();

    let stereo_src = || SliceSource {
        samples: vec![0.0f32; 64],
        pos: 0,
        layout: ChannelLayout::STEREO,
    };

    // Stereo source, 6-channel sink: refused.
    let wide = sink(&dir.path().join("mismatch.wav"), 6, Pcm::Float32);
    let err = Recorder::start(stereo_src(), wide)
        .expect_err("a width mismatch must be refused, not recorded");
    // The typed variant carries both layouts as values, so a caller can
    // branch on the widths without parsing the message.
    assert!(
        matches!(
            err,
            RecorderError::ChannelWidthMismatch { src, sink }
                if src == ChannelLayout::STEREO && sink == ChannelLayout::from(6u16)
        ),
        "expected ChannelWidthMismatch carrying both layouts, got: {err:?}"
    );
    assert!(
        err.to_string().contains("Stereo") && err.to_string().contains('6'),
        "the error must name both widths, got: {err}"
    );

    // Stereo source, mono sink: also refused. A narrowing mismatch is just as
    // wrong as a widening one, and it is the direction a caller is most likely
    // to think "it'll just downmix". Through the loop as well as the recorder.
    let mono = sink(&dir.path().join("mismatch_mono.wav"), 1, Pcm::Float32);
    assert!(PumpLoop::new(stereo_src(), mono, SCRATCH_FRAMES).is_err());

    // And the matching pair is accepted.
    let ok = sink(&dir.path().join("match.wav"), 2, Pcm::Float32);
    let rec = Recorder::start(stereo_src(), ok).expect("matching widths must be accepted");
    rec.finish().expect("finalize");
}

/// A non-stereo pairing records end to end, and every count on the way
/// through stays denominated in FRAMES.
///
/// At six channels a sample-denominated count is 6x off, so the frame total
/// read back off the finished file is a direct check on the whole chain:
/// `poll_into`, `pump`, `write`, header.
#[test]
fn a_six_channel_source_records_at_its_own_width() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("surround.wav");

    const FRAMES: usize = 500;
    const CH: usize = 6;
    let src = SliceSource {
        samples: (0..FRAMES * CH).map(|i| (i % 97) as f32 * 0.001).collect(),
        pos: 0,
        layout: ChannelLayout::from(6u16),
    };
    let mut pump_loop = PumpLoop::new(src, sink(&path, 6, Pcm::Float32), SCRATCH_FRAMES)
        .expect("matching 6ch widths");

    let written = pump_until_dry(&mut pump_loop, 16);
    assert_eq!(
        written,
        Samples(FRAMES),
        "the pump moved {written} frames of {FRAMES}: a short drain here would \
         make the header assertion below measure the harness"
    );
    pump_loop.finalize().expect("finalize");

    let reader = hound::WavReader::open(&path).expect("readable");
    assert_eq!(reader.spec().channels, 6);
    assert_eq!(
        reader.len() as usize,
        FRAMES * CH,
        "500 FRAMES of 6 channels is 3000 samples, not 500, and not 18000"
    );
}

/// A live (starving) source records until stopped: the composability a generic
/// `AudioIn` bound buys over a signature that opens a device itself.
///
/// The fixture withholds frames without being exhausted, so a loop that
/// mistook a zero-frame poll for end-of-stream would stop early.
#[test]
fn a_non_mic_live_source_records_until_stopped() {
    struct Starving {
        polls: usize,
    }

    impl AudioIn for Starving {
        const ON_EMPTY: OnEmpty = OnEmpty::Starved;

        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }

        fn poll_into(&mut self, out: &mut [f32]) -> Samples {
            let n = self.polls;
            self.polls += 1;
            // Every other poll is empty: never exhausted.
            if n % 2 == 1 || out.len() < 2 {
                return Samples::ZERO;
            }
            out[0] = 0.25;
            out[1] = -0.25;
            Samples(1)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("generated.wav");
    let mut pump_loop = PumpLoop::new(
        Starving { polls: 0 },
        sink(&path, 2, Pcm::Float32),
        SCRATCH_FRAMES,
    )
    .expect("matching stereo widths");

    // Six passes of a fixture that yields on every *other* poll: passes
    // 0, 2, 4 write a frame each and 1, 3, 5 report `Starved`. The sequence
    // fails if a `Starved` pass is treated as end-of-stream (writes stop at 1),
    // and if the parity of the fixture changes.
    let mut wrote = Samples::ZERO;
    for pass in 0..6 {
        match (pass % 2, pump_loop.pump_once()) {
            (0, PumpPass::Wrote(n)) => wrote += n,
            (1, PumpPass::Starved) => {}
            (_, other) => panic!(
                "pass {pass} of a source that yields every other poll reported {other:?}: \
                 a `Starved` pass read as end-of-stream ends a live take silently"
            ),
        }
    }
    assert_eq!(wrote, Samples(3), "three yielding passes, one frame each");

    pump_loop.finalize().expect("finalize should succeed");

    let reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
    let frames = reader.len() as usize / 2; // two samples per stereo frame
    assert_eq!(
        frames, 3,
        "the header must report exactly the frames the counted passes wrote"
    );
}

/// A sink that is not a WAV, over an element that is not `f32`: the pump is
/// generic over both, and a test sink can be read back directly.
#[derive(Clone)]
struct Collect(Arc<std::sync::Mutex<Vec<i16>>>);

impl AudioOut<i16> for Collect {
    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }
    fn write(&mut self, interleaved: &[i16]) {
        self.0.lock().unwrap().extend_from_slice(interleaved);
    }
    fn finalize(self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A finite stereo `i16` ramp: frame `k` is `(k, -k)`.
struct RampI16 {
    frames: usize,
    pos: usize,
}

impl AudioIn<i16> for RampI16 {
    const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }

    fn poll_into(&mut self, out: &mut [i16]) -> Samples {
        let fits = Samples::from_interleaved_len(out.len(), ChannelLayout::STEREO).get();
        let n = (self.frames - self.pos).min(fits);
        for i in 0..n {
            let k = (self.pos + i) as i16;
            out[i * 2] = k;
            out[i * 2 + 1] = -k;
        }
        self.pos += n;
        Samples(n)
    }
}

/// **A named capacity bounds every pass**, over any sink and element.
///
/// `bevy-tutti`'s `AudioPump` hands its capacity through here, so this is the
/// property its `Samples(64)` relies on: 1000 frames at 64 per pass is fifteen
/// full passes and a 40-frame tail, then `Ended`. The values are read back from
/// the sink, so a pass that skipped or repeated frames fails on the content,
/// not just the count.
///
/// *Mutation:* size the scratch from `SCRATCH_FRAMES` instead of `frames` in
/// `PumpLoop::new`. The first pass then writes all 1000 frames and the
/// `Wrote(64)` assertion fails.
#[test]
fn a_named_capacity_bounds_each_pass_over_any_sink() {
    let seen = Collect(Arc::new(std::sync::Mutex::new(Vec::new())));
    let mut pump_loop = PumpLoop::new(
        RampI16 {
            frames: 1000,
            pos: 0,
        },
        seen.clone(),
        Samples(64),
    )
    .expect("stereo into stereo");

    for pass in 0..15 {
        assert_eq!(
            pump_loop.pump_once(),
            PumpPass::Wrote(Samples(64)),
            "pass {pass} must move exactly the named capacity"
        );
    }
    assert_eq!(pump_loop.pump_once(), PumpPass::Wrote(Samples(40)));
    assert_eq!(pump_loop.pump_once(), PumpPass::Ended);
    pump_loop
        .finalize()
        .expect("a collecting sink finalizes cleanly");

    let got = seen.0.lock().unwrap();
    assert_eq!(got.len(), 2000, "1000 stereo frames are 2000 samples");
    for (k, frame) in got.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(*frame, [k as i16, -(k as i16)], "frame {k}");
    }
}

/// **A zero capacity is one frame, not a take that never moves.**
///
/// An empty scratch makes every poll return zero, and the source's `ON_EMPTY`
/// then reads that zero as its own: a finite take ends before its first frame,
/// a live one parks forever. `AudioPump` has always clamped to one frame; the
/// clamp lives in `PumpLoop::new`.
///
/// *Mutation:* drop the clamp (`let frames = capacity;`). The first pass then
/// reports `Ended` with nothing written and this fails.
#[test]
fn a_zero_capacity_moves_one_frame_per_pass() {
    let seen = Collect(Arc::new(std::sync::Mutex::new(Vec::new())));
    let mut pump_loop = PumpLoop::new(RampI16 { frames: 3, pos: 0 }, seen.clone(), Samples::ZERO)
        .expect("stereo into stereo");

    assert_eq!(pump_until_dry(&mut pump_loop, 16), Samples(3));
    assert_eq!(pump_loop.pump_once(), PumpPass::Ended);
    pump_loop.finalize().expect("finalize");
    assert_eq!(*seen.0.lock().unwrap(), [0, 0, 1, -1, 2, -2]);
}

/// A live source whose device can die: serves its queued frames, then reads
/// empty, and `dead` makes `is_failed` true.
struct DyingMic {
    queued: std::collections::VecDeque<[f32; 2]>,
    dead: Arc<AtomicBool>,
}

impl AudioIn for DyingMic {
    const ON_EMPTY: OnEmpty = OnEmpty::Starved;

    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }

    fn poll_into(&mut self, out: &mut [f32]) -> Samples {
        let mut n = 0;
        for chunk in out.as_chunks_mut::<2>().0 {
            let Some(frame) = self.queued.pop_front() else {
                break;
            };
            *chunk = frame;
            n += 1;
        }
        Samples(n)
    }

    fn is_failed(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }
}

fn frames(n: usize) -> std::collections::VecDeque<[f32; 2]> {
    (0..n)
        .map(|i| [i as f32 / 100.0, -(i as f32) / 100.0])
        .collect()
}

/// A mic that dies mid-take ends the take as failed, keeps the frames it
/// delivered and finalizes them. A healthy but quiet mic does not fail.
///
/// Mutation: making `is_failed` unread in `pump_once` (dropping the
/// `n.is_zero() && self.src.is_failed()` arm) fails the `Failed` assertion;
/// checking `is_failed` before the frames are drained (dropping `n.is_zero()`)
/// fails the frame count.
#[test]
fn a_dead_mic_fails_the_take_and_keeps_its_frames() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dead.wav");
    let dead = Arc::new(AtomicBool::new(false));
    let mut pump_loop = PumpLoop::new(
        DyingMic {
            queued: frames(10),
            dead: Arc::clone(&dead),
        },
        sink(&path, 2, Pcm::Float32),
        SCRATCH_FRAMES,
    )
    .unwrap();

    assert_eq!(pump_loop.pump_once(), PumpPass::Wrote(Samples(10)));
    assert_eq!(
        pump_loop.pump_once(),
        PumpPass::Starved,
        "quiet is not dead"
    );
    assert!(!pump_loop.failed());

    dead.store(true, Ordering::Release);
    assert_eq!(pump_loop.pump_once(), PumpPass::Failed);
    assert!(pump_loop.failed());

    let err = pump_loop
        .finalize()
        .expect_err("a failed source is not a clean take");
    assert!(
        matches!(RecorderError::from_take(err), RecorderError::SourceFailed),
        "typed, not a bare io error"
    );
    let reader = hound::WavReader::open(&path).expect("the prefix is a readable file");
    assert_eq!(reader.len(), 20, "the ten delivered frames are kept");
}

/// Frames still buffered when the device dies are written before the take is
/// declared failed.
///
/// Mutation: see above (dropping `n.is_zero() &&`) fails this one.
#[test]
fn frames_buffered_at_the_fault_are_written_first() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("buffered.wav");
    let mut pump_loop = PumpLoop::new(
        DyingMic {
            queued: frames(5),
            dead: Arc::new(AtomicBool::new(true)),
        },
        sink(&path, 2, Pcm::Float32),
        SCRATCH_FRAMES,
    )
    .unwrap();
    assert_eq!(pump_loop.pump_once(), PumpPass::Wrote(Samples(5)));
    assert_eq!(pump_loop.pump_once(), PumpPass::Failed);
    assert!(pump_loop.finalize().is_err());
    assert_eq!(hound::WavReader::open(&path).unwrap().len(), 10);
}

/// A sink that fails mid-take (here the WAV frame limit, the same latch a full
/// disk sets) fails the take, and the sink's error still comes out of
/// finalize.
///
/// Mutation: deleting the `self.sink.is_failed()` arm in `pump_once` fails the
/// `Failed` assertion; deleting `FileOut::is_failed` does too.
#[test]
fn a_failed_sink_fails_the_take() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("full.wav");
    let wav = sink(&path, 2, Pcm::Float32).limit_frames(4);
    let mut pump_loop = PumpLoop::new(
        DyingMic {
            queued: frames(10),
            dead: Arc::new(AtomicBool::new(false)),
        },
        wav,
        SCRATCH_FRAMES,
    )
    .unwrap();
    assert_eq!(pump_loop.pump_once(), PumpPass::Failed);
    assert!(pump_loop.failed());
    let err = pump_loop.finalize().expect_err("the sink's error");
    assert!(err.get_ref().is_some());
    assert_eq!(hound::WavReader::open(&path).unwrap().len(), 8);
}

/// A source that pushes its last frame and disconnects between a poll that came
/// back empty and the failure check: poll 1 is empty, then the late frame lands
/// and the flag flips.
struct RacingMic {
    polls: usize,
    failed: AtomicBool,
}

impl AudioIn for RacingMic {
    const ON_EMPTY: OnEmpty = OnEmpty::Starved;

    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }

    fn poll_into(&mut self, out: &mut [f32]) -> Samples {
        self.polls += 1;
        match self.polls {
            // Empty; the producer's last push and the disconnect follow.
            1 => {
                self.failed.store(true, Ordering::Release);
                Samples::ZERO
            }
            2 => {
                out[..2].copy_from_slice(&[0.5, -0.5]);
                Samples(1)
            }
            _ => Samples::ZERO,
        }
    }

    fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

/// The frame pushed just before the disconnect is recorded, not lost.
///
/// Mutation: failing on the first empty poll that sees `is_failed` (removing
/// the second `pump` in `pump_once`) returns `Failed` first and fails this.
#[test]
fn a_frame_pushed_just_before_the_fault_is_not_lost() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.wav");
    let mut pump_loop = PumpLoop::new(
        RacingMic {
            polls: 0,
            failed: AtomicBool::new(false),
        },
        sink(&path, 2, Pcm::Float32),
        SCRATCH_FRAMES,
    )
    .unwrap();
    assert_eq!(pump_loop.pump_once(), PumpPass::Wrote(Samples(1)));
    assert_eq!(pump_loop.pump_once(), PumpPass::Failed);
    let _ = pump_loop.finalize();
    assert_eq!(hound::WavReader::open(&path).unwrap().len(), 2);
}

/// `Failed` outranks `Ended`: a finite source that failed and drained is a
/// failure.
#[test]
fn a_failed_finite_source_is_failed_not_ended() {
    struct DeadFile;
    impl AudioIn for DeadFile {
        const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;
        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }
        fn poll_into(&mut self, _: &mut [f32]) -> Samples {
            Samples::ZERO
        }
        fn is_failed(&self) -> bool {
            true
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let wav = sink(&dir.path().join("f.wav"), 2, Pcm::Float32);
    let mut pump_loop = PumpLoop::new(DeadFile, wav, SCRATCH_FRAMES).unwrap();
    assert_eq!(pump_loop.pump_once(), PumpPass::Failed);
}

/// A source that reports `lost` frames once per pass: the loop sums them.
///
/// Mutation: deleting the `fetch_add` in `pump_once` leaves `lost()` at zero.
#[test]
fn the_loop_sums_what_the_source_lost() {
    struct Dropping;
    impl AudioIn for Dropping {
        const ON_EMPTY: OnEmpty = OnEmpty::Starved;
        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }
        fn poll_into(&mut self, _: &mut [f32]) -> Samples {
            Samples::ZERO
        }
        fn take_lost(&mut self) -> Samples {
            Samples(7)
        }
    }
    struct F32Collect;
    impl AudioOut for F32Collect {
        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }
        fn write(&mut self, _: &[f32]) {}
        fn finalize(self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut pump_loop = PumpLoop::new(Dropping, F32Collect, SCRATCH_FRAMES).unwrap();
    pump_loop.pump_once();
    pump_loop.pump_once();
    assert_eq!(pump_loop.lost(), Samples(14));
}
