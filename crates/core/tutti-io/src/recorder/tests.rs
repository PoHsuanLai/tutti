use super::*;
use crate::WavOut;
use crate::pump::{ManualDriver, PumpPass};
use tutti_core::ChannelLayout;
use tutti_core::io::pump;
use tutti_core::io::{AudioIn, OnEmpty};
use tutti_core::pcm::BitDepth;

/// A finite in-memory [`AudioIn`] standing in for a live mic: hands out its
/// frames in bounded chunks, returning a short-then-zero count at
/// end-of-stream. Lets the pump→`WavOut`→readback path be proven without
/// touching real hardware. Has a runtime width, like the `SliceSource`
/// fixture in `tutti_types::io`'s own tests.
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
    let mut scratch = vec![0.0f32; SCRATCH_FRAMES.interleaved_len(ChannelLayout::STEREO)];
    while !pump(&mut src, &mut wav, &mut scratch).is_zero() {}
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
    let wide = WavOut::create(&wide_path, 48_000.0, 6u16, BitDepth::Float32).expect("sink opens");
    let err = Recorder::start(stereo_src(), wide)
        .expect_err("a width mismatch must be refused, not recorded");
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
    let mono = WavOut::create(&mono_path, 48_000.0, 1u16, BitDepth::Float32).expect("sink opens");
    assert!(Recorder::start(stereo_src(), mono).is_err());

    // And the matching pair is accepted.
    let ok_path = dir.path().join("match.wav");
    let ok = WavOut::create(&ok_path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");
    let rec = Recorder::start(stereo_src(), ok).expect("matching widths must be accepted");
    rec.finish().expect("finalize");
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

    let (driver, pump) = ManualDriver::new();
    let rec = Recorder::start_with(src, wav, driver).expect("matching 6ch widths");

    // Drain by counting, not by sleeping: `finish` clears the run flag
    // immediately and a threaded pump can exit before its first pass. Here
    // the passes are the caller's, so "drained" is a fact.
    let written = pump.pump_until_dry(16);
    assert_eq!(
        written,
        Samples(FRAMES),
        "the pump moved {written} frames of {FRAMES} -- a short drain here would \
         make the header assertion below measure the harness"
    );
    rec.finish().expect("finalize");

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

        fn poll_into(&mut self, out: &mut [f32]) -> Samples {
            let n = self.polls.fetch_add(1, Ordering::Relaxed);
            // Every other poll is empty — never exhausted.
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
    let wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

    let (driver, pump) = ManualDriver::new();
    let rec = Recorder::start_with(
        Starving {
            polls: std::sync::atomic::AtomicUsize::new(0),
        },
        wav,
        driver,
    )
    .expect("matching stereo widths");

    // Six passes of a fixture that yields on every *other* poll: passes
    // 0, 2, 4 write a frame each and 1, 3, 5 report `Starved`. Nothing here
    // is a guess.
    //
    // The sequence below fails if a `Starved` pass is treated as end-of-stream
    // (writes stop at 1), and it fails if the parity of the fixture changes,
    // which is the only other way the count can move.
    let mut wrote = Samples::ZERO;
    for pass in 0..6 {
        let outcome = pump.pump_once().expect("the take is live");
        match (pass % 2, outcome) {
            (0, PumpPass::Wrote(n)) => wrote += n,
            (1, PumpPass::Starved) => {}
            (_, other) => panic!(
                "pass {pass} of a source that yields every other poll reported {other:?} \
                 -- a `Starved` pass read as end-of-stream ends a live take silently"
            ),
        }
    }
    assert_eq!(wrote, Samples(3), "three yielding passes, one frame each");

    rec.finish().expect("finalize should succeed");

    let reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
    let frames = reader.len() as usize / 2; // two samples per stereo frame
    assert_eq!(
        frames, 3,
        "the header must report exactly the frames the counted passes wrote"
    );
}

/// A live source that never ends on its own, for the `Drop` tests.
///
/// [`Starved`](OnEmpty::Starved) is the whole point: a finite source's pump
/// loop breaks by itself, so a `Drop` bug is invisible with one. This never
/// runs dry, so only `Drop` (or `finish`) can end the take.
struct AlwaysLive;

impl AudioIn for AlwaysLive {
    const ON_EMPTY: OnEmpty = OnEmpty::Starved;

    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }

    fn poll_into(&mut self, out: &mut [f32]) -> Samples {
        let frames = Samples::from_interleaved_len(out.len(), ChannelLayout::STEREO);
        for (i, s) in out[..frames.interleaved_len(ChannelLayout::STEREO)]
            .iter_mut()
            .enumerate()
        {
            *s = if i % 2 == 0 { 0.5 } else { -0.5 };
        }
        frames
    }
}

