//! Doc 025 stage 6: the streamer sleeps until a source rings its bell, one
//! case per source (the table in [`bell`](super::bell)'s docs).
//!
//! Every case starts the streamer with a **60 s backstop** and allows 10 s: a
//! streamer that does what the test waits for inside the bound was rung, since
//! nothing else would have woken it for a minute. Each case first waits until
//! the thread is parked, so the doorbell, and not a cycle that happened to be
//! running, is what wakes it. The command source and shutdown are
//! `thread.rs`'s tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tutti_types::{SampleRate, Samples};

use super::bell::Bell;
use super::config::BufferConfig;
use super::contained::constant_file;
use super::decode::Seams;
use super::status::StreamPhase;
use super::thread::Built;
use super::*;
use crate::testing::settle_threaded;

/// A session and file rate low enough that a 40 s file is 160 000 frames and
/// its ring (30 s) is 120 000: smaller than the file, so the window can fall
/// below its threshold and be outrun.
const RATE: u32 = 4_000;
const BOUND: Duration = Duration::from_secs(10);

/// A threaded streamer whose backstop is 60 s, and its bell.
fn sleeper(seams: Seams) -> (DiskStreamer<Running>, Arc<Bell>) {
    let built = Built::new(
        256,
        SampleRate::new(f64::from(RATE)),
        BufferConfig::default(),
        seams,
    )
    .with_backstop(Duration::from_secs(60));
    let bell = Arc::clone(&built.shared().bell);
    (DiskStreamer::from_built(built).expect("starts"), bell)
}

fn until(bound: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < bound {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    done()
}

fn until_asleep(bell: &Bell) {
    assert!(
        until(BOUND, || bell.is_asleep()),
        "the streamer never went to sleep"
    );
}

/// A stream on the long file, primed and the streamer asleep again, with the
/// window it holds.
fn primed(path: &Path) -> (DiskStreamer<Running>, Arc<Bell>, Stream, StreamReader, u64) {
    let (streamer, bell) = sleeper(Seams::default());
    let (stream, reader) = settle_threaded(streamer.open(path, Placement::HEAD)).expect("opens");
    assert!(
        until(BOUND, || stream.status().phase != StreamPhase::Opening),
        "the stream never primed"
    );
    until_asleep(&bell);
    let (from, to) = reader.ring.window();
    assert_eq!(from, 0);
    assert!(
        to < 160_000,
        "setup: the window must not hold the whole file"
    );
    (streamer, bell, stream, reader, to)
}

fn long_file(dir: &Path) -> PathBuf {
    constant_file(dir, "long.wav", RATE, 40, 0.25)
}

fn claim(reader: &mut StreamReader, play: u64) {
    let none = Samples::new(0)..Samples::new(0);
    drop(reader.block(Samples::new(play as usize), [none.clone(), none]));
}

/// **An open wakes a sleeping streamer**: the open's command rings, the
/// streamer answers, well inside the backstop.
#[test]
fn an_open_wakes_a_sleeping_streamer() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = long_file(dir.path());
    let (streamer, bell) = sleeper(Seams::default());
    until_asleep(&bell);
    let _ = settle_threaded(streamer.open(&path, Placement::HEAD)).expect("opens");
}

/// **A ring below its refill threshold wakes a sleeping streamer**: the
/// reader plays on inside its window until fewer than three quarters of the
/// ring are ahead of it, and its block rings; the streamer extends the window.
#[test]
fn a_ring_below_threshold_wakes_a_sleeping_streamer() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (_streamer, _bell, _stream, mut reader, to) = primed(&long_file(dir.path()));
    claim(&mut reader, to / 2);
    assert!(
        until(BOUND, || reader.ring.window().1 > to),
        "the window did not grow: the streamer slept through a ring below threshold"
    );
}

/// **A jump outside the window wakes a sleeping streamer**: the reader seeks
/// far past the window's end, and its block rings; the streamer moves the
/// window to hold it. (`refill_one` used to find this by polling.)
#[test]
fn a_jump_outside_the_window_wakes_a_sleeping_streamer() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (_streamer, _bell, _stream, mut reader, to) = primed(&long_file(dir.path()));
    let play = to + 40_001;
    claim(&mut reader, play);
    assert!(
        until(BOUND, || {
            let (from, to) = reader.ring.window();
            from <= play && play < to
        }),
        "the window did not follow the jump: the streamer slept through it"
    );
}

/// What holds a decode open until the test releases it, and says when it got
/// there.
struct Gate {
    entered: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            changed: Condvar::new(),
        })
    }

    fn hook(self: &Arc<Self>) -> super::decode::DecodeHook {
        let gate = Arc::clone(self);
        Arc::new(move |_path| {
            gate.entered.store(true, Ordering::SeqCst);
            let held = gate.released.lock().unwrap();
            let _ = gate
                .changed
                .wait_timeout_while(held, Duration::from_secs(30), |released| !*released)
                .unwrap();
        })
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

/// **The decode lane's answer wakes a sleeping streamer**: the open of a file
/// that cannot seek is handed to the lane, the streamer goes back to sleep with
/// the decode held, and the lane's reply (a command on the streamer's own
/// queue) rings it.
#[cfg(feature = "mp3")]
#[test]
fn a_decode_reply_wakes_a_sleeping_streamer() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = super::contained::mp3(dir.path());
    let gate = Gate::new();
    let (streamer, bell) = sleeper(Seams {
        decode_hook: Some(gate.hook()),
        ..Seams::default()
    });
    let opening = streamer.open(&path, Placement::HEAD);
    assert!(
        until(BOUND, || gate.entered.load(Ordering::SeqCst)),
        "the lane never started the decode"
    );
    until_asleep(&bell);
    gate.release();
    let _ = settle_threaded(opening).expect("opens");
}

/// **A voice's ring is allocation-free and wait-free on the audio thread**: a
/// reader that asks for the streamer on every block (it alternates between a
/// seek past the window and one back before it, so each block finds the ring
/// wanting a refill) claims two thousand blocks inside `assert_no_alloc`, with
/// the streamer thread parked on the bell for the first of them and running for
/// the rest (so both the unpark and the already-awake path are taken). The
/// allocator's trap covers frees too.
#[test]
fn a_voices_ring_is_allocation_free() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (_streamer, _bell, _stream, mut reader, to) = primed(&long_file(dir.path()));
    let far = to + 40_001;
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            claim(&mut reader, if i % 2 == 0 { far } else { 0 });
        }
    });
}
