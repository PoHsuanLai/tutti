//! Doc 025 cut 5, end to end: what a stream reports ([`StreamStatus`]), a
//! panic contained to its stream, a fallible construction, and the decode
//! lane. The status cell's own table is `status.rs`'s and the step's is
//! `record/tests.rs`'s; these drive a streamer, hand-stepped and threaded.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tutti_types::{Beat, Bpm};

use super::command::StreamId;
use super::decode::Seams;
use super::status::{StreamFault, StreamPhase};
use super::*;
use crate::testing::{MockTransport, play, prepared, settle, settle_threaded};
use crate::{DiskStreamerConfig, DiskVoice, OpenError};

const WAIT: Duration = Duration::from_secs(10);

/// A stereo float WAV of `seconds` at `rate`, every sample `value` (a dyadic
/// value is read back exactly).
fn constant_file(dir: &Path, name: &str, rate: u32, seconds: u32, value: f32) -> PathBuf {
    let path = dir.join(name);
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&path, spec).expect("a writable temp dir");
    for _ in 0..rate * seconds * 2 {
        w.write_sample(value).expect("writes");
    }
    w.finalize().expect("writes");
    path
}

/// The repo's FLAC fixture cut to its first 90%: it opens, reports its whole
/// length, and fails to decode past the cut.
fn cut_flac(dir: &Path) -> PathBuf {
    let bytes = std::fs::read(tutti_fixtures::asset("audio/stereo.flac")).expect("fixture");
    let path = dir.join("cut.flac");
    std::fs::write(&path, &bytes[..bytes.len() * 9 / 10]).expect("write");
    path
}

/// The two drivers, so one case runs on both.
enum Driver {
    Hand(Box<DiskStreamer<ByHand>>),
    Threaded(DiskStreamer<Running>),
}

impl Driver {
    fn start(rate: f64, threaded: bool, seams: Seams) -> Self {
        let rate = tutti_types::SampleRate(rate);
        let config = DiskStreamerConfig::default();
        if threaded {
            Self::Threaded(DiskStreamer::build(rate, config, seams).expect("the thread starts"))
        } else {
            Self::Hand(Box::new(
                DiskStreamer::manual_with(rate, config, seams).expect("builds"),
            ))
        }
    }

    /// Opens `path` and waits for the stream to be primed.
    fn open(&mut self, path: &Path) -> (Stream, StreamReader) {
        let opened = match self {
            Self::Hand(s) => {
                let opening = s.open(path, Placement::HEAD);
                settle(s.as_mut(), opening).expect("the file opens")
            }
            Self::Threaded(s) => {
                settle_threaded(s.open(path, Placement::HEAD)).expect("the file opens")
            }
        };
        self.until("the stream to be primed", || {
            opened.0.status().phase != StreamPhase::Opening
        });
        opened
    }

    /// Runs the streamer (a step, or a wait for its thread) until `done`, or
    /// fails after [`WAIT`].
    fn until(&mut self, what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < WAIT, "gave up waiting for {what}");
            match self {
                Self::Hand(s) => {
                    let _ = s.step_once();
                }
                Self::Threaded(_) => {}
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Mutation record is on each test; this is the lane's gate for a test that
/// holds a decode open.
#[derive(Clone)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn closed() -> Self {
        Self(Arc::new((Mutex::new(false), Condvar::new())))
    }

    /// The hook that waits at the gate.
    fn hook(&self) -> super::decode::DecodeHook {
        let gate = Arc::clone(&self.0);
        Arc::new(move |_path| {
            let (open, bell) = &*gate;
            let mut open = open.lock().unwrap();
            while !*open {
                open = bell.wait(open).unwrap();
            }
        })
    }

