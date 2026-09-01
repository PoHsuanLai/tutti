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
//! # Dropping is a shutdown, not a leak
//!
//! Ending a take is the *only* way `finalize` runs, so it cannot be left to a
//! caller remembering to ask. [`Drop`] performs the same stop-and-join
//! [`stop`](Recorder::stop) does — the two share one `shutdown`, and the taken
//! `JoinHandle` is what stops them joining twice.
//!
//! This is not defensive tidiness. The loop exits on its own only for a finite
//! source ([`EndOfStream`](OnEmpty::EndOfStream) breaks); a
//! [`Starved`](OnEmpty::Starved) one — every microphone capture — parks and
//! re-polls forever. Without the `Drop` impl a dropped live recorder spins a
//! thread for the life of the process and never back-patches the WAV header,
//! which leaves the file unreadable *despite* its audio being on disk.
//!
//! `Drop` cannot return the finalize `Result` and this workspace does not panic
//! in `Drop`, so that path stores its outcome in a
//! [`FinalizeStatus`] handle taken beforehand. [`stop`](Recorder::stop) remains
//! the path that hands the result straight back.
//!
//! The scratch buffer is allocated once before the loop, sized
//! `SCRATCH_FRAMES * channels` samples at the source's own width; the loop body
//! never allocates.
//!
//! Whether a zero-FRAME poll means "back off" or "finished" is the source's
//! own [`ON_EMPTY`](tutti_core::io::AudioIn::ON_EMPTY), and the pump loop is
//! where that const is spent. `AudioIn` unifies live and finite sources, so the
//! same zero carries two meanings: a mic has nothing ready *yet*, a decoded file
//! has nothing ready *ever*. Reading a live source as finite ends a take
//! milliseconds in, with no error anywhere; reading a finite one as live spins
//! the thread forever. Neither is detectable from the count alone, which is why
//! the source declares it.
//!
//! # The width check lives here
//!
//! [`start`](Recorder::start) refuses a source and sink whose channel counts
//! disagree. That check is not incidental bookkeeping: it is the **replacement**
//! for a compile-time guarantee deliberately given up. `AudioIn` and `AudioOut`
//! carry the frame width as a runtime
//! [`ChannelLayout`](tutti_core::ChannelLayout), not a `const CH`, because only
//! a runtime width can express one that comes from *data* — a decoded file, a
//! surround capture device. The cost is that "a stereo mic cannot feed a
//! 6-channel WAV" stopped being a type error. Per the project rule that an
//! omitted guarantee ships with its replacement, it is restored as two runtime
//! checks: a `debug_assert` on the two layouts inside [`pump`], and **the error
//! `start` returns — which lives in this crate.**
//!
//! This is the right home for the checked half because it is the one place both
//! endpoints are in scope *before any frame moves*. A mismatch caught here costs
//! a returned error; the same mismatch caught nowhere writes a file whose
//! channels rotate every frame and reads back as a subtly-wrong recording rather
//! than as a failure.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tutti_core::io::{pump, AudioIn, AudioOut, OnEmpty};

use crate::error::{Error, Result};
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
/// # fn go<I: tutti_core::io::AudioIn + Send + 'static>(src: I) -> tutti_io::Result<()> {
/// let wav = WavOut::create("take.wav", 48_000.0, 2u16, BitDepth::Float32)
///     .expect("sink opens");
/// let rec = Recorder::start(src, wav)?;   // errors if the widths disagree
/// // ... later ...
/// rec.stop()
/// # }
/// ```
pub struct Recorder {
    /// Cleared by [`stop`](Self::stop) to break the pump loop.
    running: Arc<AtomicBool>,
    /// The pump thread; its closure returns the `finalize` result. `Option`
    /// because taking it is what makes the shutdown once-only: both
    /// [`stop`](Self::stop) and [`drop`](Self::drop) go through
    /// [`shutdown`](Self::shutdown), and whichever runs first leaves `None`
    /// behind for the other.
    handle: Option<JoinHandle<std::io::Result<()>>>,
    /// Where a shutdown that cannot return its result leaves it.
    ///
    /// Only [`drop`](Self::drop) writes here — [`stop`](Self::stop) returns the
    /// same value to its caller instead. It is a *shared* cell rather than a
    /// plain field so a caller can hold [`finalize_status`](Self::finalize_status)
    /// across the drop and read the outcome afterwards, which is the only
    /// moment the answer exists on that path.
    ///
    /// `None` means no drop-path finalize has completed: either `stop` was
    /// called, or the recorder is still alive.
    status: Arc<FinalizeStatus>,
}

