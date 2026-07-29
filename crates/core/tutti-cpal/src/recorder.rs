//! [`Recorder`] — the live driver that turns a [`MicIn`] into a WAV file.
//!
//! Recording is `pump(mic, wav)`: poll a block of frames from the
//! [`AudioIn`](tutti_core::io::AudioIn) mic, write it to the
//! [`AudioOut`](tutti_core::io::AudioOut) WAV sink, repeat. This is the caller
//! side of [`tutti_core::io::pump`] — the loop and the stop policy that the pump
//! function itself deliberately leaves out.
//!
//! # Threading
//!
//! The pump runs on its own background thread, NOT `cpal`'s real-time input
//! callback (that thread only ever `try_push`es into [`MicIn`]'s ring; see
//! [`mic`](crate::MicIn)). The pump thread owns both the `MicIn` and the
//! [`WavOut`] outright, so [`finalize`](tutti_core::io::AudioOut::finalize) —
//! which consumes the sink by value and can happen only once — has a clear home:
//! the thread breaks its loop on the stop flag, finalizes, and returns the
//! `io::Result`, which [`stop`](Recorder::stop) recovers by joining.
//!
//! The scratch buffer is allocated once before the loop; the loop body never
//! allocates. A live mic frequently has nothing ready (the ring hasn't filled
//! since the last poll), so an empty [`pump`](tutti_core::io::pump) parks briefly
//! rather than busy-spinning a core.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tutti_sampler::capture::CaptureFormat;
use tutti_sampler::{pump, AudioOut, WavOut};

use crate::error::{Error, Result};
use crate::mic::MicIn;

/// Frames moved per pump pass. One bufferful, allocated once before the loop so
/// the pump body stays allocation-free. ~21ms at 48kHz — small enough to bound
/// how far the sink can lag the ring, large enough to amortize per-pass cost.
const SCRATCH_FRAMES: usize = 1024;

/// How long to park the pump thread when a pass moved zero frames. A live mic
/// has nothing ready between callback pushes; sleeping avoids busy-spinning a
/// core while still draining the ring far faster than it can overrun.
const IDLE_PARK: Duration = Duration::from_millis(5);

/// A live microphone→WAV recorder.
///
/// [`start`](Self::start) opens the input device, creates a [`WavOut`] at the
/// mic's native sample rate, and spawns a background thread that pumps mic
/// frames into the sink. [`stop`](Self::stop) signals that thread to finish,
/// joins it, and finalizes the WAV — the once-only close is guaranteed because
/// the sink is owned by the pump thread and `stop` takes `self` by value.
pub struct Recorder {
    /// Cleared by [`stop`](Self::stop) to break the pump loop.
    running: Arc<AtomicBool>,
    /// The pump thread; its closure returns the `finalize` result. `Option` so
    /// [`stop`](Self::stop) can `take` it out to join.
    handle: Option<JoinHandle<std::io::Result<()>>>,
}

impl Recorder {
    /// Open `device_index` (or the default input device when `None`), create a
    /// stereo 32-bit-float WAV at `path` matching the mic's native sample rate,
    /// and spawn the background pump thread. Returns once recording is live.
    pub fn start(path: PathBuf, device_index: Option<usize>) -> Result<Self> {
        let mut mic = MicIn::open(device_index)?;
        // The sink's header must match the frames it's fed: same rate as the
        // mic, stereo (the shape `MicIn` produces), float (the simple path).
        let mut wav = WavOut::create(&path, mic.sample_rate(), 2, CaptureFormat::F32)
            .ok_or_else(|| Error::InvalidConfig(format!("Cannot create WAV file: {path:?}")))?;

        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);

        let handle = std::thread::spawn(move || {
            // Allocated once; the loop body below never allocates.
            let mut scratch = [[0.0f32; 2]; SCRATCH_FRAMES];
            while thread_running.load(Ordering::Acquire) {
                if pump(&mut mic, &mut wav, &mut scratch) == 0 {
                    // Nothing in the ring yet — back off instead of spinning.
                    std::thread::sleep(IDLE_PARK);
                }
            }
            // Loop broken by `stop`: finalize exactly once, here, where the sink
            // is owned. The result rides the joined thread back to `stop`.
            wav.finalize()
        });

        Ok(Self {
            running,
            handle: Some(handle),
        })
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
            WavOut::create(&path, 48_000.0, 2, CaptureFormat::F32).expect("sink should open");

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
}
