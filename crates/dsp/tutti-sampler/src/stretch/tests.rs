//! Tests for the phase vocoder. Kept in one module: most drive the public
//! [`Unit`] end to end, and several reach the private buffers and [`Vocoder`]
//! beneath it.

use super::buffers::*;
use super::unit::*;
use super::vocoder::*;
use super::*;
#[allow(unused_imports)]
use tutti_core::{
    inverse_fft, real_fft, AudioUnit, BufferMut, BufferRef, Cents, ChannelLayout, Complex32,
    Radians, ReadRate, SampleRate, Samples, SignalFrame, StretchFactor,
};

use std::f32::consts::PI;
use tutti_core::BufferVec;

fn sine(freq: f32, sample_rate: f32, len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| (Radians::TAU.get() * freq * i as f32 / sample_rate).sin() * 0.5)
        .collect()
}

#[test]
fn fft_sizes_are_powers_of_two_with_quarter_hops() {
    for size in FftSize::PRESETS {
        assert!(size.size().get().is_power_of_two());
        assert_eq!(size.hop().get(), size.size().get() / 4);
    }
    assert_eq!(FftSize::N2048.size(), Samples(2048));
    assert_eq!(FftSize::N2048.hop(), Samples(512));
}

/// Every `FftSize` must satisfy COLA, because `geometry` `expect`s it.
#[test]
fn every_fft_size_yields_a_cola_grid() {
    for size in FftSize::PRESETS {
        let g = Unit::geometry(44_100.0, size);
        assert!(g.is_cola(), "{size:?} is not COLA-compliant");
        assert_eq!(g.window(), size.size());
    }
}

/// A zero or negative rate must not reach `cola`, which rejects it.
#[test]
fn non_positive_sample_rate_does_not_panic() {
    for rate in [0.0, -44_100.0] {
        let u = Unit::new(rate);
        assert_eq!(u.channels(), ChannelLayout::STEREO);
    }
}

#[test]
fn wrap_phase_maps_into_a_single_turn() {
    let tau = Radians::TAU.get();
    for &p in &[0.0, PI, -PI, 3.0 * PI, -3.0 * PI, 100.0 * tau + 1.0] {
        let w = wrap_phase(Radians(p)).get();
        assert!(w > -PI - 1e-4 && w <= PI + 1e-4, "{p} wrapped to {w}");
        // Wrapping differs from the input by a whole number of turns.
        let turns = (p - w) / tau;
        assert!((turns - turns.round()).abs() < 1e-3, "{p} -> {w}");
    }
}

/// The `while` loop this replaced iterated once per 2π, so a large phase
/// cost unbounded time on the audio thread. Arithmetic wrapping is O(1).
#[test]
fn wrap_phase_handles_a_large_accumulated_phase() {
    let w = wrap_phase(Radians(1.0e6)).get();
    assert!(w > -PI - 1e-2 && w <= PI + 1e-2, "wrapped to {w}");
}

/// The fact `COLA_GAIN` depends on: microfft's inverse transform already
/// normalizes, so a forward/inverse pair is the identity and synthesis must
/// NOT divide by the FFT size again.
///
/// The original code did divide, attenuating stretched audio by 60–78 dB
/// depending on window. If a future FFT backend returns an unnormalized
/// inverse, this fails loudly here rather than showing up as a quiet
/// stretcher.
#[test]
fn fft_roundtrip_is_the_identity() {
    let n = 1024usize;
    let orig: Vec<f32> = (0..n)
        .map(|i| (Radians::TAU.get() * 5.0 * i as f32 / n as f32).sin() * 0.5)
        .collect();

    let mut buf = orig.clone();
    let packed = real_fft(&mut buf);
    let bins = n / 2 + 1;
    let mut spectrum = vec![Complex32::new(0.0, 0.0); n];
    spectrum[..packed.len()].copy_from_slice(packed);
    let (dc, nyquist) = (spectrum[0].re, spectrum[0].im);
    spectrum[0] = Complex32::new(dc, 0.0);
    spectrum[bins - 1] = Complex32::new(nyquist, 0.0);
    for i in 1..bins - 1 {
        spectrum[n - i] = spectrum[i].conj();
    }
    inverse_fft(&mut spectrum);

    for (i, &want) in orig.iter().enumerate() {
        let got = spectrum[i].re;
        assert!(
            (got - want).abs() < 1e-4,
            "sample {i}: {got} != {want} — inverse_fft normalization changed"
        );
    }
}

#[test]
fn fifo_peeks_without_consuming() {
    let mut f = SampleFifo::new(8);
    f.push(&[1.0, 2.0, 3.0]);
    assert_eq!(f.available(), 3);

    // The defining property of the input side: reading does not consume, so
    // the same frame can be read once per hop.
    assert_eq!(f.peek(0), 1.0);
    assert_eq!(f.peek(0), 1.0);
    assert_eq!(f.peek(2), 3.0);
    assert_eq!(f.available(), 3);

    f.consume(2);
    assert_eq!(f.available(), 1);
    assert_eq!(f.peek(0), 3.0);
}

/// The index wraps but the cursors do not, so a write past the ring end
/// keeps `available` exact rather than folding it to zero.
#[test]
fn fifo_wraps_the_index_not_the_cursors() {
    let mut f = SampleFifo::new(8);
    f.push(&[1.0, 2.0, 3.0]);
    f.consume(2);
    f.push(&[4.0; 7]);
    assert_eq!(f.available(), 8);
    // Read cursor is at 2, so the oldest live sample is still 3.0.
    assert_eq!(f.peek(0), 3.0);
}

#[test]
fn overlap_add_accumulates_ahead_and_drains_behind() {
    let mut o = OverlapAdd::new(8);

    // Two frames summing into overlapping spans — the operation a FIFO
    // cannot express.
    o.add_at(0, 0.5);
    o.add_at(1, 0.5);
    o.advance(1);
    o.add_at(0, 0.25); // lands on the slot the previous frame's offset 1 hit

    let mut out = [0.0; 2];
    assert_eq!(o.drain(&mut out), 1, "only one hop has been published");
    assert_eq!(out[0], 0.5);

    o.advance(1);
    assert_eq!(o.drain(&mut out), 1);
    assert_eq!(out[0], 0.75, "0.5 + 0.25 accumulated in one slot");
}

#[test]
fn overlap_add_drain_reports_short_reads() {
    let mut o = OverlapAdd::new(8);
    o.add_at(0, 1.0);
    o.advance(1);

    let mut out = [0.0; 4];
    assert_eq!(o.drain(&mut out), 1);
    assert_eq!(out, [1.0, 0.0, 0.0, 0.0]);
}

#[test]
fn overlap_add_clear_at_zeroes_a_future_slot() {
    let mut o = OverlapAdd::new(8);
    o.add_at(3, 1.0);
    o.clear_at(3);
    o.advance(4);

    let mut out = [0.0; 4];
    assert_eq!(o.drain(&mut out), 4);
    assert_eq!(out[3], 0.0, "cleared slot must not carry stale audio");
}

/// `available()` must not exceed the capacity, and an overrun must be
/// reportable.
///
/// The cursors are monotonic, so before the cap this returned 12 for a
/// 4-slot ring and `drain` cheerfully served eight slots that had been
/// overwritten twice. A caller cannot distinguish that from real audio — it
/// is the mechanism that hid the vocoder's input-rate bug for the life of the
/// file.
#[test]
fn ring_available_saturates_at_capacity_and_reports_the_overrun() {
    let mut o = OverlapAdd::new(4);
    assert!(!o.overrun());

    for i in 0..12 {
        o.add_at(0, i as f32);
        o.advance(1);
    }

    assert!(o.overrun(), "12 written into 4 slots is an overrun");
    assert_eq!(o.available(), 4, "must not claim more than the ring holds");

    let mut out = [0.0f32; 8];
    assert_eq!(o.drain(&mut out), 4, "drain is bounded by available()");
}

