//! The end-to-end this crate describes but never ran: **`AudioTap` →
//! `TapIn` → `Recorder` → `WavOut` → read it back**.
//!
//! Every piece had unit coverage and the whole had none. `tap_in.rs`'s tests
//! push frames and pop them again; `wav_out.rs`'s write into a sink and read
//! the file; `recorder.rs`'s pump a fixture. Nothing put the three together,
//! so nothing checked that the *units* line up across the seams — and the
//! units are the entire risk surface here. `CLAUDE.md` names the failure
//! exactly: "**Every count on this boundary is denominated in FRAMES** … a
//! consumer compares a returned count against a loop range or a file position,
//! both in frames, so leaking samples makes a 6-channel looped clip wrap at a
//! sixth of its length and present as 'the loop points are wrong'."
//!
//! The shape *is* documented — in `tutti-io`'s README, as a doctest. nextest
//! does not run doctests (`CLAUDE.md`, "nextest does NOT run doctests, and
//! says nothing about skipping them"), so `just test` never executed it.
//!
//! Values, not just counts. Each frame carries its own index, so a dropped,
//! duplicated, or channel-swapped frame shows up as a wrong *number* rather
//! than a right total. A count-only assertion passes on all three.
//!
//! Mutations run against `tap_in.rs`, and what each actually broke:
//!
//! | mutation | fails |
//! |---|---|
//! | `out.len() / 2` → `out.len()` in `poll_into` (frames→samples) | `…_in_order_through_the_whole_chain` only |
//! | swap `l`/`r` when writing the frame | both |
//!
//! That first row is why `…_in_order_…` pushes more frames than the pump's
//! scratch holds. Under the frames→samples mutation the burst test still
//! passes: its 512-frame bursts are smaller than the 1024-frame scratch, so
//! the ring runs dry before the inflated loop bound is reached and the count
//! comes out right by accident. Only a push that *exceeds* the scratch forces
//! the bad bound to be used. Keep `FRAMES` above `SCRATCH_FRAMES`.

use tutti_core::{AudioTap, ChannelLayout};
use tutti_io::{AudioIn, BitDepth, ManualDriver, PumpPass, Recorder, TapIn, WavOut};

/// Frames pushed into the tap, as the audio callback would.
const FRAMES: usize = 4096;

/// A value that identifies its own frame index exactly under f32.
fn left_of(i: usize) -> f32 {
    i as f32 / 65_536.0
}

/// Distinct from the left channel at every index, so a channel swap is visible.
fn right_of(i: usize) -> f32 {
    -(i as f32 / 65_536.0) - 0.5
}

/// Push `n` interleaved stereo frames into `tap` the way the CPAL callback
/// does — one bulk `push` of a `frames`-long block.
fn push_block(tap: &AudioTap, from: usize, n: usize) {
    let mut block = Vec::with_capacity(n * 2);
    for i in from..from + n {
        block.push(left_of(i));
        block.push(right_of(i));
    }
    tap.push(&block, n);
}

/// **The whole chain, driven pass by pass, with every sample checked.**
///
/// A [`ManualDriver`] rather than a thread: the counts are then exact rather
/// than whatever the scheduler allowed, which is the same argument
/// `recorder.rs` makes for its own fixtures. The thread is covered separately
/// in `recorder_thread_driver.rs`.
#[test]
fn tapped_frames_reach_the_wav_in_order_through_the_whole_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tapped.wav");

    let tap = AudioTap::new();
    let cons = tap.open().expect("a fresh tap opens");
    let src = TapIn::new(cons);
    assert_eq!(
        src.layout(),
        ChannelLayout::STEREO,
        "the tap is a stereo monitor; the sink below is built to match"
    );

    let sink = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

    let (driver, pump) = ManualDriver::new();
    let rec = Recorder::start_with(src, sink, driver).expect("stereo source into a stereo sink");

    // Fill the tap first so the pump has something to move, then drain it with
    // a bounded number of passes — never a sleep, and never "pump until it
    // looks done".
    push_block(&tap, 0, FRAMES);
    let moved = pump.pump_until_dry(64);
    assert_eq!(
        moved,
        FRAMES,
        "every frame pushed must be moved, and counted in FRAMES — a pump that \
         returned samples here would report {}",
        FRAMES * 2
    );
    // The tap is empty now, so the next pass must report a live source waiting
    // rather than a finished one. `TapIn::ON_EMPTY` is `Starved`, and this is
    // what that const buys.
    assert_eq!(
        pump.pump_once(),
        Some(PumpPass::Starved),
        "an empty tap is a producer that has not caught up, not end-of-stream"
    );

    rec.stop().expect("the take finalizes cleanly");

    let mut reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.spec().sample_rate, 48_000);
    assert_eq!(
        reader.len() as usize,
        FRAMES * 2,
        "the header counts samples; the chain counts frames. {FRAMES} frames \
         must land as {} samples",
        FRAMES * 2
    );

    let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
    for (i, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
        assert!(
            (pair[0] - left_of(i)).abs() < 1e-9,
            "frame {i} left: expected {}, got {}",
            left_of(i),
            pair[0]
        );
        assert!(
            (pair[1] - right_of(i)).abs() < 1e-9,
            "frame {i} right: expected {}, got {} (a channel swap looks like this)",
            right_of(i),
            pair[1]
        );
    }
}

/// **A tap filled in several bursts still lands one contiguous take.**
///
/// The single-burst case above cannot distinguish "moves frames correctly"
/// from "moves exactly one block correctly". Pushing across several passes
/// exercises the ring wrapping and the pump's repeated entry, and a
/// frames-vs-samples slip shows as a *short* file rather than a reordered one.
#[test]
fn interleaved_pushes_and_pumps_lose_no_frames() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bursty.wav");

    let tap = AudioTap::new();
    let src = TapIn::new(tap.open().expect("a fresh tap opens"));
    let sink = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

    let (driver, pump) = ManualDriver::new();
    let rec = Recorder::start_with(src, sink, driver).expect("stereo source into a stereo sink");

    const BURST: usize = 512;
    const BURSTS: usize = 6;
    let mut moved = 0usize;
    for b in 0..BURSTS {
        push_block(&tap, b * BURST, BURST);
        moved += pump.pump_until_dry(16);
    }
    assert_eq!(moved, BURST * BURSTS, "no frame may be lost between bursts");

    rec.stop().expect("the take finalizes cleanly");

    let mut reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
    assert_eq!(reader.len() as usize, BURST * BURSTS * 2);

    let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
    for (i, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
        assert!(
            (pair[0] - left_of(i)).abs() < 1e-9 && (pair[1] - right_of(i)).abs() < 1e-9,
            "frame {i} is not the frame that was pushed at that index — the \
             bursts did not concatenate"
        );
    }
}