    fn open(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

/// Releases the gate however the test ends, so a failure does not leave a
/// streamer thread waiting for good. A test holds a clone declared after its
/// streamer, so the gate opens before the streamer's drop joins its thread.
impl Drop for Gate {
    fn drop(&mut self) {
        self.open();
    }
}

// ---------------------------------------------------------------------------
// Status.
// ---------------------------------------------------------------------------

/// **A decode fault is `Faulted(Decode)` and the voice reads silence from
/// it.** A FLAC cut at 90% opens and reports its length; the stream is `Live`
/// until the refill reaches the cut, then faulted, and a voice playing it
/// sounds the frames before the cut and nothing after.
///
/// Mutation (run): `Record::settle` not publishing the decode fault (the
/// `fault(Decode)` call removed) -> the stream stays `Live` -> fails.
#[test]
fn a_decode_fault_is_faulted_and_the_voice_reads_silence() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = cut_flac(dir.path());
    let meta = tutti_codec::Wave::read_metadata(&path).expect("the cut file has a header");
    let rate = f64::from(meta.sample_rate);
    let total = meta.total_frames.expect("flac reports a length") as usize;

    let mut sampler =
        DiskStreamer::manual(SampleRate::new(rate), Default::default()).expect("builds");
    let opening = sampler.open(&path, Placement::HEAD);
    let (stream, reader) = settle(&mut sampler, opening).expect("a cut file opens");
    assert_eq!(
        stream.status().phase,
        StreamPhase::Faulted(StreamFault::Decode)
    );

    let mut voice = prepared(
        DiskVoice::from_reader(reader, Beat(0.0), None),
        SampleRate::new(rate),
        512,
    );
    let clock = MockTransport::rolling(Beat(0.0), Bpm::of(120.0));
    let out = play(&mut voice, &clock, SampleRate::new(rate), total, 512);
    let kept = out[0].iter().rposition(|&s| s != 0.0).map_or(0, |i| i + 1);
    assert!(
        kept > 1_000 && kept < total,
        "kept {kept} of {total} frames"
    );
    assert!(
        out.iter().all(|c| c[kept..].iter().all(|&s| s == 0.0)),
        "the voice sounded past the fault"
    );
}

/// **The end of the file is `Ended`, and a looped stream never is.** A short
/// file is wholly in its ring once primed; a loop leaves the stream `Live`.
///
/// Mutation (run): `Record::settle` never publishing `Ended` -> the end
/// assertion fails. Mutation (run): `Ended` decided without the loop (the
/// window reaching the file's length, whatever the mapping) -> the looped
/// stream reads `Ended` -> fails.
#[test]
fn the_end_of_the_file_is_ended_and_a_loop_has_none() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = constant_file(dir.path(), "short.wav", 8_000, 1, 0.25);
    let mut sampler =
        DiskStreamer::manual(SampleRate::new(8_000.0), Default::default()).expect("builds");
    let opening = sampler.open(&path, Placement::HEAD);
    let (mut stream, mut reader) = settle(&mut sampler, opening).expect("opens");
    assert_eq!(
        stream.status().phase,
        StreamPhase::Live,
        "primed, with the file's end still ahead of the window"
    );

    // The reader plays near the end: the refill appends the last frames.
    drop(reader.pos.claim(7_900, [(7_900, 7_904), (0, 0)]));
    let _ = sampler.step_until_settled(1_000);
    assert_eq!(stream.status().phase, StreamPhase::Ended);

    stream
        .set_loop(crate::LoopSetting::On {
            start: tutti_types::SamplePosition(1_000.0),
            end: tutti_types::SamplePosition(4_000.0),
            crossfade_frames: 0,
        })
        .expect("the streamer lives");
    let _ = sampler.step_until_settled(1_000);
    assert_eq!(
        stream.status().phase,
        StreamPhase::Live,
        "a looped stream has no end"
    );
}

/// **Underruns are read from the status**, the first reader of what the voice
/// counts: the total grows with what the audio side reports, and reading it
/// does not lose a count.
///
/// Mutation (run): `Stream::status` answering `underruns: 0` -> fails.
/// Mutation (run): the drained count not added to the total (the read taking
/// and discarding it) -> the second read falls back to what is new -> fails.
#[test]
fn underruns_are_read_from_the_status() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = constant_file(dir.path(), "a.wav", 8_000, 1, 0.25);
    let mut sampler =
        DiskStreamer::manual(SampleRate::new(8_000.0), Default::default()).expect("builds");
    let opening = sampler.open(&path, Placement::HEAD);
    let (stream, reader) = settle(&mut sampler, opening).expect("opens");
    assert_eq!(stream.status().underruns, 0);

    for _ in 0..3 {
        reader.rt_state.report_underrun();
    }
    assert_eq!(stream.status().underruns, 3);
    assert_eq!(stream.status().underruns, 3, "a read does not consume it");
    reader.rt_state.report_underrun();
    reader.rt_state.report_underrun();
    assert_eq!(stream.status().underruns, 5, "the total counts on");
}

