//! [`Recorder`] — the background pump that drives any live source into a WAV.
//!
//! Recording is `pump(src, wav)`: poll a block of frames from an
//! [`AudioIn`], write it to the
//! [`AudioOut`] WAV sink, repeat. This is the caller
//! side of [`tutti_core::io::pump`] — the loop and the stop policy that the pump
//! function itself deliberately leaves out.
//!
//! # It takes a source, not a device
//!
//! [`start`](Recorder::start) is generic over the [`AudioIn`], so a microphone
//! is one option among several: a socket, a decoded file, or a generated signal
//! record identically. That is what keeps this crate device-free and lets it sit
//! *below* `tutti-cpal` rather than inside it — a host opens its own `MicIn` and
//! hands it over.
//!
//! # Threading
//!
//! The pump runs on its own thread, never a shared pool. It does not run to
//! completion — a live take ends when someone stops it — so occupying a pool
//! slot would starve every other job on it. The thread owns both endpoints
//! outright, which is what gives
//! [`finalize`](tutti_core::io::AudioOut::finalize) a clear home: it consumes
//! the sink by value and may run only once, so the thread breaks its loop on the
//! stop flag, finalizes, and returns the `io::Result` that
//! [`stop`](Recorder::stop) recovers by joining.
//!
//! The scratch buffer is allocated once before the loop; the loop body never
//! allocates. Whether an empty poll means "back off" or "we are done" is the
//! source's own [`ON_EMPTY`](tutti_core::io::AudioIn::ON_EMPTY) — a mic parks
//! and retries, a file finishes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tutti_core::io::{pump, AudioIn, AudioOut, OnEmpty};

use crate::wav_out::WavOut;

/// Frames moved per pump pass. One bufferful, allocated once before the loop so
/// the pump body stays allocation-free. ~21ms at 48kHz — small enough to bound
/// how far the sink can lag the source, large enough to amortize per-pass cost.
const SCRATCH_FRAMES: usize = 1024;

/// How long to park the pump thread when a [`Starved`](OnEmpty::Starved) source
/// yields nothing. A live mic has nothing ready between callback pushes;
/// sleeping avoids busy-spinning a core while still draining the ring far
/// faster than it can overrun.
const IDLE_PARK: Duration = Duration::from_millis(5);

/// A live source→WAV recorder.
///
/// [`start`](Self::start) spawns a background thread pumping `src` into a
/// [`WavOut`]. [`stop`](Self::stop) signals that thread to finish, joins it, and
/// returns the finalize result — the once-only close is guaranteed because the
/// sink is owned by the pump thread and `stop` takes `self` by value.
///
/// ```no_run
/// # use tutti_io::{Recorder, WavOut, BitDepth};
/// # fn go<I: tutti_core::io::AudioIn + Send + 'static>(src: I) -> std::io::Result<()> {
/// let wav = WavOut::create(&"take.wav".into(), 48_000.0, 2, BitDepth::Float32)
///     .expect("sink opens");
/// let rec = Recorder::start(src, wav);
/// // ... later ...
/// rec.stop()
/// # }
/// ```
pub struct Recorder {
    /// Cleared by [`stop`](Self::stop) to break the pump loop.
    running: Arc<AtomicBool>,
    /// The pump thread; its closure returns the `finalize` result. `Option` so
    /// [`stop`](Self::stop) can `take` it out to join.
    handle: Option<JoinHandle<std::io::Result<()>>>,
}

impl Recorder {
    /// Spawn a background thread pumping `src` into `sink`. Returns once
    /// recording is live.
    ///
    /// The sink's header must already match what the source produces — same
    /// rate, same channel count. A caller opening a mic reads
    /// `MicIn::sample_rate()` and builds the `WavOut` from it; that pairing is
    /// the caller's because only it knows both halves.
    pub fn start<I>(mut src: I, mut sink: WavOut) -> Self
    where
        I: AudioIn + Send + 'static,
    {
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);

        let handle = std::thread::spawn(move || {
            // Allocated once; the loop body below never allocates.
            let mut scratch = [[0.0f32; 2]; SCRATCH_FRAMES];
            while thread_running.load(Ordering::Acquire) {
                if pump(&mut src, &mut sink, &mut scratch) == 0 {
                    match I::ON_EMPTY {
                        // Live source: the producer has not caught up.
                        OnEmpty::Starved => std::thread::sleep(IDLE_PARK),
                        // Finite source: there is no more to read.
                        OnEmpty::EndOfStream => break,
                    }
                }
            }
            // Finalize exactly once, here, where the sink is owned. The result
            // rides the joined thread back to `stop`.
            sink.finalize()
        });