/// The finalize outcome of a [`Recorder`] shut down by its [`Drop`] impl.
///
/// Obtained from [`Recorder::finalize_status`] *before* the recorder is
/// dropped; reading it afterwards is the point.
///
/// # Why this exists at all
///
/// [`Recorder::stop`] returns the finalize `Result`, and that is the path a
/// caller should take. `Drop` has no return value and this workspace does not
/// panic in `Drop`, so a dropped recorder's finalize error would otherwise
/// vanish — and a failed finalize means an unpatched WAV header, i.e. an
/// unreadable file. This is where that error goes instead of nowhere.
///
/// The crate has no logger and takes no logging dependency, so the outcome is
/// *stored* rather than printed: a caller that cares reads it, one that does
/// not pays an `AtomicBool` and a `Mutex` it never locks.
#[derive(Default)]
pub struct FinalizeStatus {
    /// Set once the drop-path finalize has run, whatever its outcome.
    /// Separate from the message so "succeeded" and "has not happened" are
    /// distinguishable without locking.
    done: AtomicBool,
    /// The error text, when the drop-path finalize failed. Behind a `Mutex`
    /// because `io::Error` is neither `Copy` nor cheap to publish atomically,
    /// and this is written at most once, off any hot path.
    error: std::sync::Mutex<Option<String>>,
}

impl FinalizeStatus {
    /// Whether a drop-path finalize has completed.
    ///
    /// `false` after [`Recorder::stop`]: that path returns the result directly
    /// and deliberately leaves nothing here.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// The drop-path finalize error, if there was one.
    ///
    /// `None` covers two cases that [`is_done`](Self::is_done) separates: the
    /// finalize succeeded, or it has not run.
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Record a completed drop-path finalize.
    fn set(&self, result: std::io::Result<()>) {
        if let Err(e) = result {
            *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
        }
        // Released last, so a reader that observes `is_done` also observes the
        // message written above it.
        self.done.store(true, Ordering::Release);
    }
}

impl Recorder {
    /// Spawn a background thread pumping `src` into `sink`. Returns once
    /// recording is live.
    ///
    /// **Errors if `src` and `sink` disagree on channel width**
    /// ([`Error::ChannelWidthMismatch`], carrying both layouts) — the module
    /// header explains why this check is here and what it replaces.
    ///
    /// The **sample rate** is still the caller's to match: `AudioIn` carries no
    /// rate at all (the trait deliberately has none — a caller that needs one
    /// holds the concrete type), so nothing here can compare them. A caller
    /// opening a mic reads `MicIn::sample_rate()` and builds the `WavOut` from
    /// it, or uses `MicIn::matching_sink`, which pairs both halves at the one
    /// place they are both in scope.
    pub fn start<I>(mut src: I, mut sink: WavOut) -> Result<Self>
    where
        I: AudioIn + Send + 'static,
    {
        let layout = src.layout();
        if layout != AudioOut::layout(&sink) {
            return Err(Error::ChannelWidthMismatch {
                src: layout,
                sink: AudioOut::layout(&sink),
            });
        }
        // Sized once, here, from the width both endpoints agreed on above.
        let scratch_samples = SCRATCH_FRAMES * layout.count().max(1) as usize;

        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);