/// **A dropped streamer publishes `Faulted(StreamerGone)` to a live stream**,
/// hand-stepped and threaded.
///
/// Mutation (run): `Drop for Record` without its `fault(StreamerGone)` -> the
/// stream still reads `Live` -> fails, on both drivers.
#[test]
fn a_dropped_streamer_publishes_streamer_gone() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = constant_file(dir.path(), "a.wav", 8_000, 1, 0.25);
    for threaded in [false, true] {
        let mut driver = Driver::start(8_000.0, threaded, Seams::default());
        let (stream, _reader) = driver.open(&path);
        assert!(
            matches!(
                stream.status().phase,
                StreamPhase::Live | StreamPhase::Ended
            ),
            "setup: a served stream (threaded: {threaded})"
        );
        drop(driver);
        assert_eq!(
            stream.status().phase,
            StreamPhase::Faulted(StreamFault::StreamerGone),
            "threaded: {threaded}"
        );
    }
}

// ---------------------------------------------------------------------------
// Containment (B5).
// ---------------------------------------------------------------------------

/// **A panicking decode faults only its stream, and the others keep filling**
/// (B5), on the hand-stepped and the threaded driver, with two streams (the
/// serial refill) and three (the parallel one). The hook on `RegionOut` panics
/// where stream 1 reads its file once armed; every stream's reader then plays
/// near the end of a file longer than its ring, so each one's refill has
/// frames to read.
///
/// Mutation (run): `Record::contained` without its `catch_unwind` (running the
/// work directly) -> the hand-stepped case panics through the step, and on the
/// threaded driver the thread dies and the survivors never fill (nor does the
/// faulted stream's status read anything but `StreamerGone`) -> fails on both.
fn a_panicking_decode_faults_only_its_stream(threaded: bool) {
    const RATE: u32 = 4_000;
    // 40 s against a 30 s ring: a reader near the end needs reads.
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = constant_file(dir.path(), "long.wav", RATE, 40, 0.25);
    let near_end = u64::from(RATE) * 38;

    for streams in [2usize, 3] {
        let armed = Arc::new(AtomicBool::new(false));
        let hook_armed = Arc::clone(&armed);
        let seams = Seams {
            read_hook: Some(Arc::new(move |id, _at| {
                if id == StreamId(1) && hook_armed.load(Ordering::SeqCst) {
                    panic!("test hook: the decoder panicked");
                }
            })),
            ..Seams::default()
        };
        let mut driver = Driver::start(f64::from(RATE), threaded, seams);
        let mut open: Vec<_> = (0..streams).map(|_| driver.open(&path)).collect();
        assert_eq!(open[0].1.id(), StreamId(1), "setup: the first open is 1");
        let before: Vec<u64> = open.iter().map(|(_, r)| r.ring.window().1).collect();

        armed.store(true, Ordering::SeqCst);
        for (_, reader) in &mut open {
            drop(
                reader
                    .pos
                    .claim(near_end, [(near_end, near_end + 4), (0, 0)]),
            );
        }

        let status = |open: &[(Stream, StreamReader)], i: usize| open[i].0.status().phase;
        driver.until("the faulted stream and the others' refills", || {
            status(&open, 0) == StreamPhase::Faulted(StreamFault::Panicked)
                && (1..streams).all(|i| open[i].1.ring.window().1 > before[i])
        });
        for i in 1..streams {
            assert!(
                !matches!(status(&open, i), StreamPhase::Faulted(_)),
                "stream {} faulted with the one that panicked (threaded: {threaded}, \
                     {streams} streams)",
                i + 1
            );
        }
        // Nothing feeds it any more: its window stays where the failed
        // refill left it.
        let left = open[0].1.ring.window();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            open[0].1.ring.window(),
            left,
            "a faulted stream is not refilled"
        );
    }
}

#[test]
fn a_panicking_decode_faults_only_its_stream_hand_stepped() {
    a_panicking_decode_faults_only_its_stream(false);
}

#[test]
fn a_panicking_decode_faults_only_its_stream_threaded() {
    a_panicking_decode_faults_only_its_stream(true);
}