/// **The rate contract.** A one-in/one-out feed keeps both rings bounded and
/// the output audible, at every stretch factor.
///
/// This is the invariant the design rests on, and it asserts both halves:
/// bounded rings alone would be satisfied by a unit that emitted silence.
///
/// It holds because the unit paces its own source intake at
/// [`input_rate`](Unit::input_rate) internally. A stretcher emits `stretch`
/// samples per source sample, but `AudioUnit::tick` hands over exactly one and
/// takes one back — so the rate change has to happen on the source side, where
/// the unit can drop or repeat, rather than on the output side, where it
/// cannot.
///
/// The old formulation consumed one source sample per tick and published
/// `hop * stretch` per `hop` consumed, leaving a surplus of
/// `hop * (stretch - 1)` output samples per frame with nowhere to go: measured
/// 79,231 pending in a 4,096-sample ring at `stretch = 2.0` — nineteen laps —
/// which made `drain` serve overwritten audio and dropped ~32 of every 256
/// blocks to silence.
#[test]
fn a_one_to_one_feed_stays_bounded_and_audible_at_every_stretch() {
    // 0.25x — `StretchFactor::MIN` — is deliberately absent, and
    // `the_slowest_factor_ripples_because_its_frames_do_not_overlap` covers
    // it instead. At MIN the analysis hop equals the window, so consecutive
    // frames share no samples and there is no phase continuity to
    // reconstruct from; the output ripples between 0.003 and 0.60 rather
    // than holding a steady level. That is geometry, not a regression.
    for factor in [0.5f32, 1.5, 2.0, 4.0] {
        let mut u = Unit::with_fft_size_and_channels(44_100.0, FftSize::N1024, 1usize);
        u.set_stretch_factor(StretchFactor::new(factor));
        assert!(u.is_processing(), "factor {factor} should not bypass");

        let mut out = [0.0f32; 1];
        let mut n = 0usize;
        let mut feed = |u: &mut Unit, count: usize, n: &mut usize| {
            let mut peak = 0.0f32;
            for _ in 0..count {
                let t = *n as f32 / 44_100.0;
                let sample = 0.5 * (Radians::TAU.get() * 3000.0 * t).sin();
                *n += 1;
                u.tick(&[sample], &mut out);
                peak = peak.max(out[0].abs());
            }
            peak
        };

        // Prime: the opening blocks are legitimately quiet while the FIFOs and
        // the overlap-add tail fill. At 4x the intake is a quarter rate, so
        // this has to be generous.
        feed(&mut u, 60_000, &mut n);

        let mut quiet = 0usize;
        for _ in 0..256 {
            if feed(&mut u, 64, &mut n) < 0.01 {
                quiet += 1;
            }
            assert!(
                !u.channels.channels.borrow()[0].output.overrun(),
                "stretch {factor}: output ring overran ({} pending)",
                u.channels.channels.borrow()[0].output.available()
            );
            assert!(
                !u.channels.channels.borrow()[0].input.0.overrun(),
                "stretch {factor}: input ring overran"
            );
        }
        assert_eq!(
            quiet, 0,
            "stretch {factor}: {quiet}/256 blocks fell silent under a steady feed"
        );
    }
}

/// The intake loop is **bounded**, which is an RT-safety property rather than
/// a performance one: it runs inside the audio callback, and an unbounded
/// `while` there is a dropout waiting for the right parameter value.
///
/// The loop is paced by [`Unit::intake_rate`], which is the **pitch** ratio —
/// not `input_rate`, which is the caller's stretch half and never reaches
/// this loop. [`MAX_PITCH_CENTS`] is +2400, so the rate cannot exceed 4.0 and
/// the loop cannot run more than four times per tick. The bound comes from
/// `set_pitch_cents` clamping on store: a raw `Cents::new(12_000.0)` would
/// otherwise ask for a thousand iterations.
///
/// This test used to guard `input_rate` for the same reason, back when that
/// method paced the loop. It is asserted against `intake_rate` now because
/// that is what the loop actually reads — guarding the other one would pass
/// while the real bound went unchecked.
#[test]
fn the_intake_loop_is_bounded_by_the_pitch_clamp() {
    let u = Unit::with_channels(44_100.0, 1usize);

    // Well past the clamp, in the direction that increases intake.
    u.set_pitch_cents(Cents::new(12_000.0));
    assert_eq!(u.pitch_cents().get(), MAX_PITCH_CENTS);
    assert!(
        u.intake_rate() <= 4.0 + 1e-6,
        "intake rate {} would run the per-tick loop more than 4 times",
        u.intake_rate()
    );

    // And the other end cannot drive it to zero, which would starve the FIFO.
    u.set_pitch_cents(Cents::new(-12_000.0));
    assert_eq!(u.pitch_cents().get(), MIN_PITCH_CENTS);
    assert!(u.intake_rate() > 0.0);
}

/// The caller's rate is bounded too, for a different reason: it scales a
/// source cursor rather than a loop, so an unbounded value reads off the end
/// of a wave rather than spinning the callback.
#[test]
fn the_callers_read_rate_is_bounded_by_the_stretch_clamp() {
    let u = Unit::with_channels(44_100.0, 1usize);

    u.set_stretch_factor(StretchFactor::new(0.001));
    assert_eq!(u.stretch_factor(), StretchFactor::MIN);
    assert!(u.input_rate().get() <= 4.0 + 1e-6);

    u.set_stretch_factor(StretchFactor::new(100.0));
    assert_eq!(u.stretch_factor(), StretchFactor::MAX);
    assert!(u.input_rate().get() > 0.0);

    // Pitch must NOT appear here — it is the unit's own half. A pitch shift
    // folded into the caller's read cancels itself against the hops, which
    // is precisely how pitch shift was silently inert before.
    u.set_stretch_factor(StretchFactor::UNITY);
    u.set_pitch_cents(Cents::new(1200.0));
    assert_eq!(
        u.input_rate(),
        ReadRate::UNITY,
        "pitch leaked into the caller's read rate, which cancels the shift"
    );
}