        let handle = std::thread::spawn(move || {
            // Allocated once; the loop body below never allocates.
            let mut scratch = vec![0.0f32; scratch_samples];
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

        Ok(Self {
            running,
            handle: Some(handle),
            status: Arc::new(FinalizeStatus::default()),
        })
    }

    /// Signal the pump thread to stop, join it, and finalize the WAV. Returns
    /// [`finalize`](tutti_core::io::AudioOut::finalize)'s result — an error here
    /// means the WAV header was left unpatched and the file is unreadable.
    ///
    /// This is the path that hands the result back. [`Drop`] runs the same
    /// shutdown and has nowhere to return it, so it stores it in
    /// [`finalize_status`](Self::finalize_status) instead.
    pub fn stop(mut self) -> Result<()> {
        // `self` is consumed, so the `Drop` that follows finds `handle` already
        // taken and does nothing — the join happens exactly once.
        Ok(self.shutdown()?)
    }

    /// Where a [`Drop`]-path finalize leaves its outcome.
    ///
    /// Take this handle *before* dropping the recorder; it outlives the
    /// recorder, which is what makes the answer readable at all on that path.
    /// A caller using [`stop`](Self::stop) has no use for it — that returns the
    /// same result directly, and leaves this untouched.
    pub fn finalize_status(&self) -> Arc<FinalizeStatus> {
        Arc::clone(&self.status)
    }

    /// Break the pump loop, join the thread, and surface the finalize result.
    ///
    /// The shared body of [`stop`](Self::stop) and [`Drop`]. Taking `handle`
    /// is what makes it once-only: a second call finds `None` and returns
    /// `Ok(())` without joining anything.
    ///
    /// Clearing `running` is the half that matters for a **live** source. The
    /// pump loop exits on its own only for a finite one
    /// ([`OnEmpty::EndOfStream`] breaks); a starving source — every microphone
    /// capture — parks and re-polls forever, so nothing else ever reaches the
    /// `finalize` below it.
    fn shutdown(&mut self) -> std::io::Result<()> {
        self.running.store(false, Ordering::Release);
        match self.handle.take() {
            // The thread finalizes the sink and returns that io::Result.
            Some(handle) => handle
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("recording thread panicked"))),
            None => Ok(()),
        }
    }
}