/// **The `Drop` guarantee.** A live recorder dropped without `finish` still
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

    let (driver, pump) = ManualDriver::new();
    let status = {
        let rec = Recorder::start_with(AlwaysLive, wav, driver).expect("matching stereo widths");
        let status = rec.finalize_status();
        // Land a known number of frames, then drop without calling `finish`.
        // Three passes is a count, not a sleep.
        for _ in 0..3 {
            assert!(
                matches!(pump.pump_once(), Some(PumpPass::Wrote(n)) if !n.is_zero()),
                "AlwaysLive fills every buffer it is handed"
            );
        }
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
    // The take is over: the loop is out of the slot and no pass can run.
    // A direct observation of the shutdown, rather than inferring it from
    // the file below.
    assert_eq!(
        pump.pump_once(),
        None,
        "the dropped recorder must have taken the pump loop, not merely stopped polling it"
    );

    let reader =
        hound::WavReader::open(&path).expect("a dropped recorder must still leave a readable WAV");
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(
        reader.len() as usize,
        (SCRATCH_FRAMES * 3).interleaved_len(ChannelLayout::STEREO),
        "the header must report exactly the frames the three counted passes wrote"
    );
}

/// **The other half of the `Drop` guarantee: a finalize that *fails* there
/// still reaches the caller.**
///
/// The sibling test above proves the drop path runs finalize and records
/// success. Nothing proved it records a *failure* — `error()` returning
/// `Some` had no coverage at all, so the entire reason
/// [`FinalizeStatus`] carries an error rather than just a done flag was
/// untested. `Drop` cannot return a `Result` and this workspace does not
/// panic in `Drop`, so this handle is the only path a disk-full on the last
/// block has to the caller; if it silently dropped the error, a truncated
/// take would look like a clean one.
///
/// The failure is provoked, not simulated: `narrow_header_for_test` opens
/// an 8-bit header that the sink then quantizes into at `Int16`, so
/// `AlwaysLive`'s ±0.5 (±16383 at `Int16`) is rejected by `hound` with
/// `TooWide` on the write. See that constructor for why this shape rather
/// than a permission or disk trick.
///
/// Mutation-checked: deleting the `self.status.set(result)` line from
/// `Drop` — or narrowing it to store only successes — fails this test while
/// leaving every other test in the file green.
#[test]
fn a_finalize_that_fails_on_the_drop_path_is_recorded_not_swallowed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doomed.wav");
    let wav = WavOut::narrow_header_for_test(&path);

    let (driver, pump) = ManualDriver::new();
    let status = {
        let rec = Recorder::start_with(AlwaysLive, wav, driver)
            .expect("the fixture sink is stereo, as AlwaysLive is");
        let status = rec.finalize_status();
        // One pass is enough: the first sample is already too wide, and
        // `WavOut` latches `first_error` and stops writing from there.
        assert!(
            matches!(pump.pump_once(), Some(PumpPass::Wrote(n)) if !n.is_zero()),
            "the pump reports frames moved; the sink's failure is latched, not returned"
        );
        status
    };

    assert!(
        status.is_done(),
        "the drop path must run finalize even when it is going to fail"
    );
    let err = status
        .error()
        .expect("a failed finalize on the drop path must be recorded, not swallowed");
    // The specific cause survives — a caller can tell a too-wide sample
    // from a full disk. Flattening to "finalize failed" would make the two
    // indistinguishable, which is the information loss `WavOut::finalize`
    // already refuses to accept.
    assert!(
        err.contains("more bits than the destination type"),
        "the underlying hound error should be carried, got: {err}"
    );
}

/// `finish` and `Drop` cannot both join: `finish` consumes the recorder, so the
/// `Drop` that immediately follows finds the handle already taken.
///
/// The observable half is that `finish` still returns the finalize result and
/// the drop path leaves `FinalizeStatus` untouched — a second join would
/// panic on the already-consumed `JoinHandle` long before this assertion,
/// so reaching it at all is most of the proof.
#[test]
fn stop_and_drop_do_not_both_join() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stopped.wav");
    let wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

    let (driver, pump) = ManualDriver::new();
    let rec = Recorder::start_with(AlwaysLive, wav, driver).expect("matching stereo widths");
    let status = rec.finalize_status();
    assert!(matches!(pump.pump_once(), Some(PumpPass::Wrote(_))));
    rec.finish().expect("finish returns the finalize result");

    // Same observation as the drop test, and it is what makes this one
    // non-vacuous: `finish` took the loop, so there is nothing left to join a
    // second time.
    assert_eq!(pump.pump_once(), None, "`finish` must take the pump loop");

    assert!(
        !status.is_done(),
        "`finish` returns the result itself; the drop path must not also \
         record one, or a caller cannot tell which shutdown ran"
    );
    assert!(hound::WavReader::open(&path).is_ok());
}