/// **A panic while a loop is captured faults that stream and no other**: the
/// capture reads the file (a crossfade's lead-in), under the same containment
/// as a refill. Hand-stepped.
///
/// Mutation (run): `handle_loop` calling `capture_loop` directly, not through
/// `Record::contained` -> the step panics -> fails.
#[test]
fn a_panic_while_a_loop_is_captured_faults_that_stream() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = constant_file(dir.path(), "a.wav", 8_000, 2, 0.25);
    let armed = Arc::new(AtomicBool::new(false));
    let hook_armed = Arc::clone(&armed);
    let seams = Seams {
        read_hook: Some(Arc::new(move |id, _at| {
            if id == StreamId(1) && hook_armed.load(Ordering::SeqCst) {
                panic!("test hook: the decoder panicked");
            }
        })),
        ..Seams::default()
    };
    let mut driver = Driver::start(8_000.0, false, seams);
    let (mut first, _r1) = driver.open(&path);
    let (second, _r2) = driver.open(&path);

    armed.store(true, Ordering::SeqCst);
    first
        .set_loop(crate::LoopSetting::On {
            start: tutti_types::SamplePosition(2_000.0),
            end: tutti_types::SamplePosition(6_000.0),
            crossfade_frames: 500,
        })
        .expect("the streamer lives");
    driver.until("the capture to fault its stream", || {
        first.status().phase == StreamPhase::Faulted(StreamFault::Panicked)
    });
    assert_ne!(
        second.status().phase,
        StreamPhase::Faulted(StreamFault::Panicked)
    );
    assert!(
        !matches!(second.status().phase, StreamPhase::Faulted(_)),
        "the other stream faulted too"
    );
}

// ---------------------------------------------------------------------------
// Fallible construction.
// ---------------------------------------------------------------------------

fn refuse(
    _name: &str,
    _body: Box<dyn FnOnce() + Send>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    Err(std::io::Error::other("no threads"))
}

/// **`DiskStreamer::new` returns `Err` when the spawn fails**, where it used
/// to panic: the streamer's thread is refused through the spawn seam.
///
/// Mutation (run): `Built::start` mapping the failure to a panic (`.expect`)
/// -> the test panics -> fails.
#[test]
fn a_refused_spawn_is_an_error() {
    let seams = Seams {
        spawn: refuse,
        ..Seams::default()
    };
    let built = DiskStreamer::build(
        tutti_types::SampleRate(48_000.0),
        DiskStreamerConfig::default(),
        seams,
    );
    assert!(
        matches!(&built, Err(crate::Error::Spawn(e)) if e.to_string() == "no threads"),
        "{:?}",
        built.map(|_| ())
    );
}

// ---------------------------------------------------------------------------
// The decode lane.
// ---------------------------------------------------------------------------

/// The repo's mp3 with the marker of its length (the `Info` tag of its first
/// frame) overwritten, so the container reports no frame count: a file that
/// cannot seek, which a stream decodes whole. Pinned as such, or the lane
/// tests prove nothing.
#[cfg(feature = "mp3")]
fn mp3(dir: &Path) -> PathBuf {
    let mut bytes = std::fs::read(tutti_fixtures::asset("audio/stereo.mp3")).expect("fixture");
    let at = bytes
        .windows(4)
        .position(|w| w == b"Info")
        .expect("the fixture carries an Info tag");
    bytes[at..at + 4].copy_from_slice(b"XXXX");
    let path = dir.join("no_length.mp3");
    std::fs::write(&path, &bytes).expect("write");
    assert!(
        !crate::probe(&path).expect("a probe").streamable,
        "setup: the mp3 must be a file the streamer decodes whole"
    );
    path
}

/// **A file that cannot seek opens through the decode lane and plays what
/// the file holds**, hand-stepped and threaded: the first frames of its ring
/// are the decoded file's.
///
/// Mutation (run): the decode lane answering with a silent wave (a `Wave::new`
/// of the right shape) -> the ring holds zeros -> fails.
#[cfg(feature = "mp3")]
#[test]
fn a_file_that_cannot_seek_opens_through_the_decode_lane() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = mp3(dir.path());
    let wave = tutti_codec::Wave::read(&path).expect("the fixture decodes");
    for threaded in [false, true] {
        let mut driver = Driver::start(wave.sample_rate().get(), threaded, Seams::default());
        let (stream, mut reader) = driver.open(&path);
        assert!(matches!(
            stream.status().phase,
            StreamPhase::Live | StreamPhase::Ended
        ));
        let claim = reader.pos.claim(0, [(0, 8_000), (0, 0)]);
        let mut sounded = false;
        for s in 0..8_000u64 {
            assert!(claim.holds(s), "frame {s} (threaded: {threaded})");
            for c in 0..2 {
                let want = wave.at(c, s as usize);
                assert_eq!(claim.sample(s, c), want, "frame {s} channel {c}");
                sounded |= want != 0.0;
            }
        }
        assert!(sounded, "setup: the fixture's first frames are not silence");
    }
}