/// `input_rate` is `1.0` while bypassing, so a caller can apply it
/// unconditionally without branching on `is_processing`.
/// A clone must not rebuild the immutable tables — the measurable half of
/// commit cost,
/// and the only half this crate can fix without a design change.
///
/// `Net::commit` deep-clones every node, once per channel. Rebuilding the Hann
/// window there costs `size` `cos()` calls per vocoder; sharing it is a
/// refcount bump. Asserted structurally (pointer identity) rather than by
/// timing, because a wall-clock threshold in a test suite is a flake generator.
///
/// # Commit cost is still over budget — do not proceed to per-voice nodes
///
/// Measured on this machine, release, 640 stretch nodes (32 tracks x 20
/// voices), against the 2 ms budget a graph edit has before it risks an audio
/// dropout:
///
/// | width  | before sharing | after  | budget |
/// |--------|----------------|--------|--------|
/// | stereo | 18.5 ms        | 12.9 ms | 2 ms  |
/// | 6ch    | 628 ms         | 448 ms  | 2 ms  |
///
/// Still 6x over at stereo and 224x at six channels. The window was never the
/// dominant term: each `Vocoder` allocates and zeroes ~108 KB of state, so 640
/// six-channel nodes touch ~405 MB per commit. No amount of sharing immutable
/// data fixes that — the buffers must either be pooled (so a clone claims
/// rather than allocates) or not cloned at all.
///
/// **This is the measurement gating the container dissolve.** It says the
/// per-voice-node design cannot land as written: 640 nodes is a realistic
/// project, and a commit at that scale would stall the main thread long enough
/// to underrun the callback.
#[test]
fn cloning_shares_the_bank_and_isolate_severs_it() {
    let u = Unit::with_channels(44_100.0, 6usize);
    let c = u.clone();

    // The commit path: a refcount bump, not ~96 KB per channel.
    assert!(
        Arc::ptr_eq(&u.channels, &c.channels),
        "the clone deep-copied the vocoder bank instead of sharing it"
    );
    assert_eq!(
        c.width,
        ChannelLayout::from(6u16),
        "width must mirror the bank without borrowing it"
    );

    // The safety boundary. An offline render clones the live net and ticks
    // it on a worker pool WHILE the audio thread plays the original, so a
    // shared bank there would have two threads writing one set of FIFOs.
    // `isolate` is called on every node of that clone before it reaches the
    // worker, and it must hand back private state.
    let mut isolated = u.clone();
    assert!(Arc::ptr_eq(&u.channels, &isolated.channels));
    isolated.isolate();
    assert!(
        !Arc::ptr_eq(&u.channels, &isolated.channels),
        "isolate() left the render sharing the live graph's vocoder state"
    );
    assert_eq!(
        isolated.width,
        ChannelLayout::from(6u16),
        "isolate must preserve the unit's width"
    );

    // Isolated state is clean, matching what a clone used to produce: a
    // render starts its filter fresh rather than mid-frame on audio it will
    // never emit.
    assert_eq!(isolated.channels.channels.borrow()[0].input.available(), 0);
    assert_eq!(isolated.channels.channels.borrow()[0].output.available(), 0);

    // The immutable tables still ride by `Arc` through an isolate, so
    // severing does not pay to rebuild the window or the phase table.
    assert!(Arc::ptr_eq(
        &u.channels.channels.borrow()[0].window,
        &isolated.channels.channels.borrow()[0].window
    ));
    assert!(Arc::ptr_eq(
        &u.channels.channels.borrow()[0].phase_per_sample,
        &isolated.channels.channels.borrow()[0].phase_per_sample
    ));
}

/// The invariant has teeth: two live handles ticking one bank is caught.
///
/// This is the test the whole hardening exists for. Interleaving is silent —
/// no race, no panic, just plausible-and-wrong audio — so without a guard
/// the only symptom is a subtly damaged render that every `!= 0.0` assertion
/// in this file would pass.
///
/// `AudioThreadCell`'s debug flag cannot catch this: it detects *concurrent*
/// borrows, and interleaved ticking is sequential. Hence [`Bank::claim`].
#[test]
#[should_panic(expected = "two live stretch::Unit handles")]
#[cfg(debug_assertions)]
fn two_live_handles_ticking_one_bank_is_caught() {
    let mut a = Unit::with_channels(44_100.0, 2usize);
    a.set_stretch_factor(StretchFactor::new(2.0));
    a.allocate();

    // A committed generation, sharing `a`'s bank.
    let mut b = a.clone();
    b.allocate();

    let mut frame = [0.0f32; 2];
    // `a` claims the bank...
    a.tick(&[0.25, 0.25], &mut frame);
    // ...and `b` ticking it too is the bug. Both handles are still alive, so
    // this is the interleave case and not a legitimate succession.
    b.tick(&[0.25, 0.25], &mut frame);
}

/// Succession is legitimate and must NOT trip the claim.
///
/// A committed generation replaces the one it was cloned from, and the live
/// path reaches that through `reset` / `isolate`. If either tripped the
/// guard, the guard would be unusable — so pin both directions, not just the
/// failing one.
#[test]
fn succession_and_isolation_do_not_trip_the_claim() {
    let mut a = Unit::with_channels(44_100.0, 2usize);
    a.set_stretch_factor(StretchFactor::new(2.0));
    a.allocate();
    let mut frame = [0.0f32; 2];
    a.tick(&[0.25, 0.25], &mut frame);

    // Reset transfers ticking rights to the successor.
    let mut b = a.clone();
    b.allocate();
    b.reset();
    b.tick(&[0.25, 0.25], &mut frame);

    // Isolation gives a private bank, so the render path is free regardless.
    let mut c = b.clone();
    c.isolate();
    c.allocate();
    c.tick(&[0.25, 0.25], &mut frame);

    // And the isolated handle owns state nobody else can reach.
    assert!(!Arc::ptr_eq(&b.channels, &c.channels));
}

/// A successor generation continues the stream rather than restarting it.
///
/// This is the payoff of sharing: a graph commit hands the next generation
/// the same bank, so playback continues seamlessly across a graph edit
/// instead of re-filling the vocoder and dropping ~46 ms of audio.
///
/// Written second. The first attempt ticked the original and the clone while
/// **both were alive**, which is precisely the interleave bug — and
/// [`Bank::claim`] caught it, which is the guard earning its place on a test
/// its author got wrong. Succession means the predecessor stops.
#[test]
fn a_successor_generation_continues_the_stream() {
    let mut original = Unit::with_channels(44_100.0, 2usize);
    original.set_stretch_factor(StretchFactor::new(2.0));
    original.allocate();

    let size = 64;
    let mut input = BufferVec::new(2);
    for i in 0..size {
        let s = (i as f32 * 0.05).sin() * 0.5;
        input.buffer_mut().set_f32(0, i, s);
        input.buffer_mut().set_f32(1, i, s);
    }
    let mut out = BufferVec::new(2);

    // Warm past the fill-up so the bank holds real history.
    for _ in 0..96 {
        original.process(size, &input.buffer_ref(), &mut out.buffer_mut());
    }
    let history = original.channels.channels.borrow()[0].input.available();
    assert!(history > 0, "the bank should hold history to inherit");

    // The commit: the successor takes the bank, the predecessor retires.
    let mut successor = original.clone();
    successor.allocate();
    assert!(
        Arc::ptr_eq(&original.channels, &successor.channels),
        "the successor should share the bank, not copy it"
    );
    drop(original);

    // The inherited state is the predecessor's, not a fresh filter's.
    assert_eq!(
        successor.channels.channels.borrow()[0].input.available(),
        history,
        "the successor restarted the stream instead of continuing it"
    );

    // And it emits immediately — no second fill-up latency after the edit.
    successor.process(size, &input.buffer_ref(), &mut out.buffer_mut());
    let heard = (0..size).any(|i| out.buffer_ref().at_f32(0, i).abs() > 1e-6);
    assert!(heard, "the successor went silent across the commit");
}