/// Stop, join and finalize a recorder nobody called [`stop`](Recorder::stop)
/// on.
///
/// Without this, dropping a live recorder leaks the pump thread *and* the
/// take: the loop only breaks on `running`, so a
/// [`Starved`](OnEmpty::Starved) source parks and re-polls forever,
/// `finalize` never runs, and the WAV header is never back-patched — the
/// module header explains why that leaves an unreadable file. The data is on
/// disk; only the header says how much of it there is.
///
/// The finalize `Result` has nowhere to go from here, and this workspace does
/// not panic in `Drop`. It is stored in [`FinalizeStatus`] instead, which a
/// caller reads through a handle taken before the drop.
impl Drop for Recorder {
    fn drop(&mut self) {
        // `stop` consumed `self` and already took `handle`, so this is a no-op
        // on that path and must not overwrite the status it deliberately left
        // clear.
        if self.handle.is_some() {
            let result = self.shutdown();
            self.status.set(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::io::{AudioIn, OnEmpty};
    use tutti_core::pcm::BitDepth;
    use tutti_core::ChannelLayout;

    /// A finite in-memory [`AudioIn`] standing in for a live mic: hands out its
    /// frames in bounded chunks, returning a short-then-zero count at
    /// end-of-stream. Lets the pump→`WavOut`→readback path be proven without
    /// touching real hardware. Mirrors the `SliceSource` fixture in
    /// `tutti_types::io`'s own tests, including its runtime width.
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

        fn poll_into(&mut self, out: &mut [f32]) -> usize {
            let ch = self.layout.count() as usize;
            let total = self.samples.len() / ch;
            let n = (total - self.pos).min(out.len() / ch);
            out[..n * ch].copy_from_slice(&self.samples[self.pos * ch..(self.pos + n) * ch]);
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

        const FRAMES: usize = 3000;
        let samples: Vec<f32> = (0..FRAMES)
            .flat_map(|i| [i as f32 / 3000.0, -(i as f32) / 3000.0])
            .collect();
        let mut src = SliceSource {
            samples,
            pos: 0,
            layout: ChannelLayout::STEREO,
        };
        let mut wav =
            WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink should open");

        // The exact loop the pump thread runs — allocate the scratch once, drain
        // to exhaustion. (No idle-park: the fake source never returns 0 early.)
        let mut scratch = vec![0.0f32; SCRATCH_FRAMES * 2];
        while pump(&mut src, &mut wav, &mut scratch) != 0 {}
        wav.finalize()
            .expect("finalize should back-patch the header");

        let reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 48_000);
        // Two samples (L, R) per stereo frame.
        assert_eq!(reader.len() as usize, FRAMES * 2);
    }

    /// **The replacement for the lost compile error, outer half.**
    ///
    /// A width mismatch is a returned error, checked BEFORE the thread spawns,
    /// so no frame is ever written to a file whose channels would rotate. This
    /// is the guarantee the runtime `ChannelLayout` gave up at the type level.
    ///
    /// The matching pair must still be accepted, which is the half that stops
    /// this from passing vacuously.
    #[test]
    fn start_rejects_a_source_sink_width_mismatch() {
        let dir = tempfile::tempdir().unwrap();

        let stereo_src = || SliceSource {
            samples: vec![0.0f32; 64],
            pos: 0,
            layout: ChannelLayout::STEREO,
        };

        // Stereo source, 6-channel sink: refused.
        let wide_path = dir.path().join("mismatch.wav");
        let wide =
            WavOut::create(&wide_path, 48_000.0, 6u16, BitDepth::Float32).expect("sink opens");
        let err = Recorder::start(stereo_src(), wide)
            .err()
            .expect("a width mismatch must be refused, not recorded");
        // The typed variant carries both layouts as values, so a caller can
        // branch on the widths without parsing the message.
        assert!(
            matches!(
                err,
                Error::ChannelWidthMismatch { src, sink }
                    if src == ChannelLayout::STEREO && sink == ChannelLayout::from(6u16)
            ),
            "expected ChannelWidthMismatch carrying both layouts, got: {err:?}"
        );
        assert!(
            err.to_string().contains("Stereo") && err.to_string().contains("6"),
            "the error must name both widths, got: {err}"
        );

        // Stereo source, mono sink: also refused. A narrowing mismatch is just
        // as wrong as a widening one, and it is the direction a caller is most
        // likely to think "it'll just downmix".
        let mono_path = dir.path().join("mismatch_mono.wav");
        let mono =
            WavOut::create(&mono_path, 48_000.0, 1u16, BitDepth::Float32).expect("sink opens");
        assert!(Recorder::start(stereo_src(), mono).is_err());

        // And the matching pair is accepted.
        let ok_path = dir.path().join("match.wav");
        let ok = WavOut::create(&ok_path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");
        let rec = Recorder::start(stereo_src(), ok).expect("matching widths must be accepted");
        rec.stop().expect("finalize");
    }

    /// A non-stereo pairing records end to end, and every count on the way
    /// through stays denominated in FRAMES.
    ///
    /// At six channels a sample-denominated count is 6× off, so the frame total
    /// read back off the finished file is a direct check on the whole chain:
    /// `poll_into` → `pump` → `write` → header.
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
        let wav = WavOut::create(
            &path,
            48_000.0,
            ChannelLayout::from(6u16),
            BitDepth::Float32,
        )
        .expect("sink opens");

        let rec = Recorder::start(src, wav).expect("matching 6ch widths");
        // A finite source ends on its own, but `stop` clears the run flag
        // immediately — so without this the thread can exit before its first
        // pump pass and record nothing. Give it time to drain, then join.
        std::thread::sleep(Duration::from_millis(50));
        rec.stop().expect("finalize");

        let reader = hound::WavReader::open(&path).expect("readable");
        assert_eq!(reader.spec().channels, 6);
        assert_eq!(
            reader.len() as usize,
            FRAMES * CH,
            "500 FRAMES of 6 channels is 3000 samples — not 500, and not 18000"
        );
    }

    /// A live (starving) source records until stopped — the composability a
    /// generic `AudioIn` bound buys over a signature that opens a device itself.
    ///
    /// The fixture withholds frames without being exhausted, so a recorder that
    /// mistook a zero-frame poll for end-of-stream would stop early.
    #[test]
    fn a_non_mic_live_source_records_until_stopped() {
        struct Starving {
            polls: std::sync::atomic::AtomicUsize,
        }

        impl AudioIn for Starving {
            const ON_EMPTY: OnEmpty = OnEmpty::Starved;

            fn layout(&self) -> ChannelLayout {
                ChannelLayout::STEREO
            }

            fn poll_into(&mut self, out: &mut [f32]) -> usize {
                let n = self.polls.fetch_add(1, Ordering::Relaxed);
                // Every other poll is empty — never exhausted.
                if n % 2 == 1 || out.len() < 2 {
                    return 0;
                }
                out[0] = 0.25;
                out[1] = -0.25;
                1
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generated.wav");
        let wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

        let rec = Recorder::start(
            Starving {
                polls: std::sync::atomic::AtomicUsize::new(0),
            },
            wav,
        )
        .expect("matching stereo widths");
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

    /// A live source that never ends on its own, for the `Drop` tests.
    ///
    /// [`Starved`](OnEmpty::Starved) is the whole point: a finite source's pump
    /// loop breaks by itself, so a `Drop` bug is invisible with one. This never
    /// runs dry, so only `Drop` (or `stop`) can end the take.
    struct AlwaysLive;

    impl AudioIn for AlwaysLive {
        const ON_EMPTY: OnEmpty = OnEmpty::Starved;

        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }

        fn poll_into(&mut self, out: &mut [f32]) -> usize {
            let frames = out.len() / 2;
            for (i, s) in out[..frames * 2].iter_mut().enumerate() {
                *s = if i % 2 == 0 { 0.5 } else { -0.5 };
            }
            frames
        }
    }

    /// **The `Drop` guarantee.** A live recorder dropped without `stop` still
    /// finalizes, so the WAV is readable.
    ///
    /// Before the `Drop` impl this file was unreadable: the pump loop for a
    /// `Starved` source has no exit but the `running` flag, so `finalize` never
    /// ran and the RIFF/data sizes stayed at their placeholder values. The
    /// audio was on disk the whole time — only the header disagreed, which is
    /// what makes the failure silent.
    ///
    /// Mutation check: delete `impl Drop for Recorder` and this fails at
    /// `WavReader::open` (and the thread it leaks keeps running).
    #[test]
    fn dropping_a_live_recorder_still_finalizes_the_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dropped.wav");
        let wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

        let status = {
            let rec = Recorder::start(AlwaysLive, wav).expect("matching stereo widths");
            let status = rec.finalize_status();
            // Let the pump land some frames, then drop without calling `stop`.
            std::thread::sleep(Duration::from_millis(30));
            status
        };

        assert!(
            status.is_done(),
            "the drop path must run finalize and record that it did"
        );
        assert_eq!(
            status.error(),
            None,
            "finalize should have succeeded on a healthy sink"
        );

        let reader = hound::WavReader::open(&path)
            .expect("a dropped recorder must still leave a readable WAV");
        assert_eq!(reader.spec().channels, 2);
        assert!(
            reader.len() > 0,
            "the header must report the frames the pump actually wrote"
        );
    }

    /// `stop` and `Drop` cannot both join: `stop` consumes the recorder, so the
    /// `Drop` that immediately follows finds the handle already taken.
    ///
    /// The observable half is that `stop` still returns the finalize result and
    /// the drop path leaves `FinalizeStatus` untouched — a second join would
    /// panic on the already-consumed `JoinHandle` long before this assertion,
    /// so reaching it at all is most of the proof.
    #[test]
    fn stop_and_drop_do_not_both_join() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stopped.wav");
        let wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

        let rec = Recorder::start(AlwaysLive, wav).expect("matching stereo widths");
        let status = rec.finalize_status();
        std::thread::sleep(Duration::from_millis(20));
        rec.stop().expect("stop returns the finalize result");

        assert!(
            !status.is_done(),
            "`stop` returns the result itself; the drop path must not also \
             record one, or a caller cannot tell which shutdown ran"
        );
        assert!(hound::WavReader::open(&path).is_ok());
    }
}