/// A sink that is not a WAV, over an element that is not `f32`: the pump
/// is generic over both, and a test sink can be read back directly.
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
/// `bevy-tutti`'s `AudioPump` hands its capacity through here, so this is
/// the property its `Samples(64)` relies on: 1000 frames at 64 per pass is
/// fifteen full passes and a 40-frame tail, then `Ended`. The values are
/// read back from the sink, so a pass that skipped or repeated frames fails
/// on the content, not just the count.
///
/// *Mutation:* size the scratch from `SCRATCH_FRAMES` instead of `frames`
/// in `start_with_capacity`. The first pass then writes all 1000 frames and
/// the `Wrote(64)` assertion fails.
#[test]
fn a_named_capacity_bounds_each_pass_over_any_sink() {
    let seen = Collect(Arc::new(std::sync::Mutex::new(Vec::new())));
    let (driver, pump) = ManualDriver::new();
    let rec = Recorder::start_with_capacity(
        RampI16 {
            frames: 1000,
            pos: 0,
        },
        seen.clone(),
        Samples(64),
        driver,
    )
    .expect("stereo into stereo");

    for pass in 0..15 {
        assert_eq!(
            pump.pump_once(),
            Some(PumpPass::Wrote(Samples(64))),
            "pass {pass} must move exactly the named capacity"
        );
    }
    assert_eq!(pump.pump_once(), Some(PumpPass::Wrote(Samples(40))));
    assert_eq!(pump.pump_once(), Some(PumpPass::Ended));
    rec.finish().expect("a collecting sink finalizes cleanly");

    let got = seen.0.lock().unwrap();
    assert_eq!(got.len(), 2000, "1000 stereo frames are 2000 samples");
    for (k, frame) in got.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(*frame, [k as i16, -(k as i16)], "frame {k}");
    }
}

/// **A zero capacity is one frame, not a take that never moves.**
///
/// An empty scratch makes every poll return zero, and the source's
/// `ON_EMPTY` then reads that zero as its own: a finite take ends before
/// its first frame, a live one parks forever. `AudioPump` has always
/// clamped to one frame; the clamp moved here with it.
///
/// *Mutation:* drop the clamp (`let frames = capacity;`). The first pass
/// then reports `Ended` with nothing written and this fails.
#[test]
fn a_zero_capacity_moves_one_frame_per_pass() {
    let seen = Collect(Arc::new(std::sync::Mutex::new(Vec::new())));
    let (driver, pump) = ManualDriver::new();
    let rec = Recorder::start_with_capacity(
        RampI16 { frames: 3, pos: 0 },
        seen.clone(),
        Samples::ZERO,
        driver,
    )
    .expect("stereo into stereo");

    assert_eq!(pump.pump_until_dry(16), Samples(3));
    assert_eq!(pump.pump_once(), Some(PumpPass::Ended));
    rec.finish().expect("finalize");
    assert_eq!(*seen.0.lock().unwrap(), [0, 0, 1, -1, 2, -2]);
}

/// **`finish_in_place` finalizes in place, once.**
///
/// A component holding a take cannot move it into `finish`, so it calls
/// `finish_in_place` from a `&mut`. The first call returns the finalize result;
/// every later one returns `None`, and the `Drop` that follows records
/// nothing, since the caller already holds the result.
///
/// *Mutations:* make `join_pump` return `Some(Ok(()))` when the handle is
/// gone (the second-call assertion fails); make `Drop` call
/// `self.status.set(Ok(()))` unconditionally (the `is_done` assertion
/// fails).
#[test]
fn finish_in_place_finalizes_in_place_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("in_place.wav");
    let wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

    let (driver, pump) = ManualDriver::new();
    let mut rec = Recorder::start_with(AlwaysLive, wav, driver).expect("matching stereo widths");
    let status = rec.finalize_status();
    assert!(matches!(pump.pump_once(), Some(PumpPass::Wrote(_))));

    assert!(rec.is_running());
    assert!(
        matches!(rec.finish_in_place(), Some(Ok(()))),
        "the first shutdown returns the finalize result"
    );
    assert!(!rec.is_running(), "a shutdown clears the run flag");
    assert!(
        rec.finish_in_place().is_none(),
        "a second shutdown has nothing to join and must say so"
    );
    assert!(!rec.is_finished(), "no pump is left to report on");
    assert_eq!(pump.pump_once(), None, "the loop is finalized and gone");

    drop(rec);
    assert!(
        !status.is_done(),
        "the drop after a shutdown must not record a second finalize"
    );
    let reader = hound::WavReader::open(&path).expect("finalized in place");
    assert_eq!(
        reader.len() as usize,
        SCRATCH_FRAMES.interleaved_len(ChannelLayout::STEREO)
    );
}