/// The block scratch rides the shared bank, so a commit reallocates nothing.
///
/// Rewritten twice, and the history is the point. First the clone copied
/// 64 KB per channel outright. Then it deferred that to `allocate` — which
/// changed *when* the cost was paid, not *whether*: `Net::commit` calls
/// `allocate` on every generation, so after the vocoder bank was shared this
/// scratch was **98% of a commit's remaining traffic at both widths**
/// (240 MB of 243.8 at six channels).
///
/// Now it lives on the bank, under the same claim and the same isolate
/// boundary as the vocoders, and a successor generation inherits it sized.
#[test]
fn the_block_scratch_rides_the_shared_bank() {
    let mut u = Unit::with_channels(44_100.0, 6usize);
    u.allocate();
    assert!(u.channels.scratch_is_ready(6));

    // A commit's clone shares the bank, so it inherits sized scratch and
    // `allocate` has nothing left to do.
    let c = u.clone();
    assert!(
        c.channels.scratch_is_ready(6),
        "the successor should inherit sized scratch, not reallocate it"
    );
    assert!(Arc::ptr_eq(&u.channels, &c.channels));

    // Idempotent: the graph allocates every generation, and re-sizing would
    // throw away 64 KB per channel per commit — the exact cost this removes.
    let ptr_before = u.channels.scratch_in.borrow()[0].capacity();
    let mut c2 = u.clone();
    c2.allocate();
    assert_eq!(u.channels.scratch_in.borrow()[0].capacity(), ptr_before);

    // Isolation severs it with the rest of the bank. The fresh bank is
    // sized at construction, so a render's isolated node is immediately
    // usable — the same guarantee a directly built unit has, and the one
    // `time_stretch_process_is_allocation_free` depends on.
    let mut iso = u.clone();
    iso.isolate();
    assert!(!Arc::ptr_eq(&u.channels, &iso.channels));
    assert!(
        iso.channels.scratch_is_ready(6),
        "a severed bank must arrive usable, not needing a later allocate"
    );
    iso.allocate();
    assert_eq!(iso.channels.scratch_out.borrow().len(), 6);
}

/// An isolated clone renders exactly what the original renders.
///
/// Rewritten when the vocoder bank became shared. The previous version ticked
/// the original and a plain clone alternately and asserted they matched
/// sample-for-sample — which a shared bank makes meaningless, because the two
/// handles now feed ONE FIFO and interleave rather than run in parallel. That
/// is the design working, not a regression, but it means the property has to
/// be asserted on an `isolate`d clone, which is the only clone that genuinely
/// owns its state.
///
/// Asserted on sample values rather than liveness — a quieter or truncated
/// block is the failure mode, and every `!= 0.0` assertion here would pass it.
#[test]
fn an_isolated_clone_renders_identically() {
    let mut original = Unit::with_channels(44_100.0, 2usize);
    original.set_stretch_factor(StretchFactor::new(2.0));
    original.allocate();

    // The offline-render shape: clone, isolate, allocate. Isolation gives it
    // private state, so it must now track the original exactly.
    let mut clone = original.clone();
    clone.isolate();
    clone.allocate();

    // fundsp's `Buffer` is fixed at 64 samples per channel; a larger `size`
    // reads past it rather than being clamped.
    let size = 64;
    let mut input_vec = BufferVec::new(2);
    for i in 0..size {
        let s = (i as f32 * 0.05).sin() * 0.5;
        input_vec.buffer_mut().set_f32(0, i, s);
        input_vec.buffer_mut().set_f32(1, i, s);
    }
    let mut out_a = BufferVec::new(2);
    let mut out_b = BufferVec::new(2);

    // Enough blocks to clear the fill-up latency: at 2x stretch the vocoder
    // emits nothing until its FIFO holds a whole 2048-sample window, which
    // is 64 blocks of source at this size — so a handful would compare
    // silence to silence and prove nothing.
    let mut heard_signal = false;
    for block in 0..128 {
        original.process(size, &input_vec.buffer_ref(), &mut out_a.buffer_mut());
        clone.process(size, &input_vec.buffer_ref(), &mut out_b.buffer_mut());

        for ch in 0..2 {
            for i in 0..size {
                let (x, y) = (
                    out_a.buffer_ref().at_f32(ch, i),
                    out_b.buffer_ref().at_f32(ch, i),
                );
                assert_eq!(
                    x, y,
                    "block {block}, channel {ch}, sample {i}: the isolated \
                     clone diverged from the original"
                );
                heard_signal |= x.abs() > 1e-6;
            }
        }
    }

    assert!(
        heard_signal,
        "both rendered silence — the comparison proved nothing"
    );
}

/// Dominant frequency of a settled signal, by Goertzel-style scan.
///
/// A test helper rather than production code: the crate has no FFT-analysis
/// surface and does not need one. Scans 40..4000 Hz at 1 Hz, Hann-windowed
/// so a partial that falls between probe frequencies does not split.
#[cfg(test)]
fn dominant_hz(x: &[f32], sample_rate: f32) -> f32 {
    let n = x.len();
    let mut best = (0.0f32, 0.0f32);
    let mut f = 40.0f32;
    while f < 4000.0 {
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (i, &s) in x.iter().enumerate() {
            let w = 0.5 - 0.5 * (Radians::TAU.get() * i as f32 / n as f32).cos();
            let p = Radians::TAU.get() * f * i as f32 / sample_rate;
            re += s * w * p.cos();
            im -= s * w * p.sin();
        }
        let m = (re * re + im * im).sqrt();
        if m > best.1 {
            best = (f, m);
        }
        f += 1.0;
    }
    best.0
}

#[cfg(test)]
fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
}

/// Render a 440 Hz source through a unit, **observing the crate's feed
/// contract**: one source sample in per output sample out, with the unit
/// pacing its own intake.
///
/// That contract is what every voice path does and what
/// `stretch_factor_changes_the_source_consumption_rate` pins, so a pitch test
/// that fed differently would be measuring a call shape no caller uses. An
/// earlier draft of this helper advanced a source cursor by
/// [`Unit::input_rate`] instead; it made all four pitch tests pass while
/// breaking nine existing ones, because scaling the read *and* the intake
/// resamples twice and the halves cancel.
#[cfg(test)]
fn render_440(u: &Unit, sample_rate: f32, out_len: usize) -> Vec<f32> {
    let mut au = u.clone();
    au.allocate();
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let s = (Radians::TAU.get() * 440.0 * i as f32 / sample_rate).sin();
        let mut o = [0.0f32];
        au.tick(&[s], &mut o);
        out.push(o[0]);
    }
    out
}

/// Render a 440 Hz **wave table** through a unit the way a placed voice does:
/// the caller advances a source cursor by [`Unit::input_rate`] and reads the
/// table at that fractional index.
///
/// The table is load-bearing and cost real debugging time. Generating the
/// source from `sin(TAU·f·pos/sr)` while stepping `pos` by the read rate makes
/// the *generator* perform the resampling, which pre-cancels the very
/// transposition under test and reports a clean 440 Hz for every setting. A
/// table read is what `MemorySource::get_sample_into` does, and it is the only
/// form that can observe the effect.
#[cfg(test)]
fn render_440_placed(u: &Unit, sample_rate: f32, out_len: usize) -> Vec<f32> {
    let table: Vec<f32> = (0..400_000)
        .map(|i| (Radians::TAU.get() * 440.0 * i as f32 / sample_rate).sin())
        .collect();
    let rate = u.input_rate().get() as f32;
    let mut au = u.clone();
    au.allocate();
    let mut out = Vec::with_capacity(out_len);
    let mut pos = 0.0f32;
    for _ in 0..out_len {
        let j = pos.floor() as usize;
        let frac = pos - j as f32;
        let a = table.get(j).copied().unwrap_or(0.0);
        let b = table.get(j + 1).copied().unwrap_or(0.0);
        let mut o = [0.0f32];
        au.tick(&[a + (b - a) * frac], &mut o);
        out.push(o[0]);
        pos += rate;
    }
    out
}