        Self {
            running,
            handle: Some(handle),
        }
    }

    /// Signal the pump thread to stop, join it, and finalize the WAV. Returns
    /// [`finalize`](tutti_core::io::AudioOut::finalize)'s result — an error here
    /// means the WAV header was left unpatched and the file is unreadable.
    pub fn stop(mut self) -> std::io::Result<()> {
        self.running.store(false, Ordering::Release);
        match self.handle.take() {
            // The thread finalizes the sink and returns that io::Result.
            Some(handle) => handle
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("recording thread panicked"))),
            // Unreachable in practice: `handle` is only taken here, and `stop`
            // consumes `self`. Kept total rather than unwrapping.
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::io::{AudioIn, OnEmpty};
    use tutti_core::pcm::BitDepth;

    /// A finite in-memory [`AudioIn`] standing in for a live mic: hands out its
    /// frames in bounded chunks, returning a short-then-zero count at
    /// end-of-stream. Lets the pump→`WavOut`→readback path be proven without
    /// touching real hardware. Mirrors the `SliceSource` fixture in
    /// `tutti_types::io`'s own tests.
    struct SliceSource {
        frames: Vec<[f32; 2]>,
        pos: usize,
    }

    impl AudioIn for SliceSource {
        const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

        fn poll_into(&mut self, out: &mut [[f32; 2]]) -> usize {
            let n = (self.frames.len() - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.frames[self.pos..self.pos + n]);
            self.pos += n;
            n
        }
    }

    /// Pumping a fake source into a real [`WavOut`] and finalizing yields a WAV
    /// whose frame count matches what was fed — the round trip the `Recorder`
    /// runs, minus only the mic and the thread (both untestable without a
    /// device). Proves `WavOut::create` is reachable from this crate and that
    /// the pump moves every frame into a readable file.
    #[test]
    fn pump_into_wav_sink_round_trips_frame_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recorded.wav");

        let frames: Vec<[f32; 2]> = (0..3000)
            .map(|i| [i as f32 / 3000.0, -(i as f32) / 3000.0])
            .collect();
        let mut src = SliceSource {
            frames: frames.clone(),
            pos: 0,
        };
        let mut wav =
            WavOut::create(&path, 48_000.0, 2, BitDepth::Float32).expect("sink should open");

        // The exact loop the pump thread runs — allocate the scratch once, drain
        // to exhaustion. (No idle-park: the fake source never returns 0 early.)
        let mut scratch = [[0.0f32; 2]; SCRATCH_FRAMES];
        while pump(&mut src, &mut wav, &mut scratch) != 0 {}
        wav.finalize()
            .expect("finalize should back-patch the header");

        let reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 48_000);
        // Two samples (L, R) per stereo frame.
        assert_eq!(reader.len() as usize, frames.len() * 2);
    }

    /// A live (starving) source records until stopped — the composability the
    /// `AudioIn` signature buys.
    ///
    /// The old signature opened a device itself, so this case could not be
    /// written at all: anything that is not a microphone had no way in. The
    /// fixture withholds frames without being exhausted, so a recorder that
    /// mistook an empty poll for end-of-stream would stop early.
    #[test]
    fn a_non_mic_live_source_records_until_stopped() {
        struct Starving {
            polls: std::sync::atomic::AtomicUsize,
        }

        impl AudioIn for Starving {
            const ON_EMPTY: OnEmpty = OnEmpty::Starved;

            fn poll_into(&mut self, out: &mut [[f32; 2]]) -> usize {
                let n = self.polls.fetch_add(1, Ordering::Relaxed);
                // Every other poll is empty — never exhausted.
                if n % 2 == 1 || out.is_empty() {
                    return 0;
                }
                out[0] = [0.25, -0.25];
                1
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generated.wav");
        let wav = WavOut::create(&path, 48_000.0, 2, BitDepth::Float32).expect("sink opens");

        let rec = Recorder::start(
            Starving {
                polls: std::sync::atomic::AtomicUsize::new(0),
            },
            wav,
        );
        // Long enough to cross several idle parks.
        std::thread::sleep(Duration::from_millis(40));
        rec.stop().expect("finalize should succeed");

        let reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
        // Not `> 0`: a single frame would prove only that the thread ran once,
        // which a recorder that stopped at its FIRST empty poll would also
        // satisfy — and that is exactly the bug `Starved` exists to prevent.
        // The fixture yields one frame every *other* poll, so several frames
        // means several empty passes were survived. 40 ms of 5 ms parks allows
        // ~4 yielding polls; 3 is the floor that still holds on a loaded
        // machine.
        let frames = reader.len() as usize / 2; // two samples per stereo frame
        assert!(
            frames >= 3,
            "a starving source must keep being polled ACROSS its empty passes; \
             got {frames} frame(s), which is consistent with stopping at the first"
        );
    }
}