/// **A non-seekable file opens while another stream keeps refilling**: the
/// decode is held open at a gate on the decode thread, and a stream on the
/// shared thread is still refilled meanwhile; the open then completes when the
/// gate does.
///
/// Mutation (run): the decode run inline on the streamer's thread (the hook
/// and `load_wave` called in `begin` in place of the lane) -> the gate holds
/// the shared thread, the other stream is never refilled -> fails at the bound.
#[cfg(feature = "mp3")]
#[test]
fn a_decode_held_open_does_not_stop_another_stream() {
    const RATE: u32 = 4_000;
    let dir = tempfile::tempdir().expect("a temp dir");
    let long = constant_file(dir.path(), "long.wav", RATE, 40, 0.25);
    let near_end = u64::from(RATE) * 38;
    let gate = Gate::closed();
    let seams = Seams {
        decode_hook: Some(gate.hook()),
        ..Seams::default()
    };
    let mut driver = Driver::start(f64::from(RATE), true, seams);
    let _release = gate.clone();
    let (_busy, mut reader) = driver.open(&long);
    let before = reader.ring.window().1;

    let Driver::Threaded(streamer) = &driver else {
        unreachable!()
    };
    let mut opening = streamer.open(&mp3(dir.path()), Placement::HEAD);
    // Let the streamer reach the decode, which holds at the gate.
    std::thread::sleep(Duration::from_millis(50));
    opening = match opening.poll() {
        Open::Pending(again) => again,
        other => panic!("the decode is held, yet the open answered: {other:?}"),
    };

    drop(
        reader
            .pos
            .claim(near_end, [(near_end, near_end + 4), (0, 0)]),
    );
    driver.until("the other stream's refill, with the decode held", || {
        reader.ring.window().1 > before
    });
    assert!(
        matches!(opening.poll(), Open::Pending(_)),
        "the gate is still closed"
    );
    gate.open();
}

/// **A decode thread that cannot start fails the open with `Io`**, where the
/// open would wait for good: the lane's spawn is refused through the seam
/// (the streamer's own thread is not started: hand-stepped).
///
/// Mutation (run): `DecodeLane::submit` swallowing the start error (the open
/// pushed as pending anyway) -> the open is never answered -> fails at the
/// bound.
#[cfg(feature = "mp3")]
#[test]
fn a_refused_decode_thread_fails_the_open() {
    let seams = Seams {
        spawn: refuse,
        ..Seams::default()
    };
    let mut sampler =
        DiskStreamer::manual_with(tutti_types::SampleRate(44_100.0), Default::default(), seams)
            .expect("builds");
    let dir = tempfile::tempdir().expect("a temp dir");
    let opening = sampler.open(&mp3(dir.path()), Placement::HEAD);
    let answer = settle(&mut sampler, opening);
    assert!(
        matches!(&answer, Err(OpenError::Io(std::io::ErrorKind::Other))),
        "{:?}",
        answer.map(|_| ())
    );
}

/// **A panic in the decode is that open's failure, not the lane's**: the
/// first open of a file whose decode panics fails with `Format`, and the next
/// open through the same lane succeeds.
///
/// Mutation (run): the lane's `catch_unwind` removed -> the decode thread dies
/// and the first open is never answered -> fails at the bound.
#[cfg(feature = "mp3")]
#[test]
fn a_panicking_decode_fails_that_open_only() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = mp3(dir.path());
    let first = Arc::new(AtomicBool::new(true));
    let seams = Seams {
        decode_hook: Some(Arc::new(move |_path| {
            if first.swap(false, Ordering::SeqCst) {
                panic!("test hook: the decoder panicked");
            }
        })),
        ..Seams::default()
    };
    let mut sampler =
        DiskStreamer::manual_with(tutti_types::SampleRate(44_100.0), Default::default(), seams)
            .expect("builds");
    let opening = sampler.open(&path, Placement::HEAD);
    let answer = settle(&mut sampler, opening);
    assert!(
        matches!(&answer, Err(OpenError::Format)),
        "{:?}",
        answer.map(|_| ())
    );
    let opening = sampler.open(&path, Placement::HEAD);
    settle(&mut sampler, opening).expect("the lane decodes the next file");
}