/// **Pitch shift transposes, and stretch does not.**
///
/// The assertion this crate lacked. Every pre-existing pitch test checked
/// only that the atomic round-tripped and clamped, so a unit that ignored
/// pitch entirely passed all of them — and one did, for the whole life of the
/// feature. Measured before this fix: +1200 cents moved 440 Hz to 411 and
/// -1200 moved it to 408. The *same* wrong answer in both directions, which
/// is why it could never have been a wrong-ratio bug.
///
/// Asserting frequency rather than liveness is the same lesson the 60 dB gain
/// bug taught here: `!= 0.0` is satisfied by almost any defect.
#[test]
fn pitch_shift_transposes_by_the_requested_interval() {
    let sr = 48_000.0f32;
    for &cents in &[0.0f32, 1200.0, -1200.0, 700.0, -500.0] {
        let u = Unit::with_channels(sr as f64, 1usize);
        u.set_pitch_cents(Cents::new(cents));

        let out = render_440(&u, sr, 48_000);
        let settled = &out[24_000..24_000 + 8192];
        let got = dominant_hz(settled, sr);
        let want = 440.0 * Cents::new(cents).to_pitch_ratio();

        // 2% covers the 1 Hz scan step and the vocoder's own bin resolution
        // without admitting a semitone (~6%) of error.
        assert!(
            (got - want).abs() < want * 0.02,
            "{cents:+} cents: wanted {want:.1} Hz, got {got:.1} Hz"
        );
    }
}

/// **A one-per-tick feed makes the stretch factor behave as varispeed** — it
/// moves pitch — and that is correct, not a bug.
///
/// This surprises, so it is pinned. A unit fed one source sample per output
/// sample has received the whole source by the time it has emitted the whole
/// output; there is no extra material to spread over a longer span, so a
/// factor of 2 can only mean "consume twice as fast", which transposes up an
/// octave. Measured here and on the parent commit alike: 0.5x yields 220 Hz
/// and 2.0x yields 880 Hz from a 440 Hz source.
///
/// Duration-changing stretch needs the *caller* to supply the extra material
/// by advancing its source cursor at [`Unit::input_rate`], which is what a
/// placed voice does — see
/// `stretching_a_placed_read_changes_duration_not_pitch`. The two call shapes
/// give different, individually correct answers, and conflating them is what
/// made an earlier draft of this fix break nine tests.
#[test]
fn a_one_per_tick_feed_makes_stretch_behave_as_varispeed() {
    let sr = 48_000.0f32;
    for &(factor, want) in &[(0.5f32, 220.0f32), (1.0, 440.0), (2.0, 880.0)] {
        let u = Unit::with_channels(sr as f64, 1usize);
        u.set_stretch_factor(StretchFactor::new(factor));

        let out = render_440(&u, sr, 48_000);
        let got = dominant_hz(&out[24_000..24_000 + 8192], sr);
        assert!(
            (got - want).abs() < want * 0.02,
            "stretch {factor}x on a one-per-tick feed: wanted {want:.1} Hz, got {got:.1} Hz"
        );
    }
}

/// A **placed** read — the caller advancing its cursor by
/// [`Unit::input_rate`] — changes duration without moving pitch.
///
/// This is the shape a timeline voice uses (`MemorySource` derives its
/// position from the playhead and scales it by the rate), and the one where
/// "time-stretch" means what the name says. Pinned opposite
/// `a_one_per_tick_feed_makes_stretch_behave_as_varispeed` so the difference
/// between the two call shapes is documented by executable example rather
/// than by comment.
#[test]
fn stretching_a_placed_read_changes_duration_not_pitch() {
    let sr = 48_000.0f32;
    for &factor in &[0.5f32, 1.0, 1.5, 2.0] {
        let u = Unit::with_channels(sr as f64, 1usize);
        u.set_stretch_factor(StretchFactor::new(factor));

        let out = render_440_placed(&u, sr, 48_000);
        let got = dominant_hz(&out[24_000..24_000 + 8192], sr);
        assert!(
            (got - 440.0).abs() < 440.0 * 0.02,
            "placed stretch {factor}x moved the pitch to {got:.1} Hz"
        );
    }
}

/// Pitch and stretch **compose** on a placed read: each lands on target with
/// the other engaged.
///
/// Composition is where this design could plausibly fail, because both halves
/// flow through one `effective_stretch = stretch * pitch_ratio`. If the split
/// were wrong, the single-parameter tests could still pass while every real
/// combination drifted.
#[test]
fn pitch_and_stretch_are_independent_on_a_placed_read() {
    let sr = 48_000.0f32;
    for &(factor, cents) in &[
        (2.0f32, 1200.0f32),
        (2.0, -700.0),
        (0.5, 1200.0),
        (0.5, -1200.0),
        (1.5, 500.0),
    ] {
        let u = Unit::with_channels(sr as f64, 1usize);
        u.set_stretch_factor(StretchFactor::new(factor));
        u.set_pitch_cents(Cents::new(cents));

        let out = render_440_placed(&u, sr, 48_000);
        let got = dominant_hz(&out[24_000..24_000 + 8192], sr);
        let want = 440.0 * Cents::new(cents).to_pitch_ratio();
        assert!(
            (got - want).abs() < want * 0.02,
            "placed stretch {factor}x with {cents:+} cents: \
             wanted {want:.1} Hz, got {got:.1} Hz"
        );
    }
}

/// A pitch shift must not cost level.
///
/// The phase scaling this fix deleted was not merely inert — it *attenuated*,
/// because decorrelating each bin's phase from its magnitude makes
/// overlap-add cancel where it should reinforce. Measured with the scaling
/// retained alongside the correct read-rate fix: -8.7 dB at +1200 cents and
/// -12.5 dB at +700. Without an energy assertion, a future reintroduction
/// would show up only as "sounds a bit quiet".
#[test]
fn pitch_shift_preserves_level() {
    let sr = 48_000.0f32;
    let dry = {
        let u = Unit::with_channels(sr as f64, 1usize);
        u.set_pitch_cents(Cents::new(0.0));
        rms(&render_440(&u, sr, 48_000)[24_000..24_000 + 8192])
    };
    assert!(dry > 0.1, "reference render was silent ({dry:.4})");

    for &cents in &[1200.0f32, -1200.0, 700.0] {
        let u = Unit::with_channels(sr as f64, 1usize);
        u.set_pitch_cents(Cents::new(cents));
        let wet = rms(&render_440(&u, sr, 48_000)[24_000..24_000 + 8192]);
        let db = 20.0 * (wet / dry).log10();
        // 3 dB is generous against the measured 0.0-0.6 dB, and still far
        // tighter than the 8.7-12.5 dB the deleted scaling cost.
        assert!(
            db > -3.0,
            "{cents:+} cents lost {:.1} dB (rms {wet:.4} vs dry {dry:.4})",
            -db
        );
    }
}

/// At `StretchFactor::MIN` the frames stop overlapping, and the level ripples.
///
/// `hops` pins the synthesis hop at `size / 4` and scales the analysis hop,
/// so at 0.25x the analysis hop reaches the full window: consecutive frames
/// share no samples at all. A phase vocoder reconstructs from the phase
/// *relationship* between overlapping frames, so with zero overlap there is
/// nothing to reconstruct and overlap-add sums frames whose phases are
/// unrelated — constructive in places, cancelling in others.
///
/// Measured: 16 of 256 blocks below 0.01, peaks spanning 0.003 to 0.60. The
/// boundary is sharp — 0.4x and above hold a steady level.
///
/// This is pinned rather than fixed because fixing it means changing the hop
/// geometry (a smaller synthesis hop at slow factors, costing CPU), which is
/// a design decision and not a bug fix. Pinned so it stays a known,
/// bounded property that a later change cannot silently deepen.
#[test]
fn the_slowest_factor_ripples_because_its_frames_do_not_overlap() {
    let mut u = Unit::with_fft_size_and_channels(44_100.0, FftSize::N1024, 1usize);
    u.set_stretch_factor(StretchFactor::MIN);
    let window = u.channels.channels.borrow()[0].geometry.window().get();
    let (analysis, _) = u.hops();
    assert_eq!(
        analysis, window,
        "the premise: at MIN the analysis hop must reach the whole window"
    );

    let mut out = [0.0f32; 1];
    let mut n = 0usize;
    let mut feed = |u: &mut Unit, count: usize, n: &mut usize| {
        let mut peak = 0.0f32;
        for _ in 0..count {
            let t = *n as f32 / 44_100.0;
            let sample = 0.5 * (Radians::TAU.get() * 3000.0 * t).sin();
            *n += 1;
            u.tick(&[sample], &mut out);
            peak = peak.max(out[0].abs());
        }
        peak
    };
    feed(&mut u, 60_000, &mut n);

    let mut peaks = Vec::with_capacity(256);
    for _ in 0..256 {
        peaks.push(feed(&mut u, 64, &mut n));
    }
    let loudest = peaks.iter().copied().fold(0.0f32, f32::max);

    // Still bounded and still producing audio — this is ripple, not a dead
    // filter, and not a runaway.
    assert!(
        loudest > 0.4 && loudest < 1.0,
        "MIN should still reach full level somewhere, bounded: peak {loudest:.4}"
    );
    // And the ripple is real, which is what the sibling test excludes 0.25x
    // for. If this ever stops being true the exclusion should go too.
    assert!(
        peaks.iter().any(|p| *p < 0.01),
        "expected ripple at MIN; if this now holds level, re-include 0.25x \
         in `a_one_to_one_feed_stays_bounded_and_audible_at_every_stretch`"
    );
}

/// Stretching must not change the signal's level.
///
/// The bug this pins cost up to 17.5 dB: `phase_accumulator` started at zero
/// instead of being seeded from the first analysed frame, leaving every bin
/// a permanent per-bin offset so overlap-added frames cancelled instead of
/// summing. Unity was clean (the offset is identically zero there), which is
/// why `vocoder_reconstructs_its_input_at_unity` never caught it.
///
/// **Asserted as an RMS ratio, not liveness.** Every other stretch test in
/// this file checks `> 0.01` or `!= 0.0`, and the broken output was 0.0895 —
/// audible, non-zero, and 12 dB wrong. This is the same blind spot that hid
/// the 60 dB bug documented above.
#[test]
fn stretching_preserves_the_signals_level() {
    // Faster-than-unity factors keep >=75% analysis overlap, so the phase
    // vocoder has the continuity it needs to reconstruct exactly.
    for factor in [1.0f32, 1.5, 2.0, 4.0] {
        let mut u = Unit::with_channels(44_100.0, 1usize);
        u.set_stretch_factor(StretchFactor::new(factor));
        u.allocate();

        let size = 64;
        let mut input = BufferVec::new(1);
        let mut out = BufferVec::new(1);
        let mut phase = 0.0f32;
        let inc = 2.0 * PI * 440.0 / 44_100.0;
        let (mut in_sq, mut in_n) = (0.0f64, 0usize);
        let (mut out_sq, mut out_n) = (0.0f64, 0usize);

        // Skip the fill-up: the first frames are legitimately silent while
        // the FIFO reaches a whole window.
        const WARM: usize = 500;
        for blk in 0..2000 {
            for i in 0..size {
                let s = phase.sin() * 0.5;
                phase += inc;
                input.buffer_mut().set_f32(0, i, s);
                if blk >= WARM {
                    in_sq += (s as f64).powi(2);
                    in_n += 1;
                }
            }
            u.process(size, &input.buffer_ref(), &mut out.buffer_mut());
            if blk >= WARM {
                for i in 0..size {
                    let v = out.buffer_ref().at_f32(0, i) as f64;
                    out_sq += v * v;
                    out_n += 1;
                }
            }
        }

        let gain = ((out_sq / out_n as f64).sqrt() / (in_sq / in_n as f64).sqrt()) as f32;
        assert!(
            (gain - 1.0).abs() < 0.05,
            "stretch {factor}: gain {gain:.4} ({:+.1} dB) — a phase-vocoder \
             reconstruction must preserve level within a few percent",
            20.0 * gain.max(1e-9).log10()
        );
    }
}

/// Below unity the analysis hop grows, and the level drop that follows is
/// geometry, not a bug.
///
/// `hops` pins the synthesis hop and scales the analysis hop, so at 0.5x the
/// analysis overlap falls to 50% and at 0.25x (`StretchFactor::MIN`) to 0% —
/// consecutive frames stop overlapping at all, so there is no phase
/// continuity left to reconstruct from. Pinned so the loss is a known,
/// bounded property rather than something a later change silently deepens.
#[test]
fn slowing_down_loses_level_only_as_far_as_the_overlap_allows() {
    for (factor, floor) in [(0.5f32, 0.85f32), (0.25, 0.75)] {
        let mut u = Unit::with_channels(44_100.0, 1usize);
        u.set_stretch_factor(StretchFactor::new(factor));
        u.allocate();

        let size = 64;
        let mut input = BufferVec::new(1);
        let mut out = BufferVec::new(1);
        let mut phase = 0.0f32;
        let inc = 2.0 * PI * 440.0 / 44_100.0;
        let (mut in_sq, mut in_n) = (0.0f64, 0usize);
        let (mut out_sq, mut out_n) = (0.0f64, 0usize);

        for blk in 0..2000 {
            for i in 0..size {
                let s = phase.sin() * 0.5;
                phase += inc;
                input.buffer_mut().set_f32(0, i, s);
                if blk >= 500 {
                    in_sq += (s as f64).powi(2);
                    in_n += 1;
                }
            }
            u.process(size, &input.buffer_ref(), &mut out.buffer_mut());
            if blk >= 500 {
                for i in 0..size {
                    let v = out.buffer_ref().at_f32(0, i) as f64;
                    out_sq += v * v;
                    out_n += 1;
                }
            }
        }

        let gain = ((out_sq / out_n as f64).sqrt() / (in_sq / in_n as f64).sqrt()) as f32;
        assert!(
            gain > floor && gain <= 1.05,
            "stretch {factor}: gain {gain:.4}, expected within ({floor}, 1.05]"
        );
    }
}

/// PDC must not compensate for a delay that is not happening.
///
/// `route` reports `latency_samples` to fundsp, which delays every parallel
/// branch to match. A bypassing unit copies input to output, so reporting a
/// window there desynchronises the whole graph by 46 ms at the default 2048.
#[test]
fn latency_is_zero_while_bypassing_and_a_window_while_processing() {
    let mut u = Unit::with_channels(44_100.0, 2usize);
    let window = u.channels.channels.borrow()[0].geometry.window().get();

    assert!(!u.is_processing());
    assert_eq!(u.latency_samples(), 0, "bypassing unit claimed latency");

    u.set_stretch_factor(StretchFactor::new(2.0));
    assert_eq!(u.latency_samples(), window);

    // Reachable through ordinary use: `VoiceSlot::set_stretch` keeps the
    // resident filter and writes its atomics, so a voice returned to 1.0 is a
    // built filter sitting at unity — it must stop claiming latency.
    u.set_stretch_factor(StretchFactor::UNITY);
    assert_eq!(
        u.latency_samples(),
        0,
        "unity stretch still claimed latency"
    );

    // Pitch alone is enough to make it real processing.
    u.set_pitch_cents(Cents::new(100.0));
    assert_eq!(u.latency_samples(), window);

    // Disabling bypasses regardless of the atomics.
    u.set_enabled(false);
    assert_eq!(u.latency_samples(), 0, "disabled unit claimed latency");
}

#[test]
fn input_rate_is_unity_when_bypassing() {
    let u = Unit::with_channels(44_100.0, 1usize);
    assert!(!u.is_processing());
    assert_eq!(u.input_rate(), ReadRate::UNITY);

    // Disabled counts as bypassing too.
    let mut u = Unit::with_channels(44_100.0, 1usize);
    u.set_stretch_factor(StretchFactor::new(2.0));
    u.set_enabled(false);
    assert_eq!(u.input_rate(), ReadRate::UNITY);
}

#[test]
fn overlap_add_flushes_subnormals() {
    let mut o = OverlapAdd::new(4);
    o.add_at(0, f32::MIN_POSITIVE / 4.0);
    o.advance(1);

    let mut out = [0.0; 1];
    o.drain(&mut out);
    assert_eq!(out[0], 0.0);
}

#[test]
fn creation_and_width() {
    let unit = Unit::new(44100.0);
    assert_eq!(unit.channels(), ChannelLayout::STEREO);
    assert_eq!(unit.inputs(), 2);
    assert_eq!(unit.outputs(), 2);

    assert_eq!(
        Unit::with_channels(44_100.0, 6usize).channels(),
        ChannelLayout::from(6u16)
    );
}

/// A zero-wide filter would make `inputs()`/`outputs()` lie to the graph.
#[test]
fn zero_width_is_clamped_to_one() {
    assert_eq!(
        Unit::with_channels(44_100.0, 0usize).channels(),
        ChannelLayout::MONO
    );
}

#[test]
fn parameters_round_trip_and_clamp() {
    let unit = Unit::new(44100.0);

    unit.set_stretch_factor(StretchFactor::new(2.0));
    assert!((unit.stretch_factor().get() - 2.0).abs() < 0.001);
    unit.set_pitch_cents(Cents::new(-200.0));
    assert!((unit.pitch_cents().get() + 200.0).abs() < 0.001);

    // Clamped at the unit type's own bounds, not open-coded numbers.
    unit.set_stretch_factor(StretchFactor::new(10.0));
    assert_eq!(unit.stretch_factor().get(), StretchFactor::MAX.get());
    unit.set_stretch_factor(StretchFactor::new(0.1));
    assert_eq!(unit.stretch_factor().get(), StretchFactor::MIN.get());

    unit.set_pitch_cents(Cents::new(5000.0));
    assert_eq!(unit.pitch_cents().get(), MAX_PITCH_CENTS);
    unit.set_pitch_cents(Cents::new(-5000.0));
    assert_eq!(unit.pitch_cents().get(), MIN_PITCH_CENTS);
}

#[test]
fn enabled_flag_gates_processing() {
    let mut unit = Unit::new(44100.0);
    unit.set_stretch_factor(StretchFactor::new(2.0));
    assert!(unit.is_processing());

    unit.set_enabled(false);
    assert!(!unit.is_processing());
    unit.set_enabled(true);
    assert!(unit.is_processing());
}

#[test]
fn unity_parameters_bypass() {
    let mut unit = Unit::new(44100.0);
    assert!(!unit.is_processing());

    let mut output = [0.0f32; 2];
    unit.tick(&[0.5, 0.25], &mut output);
    assert_eq!(output, [0.5, 0.25]);
}

/// Bypass must pass every channel through untouched, not just the front
/// pair.
#[test]
fn six_channel_bypass_passes_all_channels_through() {
    let mut u = Unit::with_channels(44_100.0, 6usize);
    assert!(!u.is_processing(), "unity stretch/pitch should bypass");

    let input: Vec<f32> = (0..6).map(|c| (c + 1) as f32).collect();
    let mut output = [0.0f32; 6];
    u.tick(&input, &mut output);

    for (c, &got) in output.iter().enumerate() {
        assert_eq!(got, (c + 1) as f32, "channel {c}: {output:?}");
    }
}

/// A clone carries the parameters and the width, and starts with clean
/// phase state. Clones happen per graph commit and per voice slot.
#[test]
fn clone_carries_parameters_and_width() {
    let u = Unit::with_channels(44_100.0, 6usize);
    u.set_stretch_factor(StretchFactor::new(1.5));

    let c = u.clone();
    assert_eq!(c.channels(), ChannelLayout::from(6u16));
    assert!((c.stretch_factor().get() - 1.5).abs() < 0.001);

    // The atomics are independent after the clone.
    u.set_stretch_factor(StretchFactor::new(2.0));
    assert!((c.stretch_factor().get() - 1.5).abs() < 0.001);
}

/// `route` must agree with `outputs()`. If it does not, fundsp mis-plans
/// this node's latency — which corrupts PDC without crashing or obviously
/// mis-routing audio, so nothing else in the suite would notice.
#[test]
fn route_width_tracks_outputs_at_every_width() {
    for w in [1usize, 2, 6, 8] {
        let mut u = Unit::with_channels(44_100.0, w);
        let out = u.route(&SignalFrame::new(w), 44_100.0);
        assert_eq!(out.len(), u.outputs(), "at channels={w}");
    }
}

#[test]
fn reset_clears_buffered_audio() {
    let mut u = Unit::with_channels(44_100.0, 1usize);
    u.set_stretch_factor(StretchFactor::new(2.0));

    let input = sine(440.0, 44_100.0, 8192);
    let mut out = [0.0f32; 1];
    for &s in &input {
        u.tick(&[s], &mut out);
    }
    u.reset();

    assert_eq!(u.channels.channels.borrow()[0].input.available(), 0);
    assert_eq!(u.channels.channels.borrow()[0].output.available(), 0);
}

/// Stale audio survives a source discontinuity until something calls
/// [`Unit::reset`] — and `reset` is sufficient to clear it.
///
/// This is a **characterization** test: it passes today and documents the
/// mechanism behind the seek bug rather than gating it. The gate lives one
/// level up, where a transport can actually seek
/// (`voice::voice_pool::tests::a_transport_seek_flushes_stretch_state`).
///
/// What it pins is the two halves of the fix:
///
/// - the leak is **large** — the FIFOs hold up to `window * 4` samples and
///   the phase accumulators keep resynthesising from them, so the output
///   after the input goes silent is at signal level, not at noise level;
/// - `reset()` clears it **exactly**, to zero, not merely to something
///   small.
///
/// The second half is why this test earns its place: if a later change makes
/// `Vocoder::reset` cheaper by clearing less, the fix built on top of it
/// stops working and this fails here rather than in an ear.
#[test]
fn stale_audio_survives_a_discontinuity_until_reset() {
    let fill = |u: &mut Unit, level: f32, n: usize| {
        let mut out = [0.0f32; 1];
        let mut peak = 0.0f32;
        for _ in 0..n {
            u.tick(&[level], &mut out);
            peak = peak.max(out[0].abs());
        }
        peak
    };

    // Prime with DC so "is the output still carrying the old material" is a
    // question about level alone — no phase or frequency argument needed.
    let mut leaking = Unit::with_channels(44_100.0, 1usize);
    leaking.set_stretch_factor(StretchFactor::new(2.0));
    assert!(leaking.is_processing());
    fill(&mut leaking, 0.5, 8192);

    // The discontinuity: the source goes silent. A seek into a silent region
    // looks exactly like this from the filter's side.
    let leaked = fill(&mut leaking, 0.0, 4096);
    assert!(
        leaked > 0.25,
        "expected the pre-discontinuity signal to keep draining; peak {leaked}"
    );

    // Same run, with the flush the fix will perform.
    let mut flushed = Unit::with_channels(44_100.0, 1usize);
    flushed.set_stretch_factor(StretchFactor::new(2.0));
    fill(&mut flushed, 0.5, 8192);
    flushed.reset();

    let after_reset = fill(&mut flushed, 0.0, 4096);
    assert_eq!(
        after_reset, 0.0,
        "reset must clear the FIFOs and phase state exactly, not approximately"
    );
}

/// The gain invariant: at a synthesis hop equal to the analysis hop and
/// unity pitch, the vocoder reconstructs its input.
///
/// Driven at the [`Vocoder`] rather than through [`Unit`], deliberately.
/// `Unit` bypasses at exactly unity, so the public API cannot express "run
/// the FFT path at identity settings" — and nudging the stretch factor off
/// unity to defeat the bypass makes synthesis_hop 257 against an analysis
/// hop of 256, which resamples the output and drifts it against the input.
/// That is a real property, but it is not this one.
///
/// Pins two bugs the pre-existing tests could not see, because every one of
/// them asserted only that output was non-zero:
///
/// - the spurious `1 / size` in synthesis, which attenuated by 60 dB;
/// - the missing [`COLA_GAIN`], which leaves the sum of Hann² frames 3.5 dB
///   hot.
#[test]
fn vocoder_reconstructs_its_input_at_unity() {
    let sample_rate = 44_100.0;
    let fft = FftSize::N1024;
    let size = fft.size().get();
    let hop = fft.hop().get();
    let mut v = Vocoder::new(Unit::geometry(sample_rate, fft));

    // Two tones plus a DC offset, so a corrupted DC or Nyquist bin cannot
    // hide behind the tones.
    let len = size * 8;
    let input: Vec<f32> = (0..len)
        .map(|i| {
            // Accumulate the time base in f64 and narrow once: `sample_rate`
            // used to infer as f32 here, so the reference tone this test
            // compares against was itself built at f32 precision.
            let t = (i as f64 / sample_rate) as f32;
            0.4 * (Radians::TAU.get() * 440.0 * t).sin()
                + 0.2 * (Radians::TAU.get() * 3000.0 * t).sin()
                + 0.1
        })
        .collect();

    let mut out = vec![0.0f32; len];
    let mut written = 0usize;
    for chunk in input.chunks(hop) {
        v.input.push(chunk);
        // Synthesis hop == analysis hop: no time scaling, so output and
        // input advance together.
        v.process(hop, hop);
        written += v.output.drain(&mut out[written..]);
    }

    // The drained stream is aligned with the input, NOT delayed by a window:
    // the first hop of output only becomes available once a whole window has
    // arrived, so the fill-up is absorbed by `available` rather than showing
    // up as a lag. (`Unit::latency_samples` reports one window because that
    // is the delay a *graph* sees before the first sample appears — a
    // different question from where the samples land once they do.)
    //
    // The opening hops still ramp in as the overlap-add sum reaches steady
    // state, so compare the interior.
    let start = size;
    let end = written;
    assert!(end > start, "not enough output: {written} samples");

    let (mut num, mut den) = (0.0f64, 0.0f64);
    for i in start..end {
        let want = input[i] as f64;
        let got = out[i] as f64;
        num += (got - want) * (got - want);
        den += want * want;
    }
    let error = (num / den).sqrt();
    assert!(
        error < 0.02,
        "vocoder should reconstruct at unity; relative error {error:.4}"
    );
}

/// With stretching active, every channel must reach the output — a
/// 6-channel voice through a stretcher that only ran two vocoders would
/// silently lose four channels, and no stereo test can see that.
#[test]
fn six_channel_stretch_reaches_every_channel() {
    let mut u = Unit::with_channels(44_100.0, 6usize);
    u.set_stretch_factor(StretchFactor::new(2.0));
    assert!(u.is_processing());

    let mut seen = [false; 6];
    let mut output = [0.0f32; 6];
    for n in 0..8192 {
        // Distinct per-channel tone so a cross-channel leak is not mistaken
        // for a correct read.
        let input: Vec<f32> = (0..6)
            .map(|c| ((n as f32) * 0.01 * (c + 1) as f32).sin())
            .collect();
        u.tick(&input, &mut output);
        for (c, &s) in output.iter().enumerate() {
            if s.abs() > 1e-6 {
                seen[c] = true;
            }
        }
        if seen.iter().all(|&b| b) {
            break;
        }
    }
    assert!(
        seen.iter().all(|&b| b),
        "channels {:?} never produced output",
        seen.iter()
            .enumerate()
            .filter(|(_, &b)| !b)
            .map(|(c, _)| c)
            .collect::<Vec<_>>()
    );
}

/// Stretching changes how fast the unit walks its SOURCE, not how many output
/// samples it emits.
///
/// The distinction is the whole shape of this unit. It emits exactly one
/// sample per `tick`, because that is `AudioUnit`'s contract; the time-scaling
/// shows up as the source being consumed at `1 / stretch`. So over a fixed
/// number of ticks a 2x stretch consumes half the source a 1x pass does, and a
/// 0.5x stretch consumes twice as much.
///
/// This replaces a test that asserted "2x queues up MORE output than 0.5x".
/// That was true, but only because the surplus was piling into the output ring
/// with nowhere to go — it measured the overrun bug rather than the feature.
/// With the intake paced, both factors emit one sample per tick and the ring
/// stays bounded, so that assertion is now false and the property it meant to
/// check lives on the input side.
#[test]
fn stretch_factor_changes_the_source_consumption_rate() {
    const TICKS: usize = 16_384;

    let consumed = |factor: f32| {
        let mut u = Unit::with_fft_size_and_channels(44_100.0, FftSize::N1024, 1usize);
        u.set_stretch_factor(StretchFactor::new(factor));
        let input = sine(440.0, 44_100.0, TICKS);
        let mut frame = [0.0f32; 1];
        let mut emitted = 0usize;
        for &s in &input {
            u.tick(&[s], &mut frame);
            emitted += 1;
        }
        // Everything the FIFO has seen: what it still holds plus what the
        // frames have retired.
        let seen = u.channels.channels.borrow()[0].input.0.write;
        (seen, emitted)
    };

    let (fast_seen, fast_emitted) = consumed(2.0);
    let (slow_seen, slow_emitted) = consumed(0.5);

    // Output is one-per-tick regardless — that is the contract.
    assert_eq!(fast_emitted, TICKS);
    assert_eq!(slow_emitted, TICKS);

    // 2x walks the source at half rate, 0.5x at double.
    let ratio = slow_seen as f64 / fast_seen as f64;
    assert!(
        (ratio - 4.0).abs() < 0.05,
        "0.5x should consume 4x the source 2.0x does; \
         saw {slow_seen} vs {fast_seen} (ratio {ratio:.3})"
    );
}
