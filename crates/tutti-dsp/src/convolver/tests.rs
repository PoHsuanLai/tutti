use super::*;

/// Direct (naive) convolution — the definition, used as ground truth.
///
/// `y[n] = sum_k x[k] * h[n-k]`. O(N*M) and far too slow for audio, which is
/// why the partitioned FFT exists; but for a few thousand samples in a test
/// it is instant, and it cannot share a bug with the thing it is checking.
fn direct_convolve(x: &[f32], h: &[f32]) -> Vec<f32> {
    let mut y = vec![0.0f32; x.len() + h.len() - 1];
    for (n, &xn) in x.iter().enumerate() {
        if xn == 0.0 {
            continue;
        }
        for (k, &hk) in h.iter().enumerate() {
            y[n + k] += xn * hk;
        }
    }
    y
}

/// Run `x` through the convolver and drop the reported latency.
fn run(ir: &[f32], x: &[f32], block: usize) -> Vec<f32> {
    let mut c = Convolver::new(ir, block);
    let lat = c.latency();
    // Feed `lat` extra zeros so the tail that latency delays still comes out.
    let mut out: Vec<f32> = x
        .iter()
        .chain(std::iter::repeat_n(&0.0, lat))
        .map(|&s| c.process_sample(s))
        .collect();
    out.drain(..lat);
    out
}

/// The convolver must compute a convolution.
///
/// Nothing asserted this. The node's tests check `is_finite()`,
/// `!is_empty()`, and "produces output after latency" — all of which a
/// convolver that scaled wrongly, misaligned its partitions, or dropped its
/// tail would satisfy. This compares against the definition instead.
///
/// Several block sizes, because the partitioning is where a boundary bug
/// lives: a size that divides the input evenly can hide a fence-post error
/// that a ragged one exposes.
#[test]
fn output_matches_direct_convolution() {
    // A short IR with distinct, non-round values, so a scale error or a
    // reversed kernel produces visibly wrong numbers rather than a plausible
    // rearrangement.
    let ir: Vec<f32> = vec![0.7, -0.35, 0.2, 0.9, -0.1, 0.05, 0.42, -0.6];
    // Input with an impulse, silence, and varying material — the impulse
    // alone would only prove the IR is stored, not that mixing is right.
    let mut x = vec![0.0f32; 600];
    x[3] = 1.0;
    x[100] = -0.5;
    for (i, s) in x.iter_mut().enumerate().skip(200).take(300) {
        *s = ((i as f32) * 0.1).sin() * 0.6;
    }

    for block in [2usize, 8, 64, 512] {
        let got = run(&ir, &x, block);
        let want = direct_convolve(&x, &ir);
        let n = got.len().min(want.len());
        assert!(n >= x.len(), "block {block}: too little output to compare");

        let worst = (0..n).fold(0.0f32, |a, i| a.max((got[i] - want[i]).abs()));
        assert!(
            worst < 1e-4,
            "block {block}: output diverges from direct convolution by {worst} \
             (first mismatch near sample {})",
            (0..n)
                .find(|&i| (got[i] - want[i]).abs() > 1e-4)
                .unwrap_or(0)
        );
    }
}

/// A unit impulse IR is the identity, delayed by nothing.
///
/// The simplest possible case, and the one that pins the convolver's *gain*.
/// An FFT convolver that forgot to normalize its inverse transform scales
/// every sample by the transform length — inaudible in a null test against
/// itself, obvious here.
#[test]
fn a_unit_impulse_ir_passes_the_signal_through_unchanged() {
    let ir = [1.0f32];
    let x: Vec<f32> = (0..300).map(|i| ((i as f32) * 0.37).sin() * 0.5).collect();

    for block in [2usize, 16, 128] {
        let got = run(&ir, &x, block);
        for (i, (&g, &want)) in got.iter().zip(x.iter()).enumerate() {
            assert!(
                (g - want).abs() < 1e-5,
                "block {block}: sample {i} came back as {g}, expected {want} — \
                 a unit-impulse IR must be the identity"
            );
        }
    }
}

/// `latency()` must be the delay the output actually has.
///
/// The value is documented as the block size and callers compensate by it,
/// so a wrong figure smears every convolved track's alignment against the
/// rest of the mix — silently, because the audio itself is fine.
#[test]
fn reported_latency_is_the_real_latency() {
    let ir = [1.0f32];
    for block in [2usize, 8, 64, 512] {
        let mut c = Convolver::new(&ir, block);
        let lat = c.latency();

        // Impulse at 0; with a unit IR the output impulse must land at
        // exactly `lat`.
        let n = lat * 3 + 16;
        let out: Vec<f32> = (0..n)
            .map(|i| c.process_sample(if i == 0 { 1.0 } else { 0.0 }))
            .collect();

        let at = out
            .iter()
            .position(|&s| s.abs() > 0.5)
            .unwrap_or_else(|| panic!("block {block}: the impulse never came out"));
        assert_eq!(
            at, lat,
            "block {block}: latency() reports {lat} but the impulse emerged at {at}"
        );
    }
}

/// The block path is the per-sample path, bit for bit, whatever the block
/// size and however the calls straddle the partition boundary.
///
/// Ragged call lengths (1, 7, 64, 3, …) against a small partition are what
/// put a run boundary mid-call, before the first drain, and exactly on the
/// drain — the three places the lockstep argument in `process_block` could
/// be wrong.
#[test]
fn block_path_is_bit_identical_to_the_per_sample_path() {
    let ir: Vec<f32> = vec![0.7, -0.35, 0.2, 0.9, -0.1, 0.05, 0.42, -0.6, 0.3];
    let x: Vec<f32> = (0..700)
        .map(|i| ((i as f32) * 0.173).sin() * 0.8 + if i % 97 == 0 { 0.5 } else { 0.0 })
        .collect();
    for block in [2usize, 8, 64] {
        let mut per_sample = Convolver::new(&ir, block);
        let want: Vec<f32> = x.iter().map(|&s| per_sample.process_sample(s)).collect();

        let mut blocked = Convolver::new(&ir, block);
        let mut got = vec![0.0f32; x.len()];
        let lens = [1usize, 7, 64, 3, 16, 33, 5, 64, 9];
        let (mut at, mut k) = (0, 0);
        while at < x.len() {
            let n = lens[k % lens.len()].min(x.len() - at);
            blocked.process_block(&x[at..at + n], &mut got[at..at + n]);
            at += n;
            k += 1;
        }
        for i in 0..x.len() {
            assert_eq!(
                got[i].to_bits(),
                want[i].to_bits(),
                "partition {block}: sample {i} differs ({} vs {})",
                got[i],
                want[i]
            );
        }
    }
}

/// A NaN input propagates and then leaves: it does not panic (in debug
/// builds either), it reaches the output as through any linear filter,
/// and once its partition has left the spectra history and the overlap,
/// silence in is exactly silence out again.
///
/// The NaN's spectrum is NaN in every bin, including the DC and Nyquist
/// imaginary parts, which the inverse ignores. With an
/// 8-tap IR at block 4 (two partitions), a NaN fed at sample 0 is in the
/// history for two FFT cycles and in the overlap for one more; past the
/// latency plus three blocks the output is pure convolution of zeros.
///
/// The inverse's handling of those parts is tested in
/// `tutti-types`; here the NaN reaches the output with or without them.
#[test]
fn a_nan_input_propagates_and_then_leaves() {
    let ir: Vec<f32> = vec![0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2];
    let block = 4;
    let mut c = Convolver::new(&ir, block);
    let out: Vec<f32> = std::iter::once(f32::NAN)
        .chain(std::iter::repeat_n(0.0, 64))
        .map(|x| c.process_sample(x))
        .collect();
    assert!(
        out.iter().any(|y| y.is_nan()),
        "the NaN never reached the output"
    );
    let latency = c.latency();
    // The NaN is still in the output through the third block after the
    // latency and gone from the fourth on.
    assert!(
        out[latency + 2 * block..latency + 3 * block]
            .iter()
            .any(|y| y.is_nan()),
        "the NaN left early: {:?}",
        &out[latency + 2 * block..latency + 3 * block]
    );
    let clean = latency + 3 * block;
    assert!(
        out[clean..].iter().all(|&y| y == 0.0),
        "the NaN stayed past its partition: {:?}",
        &out[clean..]
    );
}

/// A deterministic IR of `len` taps with decaying, sign-varying values.
fn long_ir(len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| ((i as f32) * 0.37).sin() * (-(i as f32) / 900.0).exp())
        .collect()
}

/// Feed `x` through `process_block` in `lens`-cycled calls.
fn run_blocks(c: &mut Convolver, x: &[f32], lens: &[usize]) -> Vec<f32> {
    let mut got = vec![0.0f32; x.len()];
    let (mut at, mut k) = (0, 0);
    while at < x.len() {
        let n = lens[k % lens.len()].min(x.len() - at);
        c.process_block(&x[at..at + n], &mut got[at..at + n]);
        at += n;
        k += 1;
    }
    got
}

/// A many-partition IR through the incremental path matches the
/// definition (a direct convolution computed here), delayed by exactly
/// `latency()`, for call lengths that divide the partition, ones that do
/// not, ones that straddle the boundary and ones longer than a partition.
///
/// The tolerance is float rounding of a 2000-tap sum of terms below 1.
#[test]
fn incremental_path_matches_direct_convolution_at_any_call_length() {
    let ir = long_ir(2000);
    let x: Vec<f32> = (0..3000)
        .map(|i| ((i as f32) * 0.173).sin() * 0.8 + if i % 211 == 0 { 0.5 } else { 0.0 })
        .collect();
    let want = direct_convolve(&x, &ir);
    let patterns: [&[usize]; 6] = [&[1], &[7], &[64], &[33, 5, 64, 9, 200], &[100], &[300, 17]];
    for part in [32usize, 64, 128] {
        for lens in patterns {
            let mut c = Convolver::with_parts(&ir, part, &[part]);
            let lat = c.latency();
            assert_eq!(lat, part);
            let mut padded = x.clone();
            padded.extend(std::iter::repeat_n(0.0, lat));
            let got = run_blocks(&mut c, &padded, lens);
            for i in 0..x.len() {
                assert!(
                    (got[i + lat] - want[i]).abs() < 2e-3,
                    "partition {part}, calls {lens:?}: sample {i} is {} want {}",
                    got[i + lat],
                    want[i]
                );
            }
            assert!(got[..lat].iter().all(|&s| s == 0.0), "fill-up is silent");
        }
    }
}

/// Linearity: `conv(a·x + b·y) = a·conv(x) + b·conv(y)` through the
/// incremental path, within float tolerance.
///
/// Covers linearity only. A dropped partition or a stale sum is still
/// linear in the input history, so this does not catch them: the
/// direct-convolution and bit-identity tests do (checked: dropping the
/// `accumulated`/`begun` reset in `process` passes this test).
#[test]
fn incremental_path_is_linear() {
    let ir = long_ir(700);
    let n = 1500;
    let x: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.21).sin()).collect();
    let y: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.05).cos() * 0.5).collect();
    let (a, b) = (0.75f32, -1.25f32);
    let mix: Vec<f32> = x.iter().zip(&y).map(|(&p, &q)| a * p + b * q).collect();
    let lens = [13usize, 64, 5];
    let cx = run_blocks(&mut Convolver::new(&ir, 64), &x, &lens);
    let cy = run_blocks(&mut Convolver::new(&ir, 64), &y, &lens);
    let cm = run_blocks(&mut Convolver::new(&ir, 64), &mix, &lens);
    for i in 0..n {
        assert!((cm[i] - (a * cx[i] + b * cy[i])).abs() < 2e-3, "sample {i}");
    }
}

/// However the partitions are split across calls the result is the same bits:
/// the block path (sum spread over the cycle) and the per-sample path
/// (the whole sum at the boundary) agree for a many-partition IR. This
/// pins the split and a loop reversal; an order both paths share is
/// pinned by `the_spread_sum_is_the_ascending_sum`.
#[test]
fn incremental_path_is_bit_identical_to_the_boundary_sum() {
    let ir = long_ir(1500);
    let x: Vec<f32> = (0..2500)
        .map(|i| ((i as f32) * 0.31).sin() * 0.7 + if i % 53 == 0 { 0.3 } else { 0.0 })
        .collect();
    for part in [16usize, 64, 256] {
        let mut per_sample = Convolver::with_parts(&ir, part, &[part]);
        let want: Vec<f32> = x.iter().map(|&s| per_sample.process_sample(s)).collect();
        let got = run_blocks(
            &mut Convolver::with_parts(&ir, part, &[part]),
            &x,
            &[9, 64, 1, 31, 128, 70],
        );
        assert!(
            got.iter()
                .zip(&want)
                .all(|(g, w)| g.to_bits() == w.to_bits()),
            "partition {part}"
        );
    }
}

/// The sum is spread, not deferred: with 8-frame calls into a 64-frame
/// partition no call advances more than its share (plus rounding), and
/// every partition is in before the call that crosses the boundary.
#[test]
fn accumulation_is_spread_across_the_calls_of_a_partition() {
    let part = 64;
    let mut c = Convolver::with_parts(&long_ir(1000), part, &[part]);
    let older = c.spectra.levels[0].segments.len() - 1;
    let x = vec![0.5f32; 8];
    let mut out = vec![0.0f32; 8];
    let share = older * 8 / (part - 8) + 1;
    // Nothing is summed before the first block's transform: the history
    // is zero, so the first cycle is not measured.
    for _ in 0..8 {
        c.process_block(&x, &mut out);
    }
    for cycle in 0..4 {
        let mut last = 0;
        for call in 0..8 {
            c.process_block(&x, &mut out);
            if call < 7 {
                let now = c.levels[0].accumulated;
                assert!(
                    now - last <= share,
                    "cycle {cycle} call {call}: {last}->{now}"
                );
                last = now;
                if call >= 6 {
                    assert_eq!(now, older, "done before the boundary call");
                }
            } else {
                assert_eq!(c.levels[0].accumulated, 0, "the boundary call drained");
            }
        }
    }
}

/// The sum the boundary call receives is the ascending one: partitions
/// `1..count` added to zero in order, with the code's own `re`/`im`
/// formulas. With 4-frame calls into a 16-frame partition the sum is
/// complete after the third call of a cycle; it is recomputed here from
/// the spectra and the history and compared bit for bit. It is same-run
/// data, so it does not depend on libm.
#[test]
fn the_spread_sum_is_the_ascending_sum() {
    let part = 16;
    let mut c = Convolver::with_parts(&long_ir(300), part, &[part]);
    let count = c.spectra.levels[0].segments.len();
    assert!(count > 8);
    let x: Vec<f32> = (0..part * 12)
        .map(|i| ((i as f32) * 0.29).sin() + if i % 5 == 0 { 0.4 } else { 0.0 })
        .collect();
    let mut out = [0.0f32; 4];
    let mut checked = 0;
    for (n, call) in x.chunks(4).enumerate() {
        c.process_block(call, &mut out);
        // Nothing is summed before the first block's transform.
        if n % 4 != 2 || n < 4 {
            continue;
        }
        assert_eq!(c.levels[0].accumulated, count - 1, "sum complete, call {n}");
        let mut want = vec![Complex32::new(0.0f32, 0.0); part + 1];
        for i in 1..count {
            let seg = &c.spectra.levels[0].segments[i];
            let hist = &c.levels[0].history[(c.levels[0].current + i) % count];
            for ((w, a), b) in want.iter_mut().zip(seg).zip(hist) {
                w.re += a.re * b.re - a.im * b.im;
                w.im += a.re * b.im + a.im * b.re;
            }
        }
        for (bin, (g, w)) in c.levels[0].building.iter().zip(&want).enumerate() {
            assert!(
                g.re.to_bits() == w.re.to_bits() && g.im.to_bits() == w.im.to_bits(),
                "call {n} bin {bin}: {g:?} want {w:?}"
            );
        }
        checked += 1;
    }
    assert_eq!(checked, 11);
}

/// A call that crosses a boundary leaves the next cycle's sum untouched
/// (`accumulated` 0, not begun), so it never costs more than the
/// boundary work: the transforms and the newest partition. With calls
/// 20, 20, 20, 50 into a 64-frame partition the 50 crosses with 46
/// frames left over.
#[test]
fn a_call_that_crosses_a_boundary_leaves_the_next_cycle_unstarted() {
    let part = 64;
    let mut c = Convolver::with_parts(&long_ir(1000), part, &[part]);
    let lens = [20usize, 20, 20, 50];
    let mut out = [0.0f32; 50];
    let (mut fill, mut crossings) = (0usize, 0);
    for k in 0..40 {
        let n = lens[k % 4];
        let x = vec![0.25f32; n];
        c.process_block(&x, &mut out[..n]);
        if fill + n > part {
            crossings += 1;
            assert_eq!(c.levels[0].accumulated, 0, "call {k} crossed");
            assert!(!c.levels[0].begun, "call {k} crossed");
        }
        fill = (fill + n) % part;
    }
    assert!(crossings >= 5);
}

/// Only a forward step suppresses the call's spread work: a call that
/// runs a tail level's inverse step still advances its sum of older
/// partitions. This shapes load only (any split of the work into calls
/// renders the same bits), so the observable is `accumulated`. Calls of
/// one frame into a cut 256-frame level: some call that ran an inverse
/// step and no forward one leaves `accumulated` higher than before.
#[test]
fn an_inverse_step_does_not_suppress_the_spread() {
    let mut c = Convolver::with_parts_split(&long_ir(8000), 64, &[64, 256], 16);
    let tail = 1;
    let (forward, all) = (
        c.spectra.levels[tail].forward_steps,
        c.spectra.levels[tail].steps.len(),
    );
    assert!(forward > 1 && all > forward);
    let mut out = [0.0f32; 1];
    let mut advanced = 0;
    for _ in 0..40 * 256 {
        let (step, acc) = (c.levels[tail].step, c.levels[tail].accumulated);
        c.process_block(&[0.25], &mut out);
        let st = &c.levels[tail];
        let inverse_only = step >= forward && st.step > step;
        if inverse_only && st.accumulated > acc {
            advanced += 1;
        }
    }
    assert!(advanced > 0);
}

/// `reset` must clear the tail, not just the input buffer.
///
/// After a reset the convolver has to behave like a fresh one. If the FFT
/// partitions keep their contents, the previous signal's tail bleeds into
/// the next region — audible as a ghost at every discontinuity, which is
/// exactly when `reset` is called.
#[test]
fn reset_clears_the_convolution_tail() {
    let ir: Vec<f32> = vec![0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2];
    let block = 8;

    let mut c = Convolver::new(&ir, block);
    // Drive it hard so there is a substantial tail in flight.
    for _ in 0..64 {
        c.process_sample(1.0);
    }
    c.reset();

    // Silence in must give silence out — any non-zero is leftover tail.
    for i in 0..64 {
        let s = c.process_sample(0.0);
        assert!(
            s.abs() < 1e-6,
            "sample {i} after reset is {s}, expected silence — \
             the previous signal's tail survived the reset"
        );
    }
}

// ---- Non-uniform layouts ------------------------------------------------

/// A noise-like signal in `-1..=1`, not periodic in any block length used here.
fn noise(len: usize) -> Vec<f32> {
    crate::contract::noise(0x2545_f491, len, 1.0)
}

/// An IR of `len` taps that does not decay, so a late partition carries
/// as much as an early one and dropping it cannot hide in the tolerance.
fn flat_ir(len: usize) -> Vec<f32> {
    noise(len + 5)[5..].iter().map(|v| v * 0.5).collect()
}

/// The round-off tolerance of an FFT convolution of `x` with `ir` through
/// transforms of at most `n` points.
///
/// One output is a sum of products, computed as the inverse of a product
/// of transforms. A transform's rounding error is `~ eps * log2(n)` of its
/// input's norm, so an output differs from the exact sum by a small
/// multiple of `eps * log2(n) * |h|_2 * |x|_2` (the norms of the whole IR
/// and the whole input, which bound every partition's and every block's;
/// the partitions' roundings are independent and add in quadrature, which
/// the bound ignores). The constant 2 is about nine times the worst error
/// measured over the layouts below, and `the_tolerance_is_tight_enough`
/// pins that ratio so the bound cannot drift loose: a wrong or missing
/// partition is an error of order `|h_k| |x|`, thousands of times more.
fn tolerance(ir: &[f32], x: &[f32], n: usize) -> f32 {
    let norm = |v: &[f32]| v.iter().map(|a| a * a).sum::<f32>().sqrt();
    2.0 * f32::EPSILON * (n as f32).log2() * norm(ir) * norm(x)
}

/// The layouts the tests force: head partition, then the partitions of the
/// tail levels.
const LAYOUTS: [(usize, &[usize]); 5] = [
    (16, &[16, 64]),
    (16, &[16, 32, 128]),
    (8, &[8, 16, 32, 64]),
    (32, &[32, 128]),
    (4, &[4, 16]),
];

/// The IR lengths that sit on and around every level's first and last tap.
fn boundary_lengths(head: usize, parts: &[usize]) -> Vec<usize> {
    let plan = build_plan(1 << 20, parts);
    let mut lens = vec![1, head - 1, head, head + 1];
    for l in &plan[1..] {
        lens.extend([l.start - 1, l.start, l.start + 1]);
        lens.extend([l.start + l.part - 1, l.start + l.part, l.start + l.part + 1]);
    }
    let last = plan.last().unwrap();
    lens.push(last.start + 2 * last.part + 7);
    lens.retain(|&l| l >= 1);
    lens.sort_unstable();
    lens.dedup();
    lens
}

/// The convolver's output for `x` then enough silence for the whole tail,
/// fed in `lens`-cycled calls, with the latency dropped.
fn render(c: &mut Convolver, x: &[f32], lens: &[usize]) -> Vec<f32> {
    let lat = c.latency();
    let mut padded = x.to_vec();
    padded.extend(std::iter::repeat_n(0.0, lat + c.ir_length()));
    let mut got = run_blocks(c, &padded, lens);
    assert!(
        got[..lat].iter().all(|&s| s == 0.0),
        "the fill-up is exact silence"
    );
    got.drain(..lat);
    got
}

/// Every layout, at IR lengths that end exactly on, one short of and one
/// past each level's first and last tap, equals the direct convolution
/// (the definition) within round-off, through the whole tail, for call
/// lengths that divide a partition, ones that do not, ones that straddle
/// the transforms' frames and ones longer than the longest partition.
///
/// The tolerance is [`tolerance`]'s.
#[test]
fn non_uniform_output_equals_direct_convolution() {
    let x = noise(900);
    let patterns: [&[usize]; 5] = [&[1], &[7, 13], &[61], &[64, 3, 127], &[5000]];
    for (head, parts) in LAYOUTS {
        for len in boundary_lengths(head, parts) {
            let ir = flat_ir(len);
            let want = direct_convolve(&x, &ir);
            let tol = tolerance(&ir, &x, 2 * parts[parts.len() - 1]);
            for lens in patterns {
                let mut c = Convolver::with_parts(&ir, head, parts);
                let got = render(&mut c, &x, lens);
                for (i, w) in want.iter().enumerate() {
                    assert!(
                        (got[i] - w).abs() <= tol,
                        "parts {parts:?}, {len} taps, calls {lens:?}: frame {i} is {} want {w} (tol {tol})",
                        got[i]
                    );
                }
                let rest = got[want.len()..]
                    .iter()
                    .fold(0.0f32, |a, &s| a.max(s.abs()));
                assert!(
                    rest <= tol,
                    "parts {parts:?}, {len} taps: ringing after the tail {rest}"
                );
            }
        }
    }
}

/// The tolerance is within a small factor of the error the layouts
/// actually make, so it cannot be so loose that a wrong partition passes.
/// Checked at IR lengths of 700, 3000 and 9000 taps (the longer ones
/// exercise transforms of up to 128 points with many partitions per
/// level), over every layout.
#[test]
fn the_tolerance_is_tight_enough() {
    let x = noise(900);
    let mut worst_ratio = 0.0f32;
    for len in [700, 3000, 9000] {
        for (head, parts) in LAYOUTS {
            let ir = flat_ir(len);
            let want = direct_convolve(&x, &ir);
            let mut c = Convolver::with_parts(&ir, head, parts);
            let got = render(&mut c, &x, &[61]);
            let err = want
                .iter()
                .zip(&got)
                .fold(0.0f32, |a, (w, g)| a.max((w - g).abs()));
            worst_ratio = worst_ratio.max(err / tolerance(&ir, &x, 2 * parts[parts.len() - 1]));
        }
    }
    assert!(
        (0.02..0.5).contains(&worst_ratio),
        "the measured error is {worst_ratio} of the tolerance"
    );
}

/// An impulse in gives the IR out, at exactly the declared latency: exact
/// silence for the `latency` frames, then the IR's taps in order from the
/// impulse's frame.
#[test]
fn an_impulse_gives_the_ir_at_the_declared_latency() {
    for (head, parts) in LAYOUTS {
        let last = parts[parts.len() - 1];
        let ir = flat_ir(boundary_lengths(head, parts).into_iter().max().unwrap());
        let mut x = vec![0.0f32; 11];
        x[3] = 1.0;
        let mut c = Convolver::with_parts(&ir, head, parts);
        assert_eq!(c.latency(), head);
        let lat = c.latency();
        let mut padded = x.clone();
        padded.extend(std::iter::repeat_n(0.0, lat + ir.len()));
        let got = run_blocks(&mut c, &padded, &[9, 1, 40]);
        assert!(
            got[..lat].iter().all(|&s| s == 0.0),
            "parts {parts:?}: the fill-up is exact silence"
        );
        let tol = tolerance(&ir, &x, 2 * last);
        // Before the impulse (inside its block, where the transforms'
        // rounding is) the output is within round-off of silence.
        assert!(
            got[lat..lat + 3].iter().all(|&s| s.abs() <= tol),
            "parts {parts:?}: {:?}",
            &got[lat..lat + 3]
        );
        for (k, &h) in ir.iter().enumerate() {
            assert!(
                (got[lat + 3 + k] - h).abs() <= tol,
                "parts {parts:?}: tap {k} is {} want {h}",
                got[lat + 3 + k]
            );
        }
    }
}

/// The IR's last tap reaches the output, in the last level, whether the
/// tap is the level's first or last or the IR ends part-way through a
/// partition: a unit impulse through an IR of zeros but for its last tap
/// comes out as a single unit at `latency + len - 1`.
#[test]
fn the_tail_reaches_the_output() {
    for (head, parts) in LAYOUTS {
        let plan = build_plan(1 << 20, parts);
        let last = plan[plan.len() - 1];
        assert!(parts.len() > 1);
        for len in [
            last.start + 1,
            last.start + last.part / 2,
            last.start + last.part,
            last.start + last.part + 1,
        ] {
            let mut ir = vec![0.0f32; len];
            ir[len - 1] = 1.0;
            let mut c = Convolver::with_parts(&ir, head, parts);
            let spectra = Arc::clone(c.spectra());
            assert_eq!(spectra.levels.len(), parts.len(), "the tail level exists");
            let mut x = vec![0.0f32; 1];
            x[0] = 1.0;
            let got = render(&mut c, &x, &[3, 50]);
            for (n, &s) in got.iter().enumerate() {
                let want = if n == len - 1 { 1.0 } else { 0.0 };
                assert!(
                    (s - want).abs() < 1e-4,
                    "parts {parts:?}, {len} taps: frame {n} is {s} want {want}"
                );
            }
        }
    }
}

/// The render does not depend on how the host cuts its calls: odd
/// lengths, one frame, a call longer than the longest partition, and the
/// per-sample path all give the same bits, through every level.
#[test]
fn the_render_is_independent_of_the_call_lengths() {
    let x = noise(2500);
    for (head, parts) in LAYOUTS {
        let ir = flat_ir(700);
        let mut per_sample = Convolver::with_parts(&ir, head, parts);
        let want: Vec<f32> = x.iter().map(|&s| per_sample.process_sample(s)).collect();
        let patterns: [&[usize]; 7] = [
            &[3],
            &[7, 13, 1],
            &[head - 1, head + 1],
            &[61, 64, 127],
            &[509],
            &[1000, 1, 2],
            &[5000],
        ];
        for lens in patterns {
            let got = run_blocks(&mut Convolver::with_parts(&ir, head, parts), &x, lens);
            assert!(
                got.iter()
                    .zip(&want)
                    .all(|(g, w)| g.to_bits() == w.to_bits()),
                "parts {parts:?}, calls {lens:?}"
            );
        }
    }
    // The layout the cost model picks for a long IR, at odd calls.
    let ir = flat_ir(6000);
    let mut per_sample = Convolver::new(&ir, 32);
    assert!(per_sample.spectra().levels.len() > 1);
    let want: Vec<f32> = x.iter().map(|&s| per_sample.process_sample(s)).collect();
    let got = run_blocks(&mut Convolver::new(&ir, 32), &x, &[33, 5, 100, 1, 31]);
    assert!(
        got.iter()
            .zip(&want)
            .all(|(g, w)| g.to_bits() == w.to_bits())
    );
}

/// The layout of every length and head is well formed: levels are
/// contiguous from tap 0 and cover the IR with no empty last partition;
/// a tail level starts at its own partition size, which is the slack that
/// makes its output due `block_size` frames after its uniform latency;
/// the offsets of its two transforms fall in that slack, in order, inside
/// one partition; and the rings hold what the levels read and write.
#[test]
fn layouts_are_contiguous_and_schedule_inside_their_slack() {
    // Forced layouts of three and four levels follow the same rule.
    for (head, parts) in LAYOUTS {
        let plan = build_plan(1 << 20, parts);
        for (j, p) in plan.iter().enumerate().skip(1) {
            assert_eq!(p.start, p.part, "{parts:?} level {j}");
            assert_eq!(
                p.start,
                plan[j - 1].start + plan[j - 1].count * plan[j - 1].part
            );
        }
        assert_eq!(plan[0].count * head, parts.get(1).copied().unwrap_or(head));
    }
    for head in [2usize, 4, 16, 64, 512, 2048, 8192] {
        for len in [1, 2, 5, 100, 1000, 3000, 24_000, 96_000, 240_000] {
            let ir = vec![0.25f32; len];
            let s = IrSpectra::new(&ir, head);
            let plan = plan_levels(len, head);
            assert_eq!(plan.len(), s.levels.len());
            let mut next = 0;
            for (j, (p, l)) in plan.iter().zip(&s.levels).enumerate() {
                assert_eq!(p.start, next, "head {head}, {len} taps: contiguous");
                next += p.count * p.part;
                assert!(p.start < len, "no level starts past the IR");
                assert!(p.part <= MAX_PARTITION.max(head));
                assert!(p.part == head || p.part <= MAX_TAIL_RATIO * head);
                assert_eq!(l.delay, p.start + head - p.part);
                if j > 0 {
                    assert_eq!(p.start, p.part, "a tail level starts at its own size");
                    assert!(l.delay >= head, "slack of a head partition");
                } else {
                    assert_eq!((l.delay, &l.steps[..]), (0, &[0usize, 0][..]));
                }
                assert!(l.steps.windows(2).all(|w| w[0] <= w[1]));
                assert!(l.steps.iter().all(|&o| o <= l.delay && o < l.part));
                assert!(0 < l.forward_steps && l.forward_steps < l.steps.len());
                assert!(s.input_ring >= l.part + l.steps[0]);
                assert!(s.output_ring > l.delay + l.part);
                assert!(s.input_ring.is_power_of_two() && s.output_ring.is_power_of_two());
            }
            let last = plan.last().unwrap();
            assert!(
                next >= len && next - last.part < len,
                "no empty last partition"
            );
        }
    }
}

/// A 6000-tap IR at a 32-frame head is cut into more than one level (the
/// node's long-IR render test relies on it).
#[test]
fn a_long_ir_is_cut_into_levels() {
    let ir = vec![0.1f32; 6000];
    assert!(Convolver::new(&ir, 32).spectra().shape().len() > 1);
}

/// The cost model keeps the uniform layout where an extra level does not
/// pay, and cuts a long IR into a head and larger tail partitions at most
/// [`MAX_TAIL_RATIO`] times the head; a head of [`MAX_PARTITION`] or more
/// has no larger partition to offer. A chosen non-uniform layout costs at
/// most [`MIN_GAIN`] of the uniform one.
#[test]
fn the_cost_model_picks_uniform_for_short_irs_and_levels_for_long_ones() {
    let shape = |len: usize, head: usize| Convolver::new(&vec![0.1; len], head).spectra().shape();
    assert_eq!(shape(0, 512), vec![]);
    assert_eq!(shape(100, 512), vec![(512, 1)]);
    assert_eq!(shape(3 * 512, 512), vec![(512, 3)]);
    assert_eq!(shape(24_000, 512), vec![(512, 47)]);
    assert_eq!(shape(240_000, 8192), vec![(8192, 30)]);
    let long = shape(240_000, 512);
    assert!(long.len() >= 2, "{long:?}");
    assert_eq!(long[0].0, 512);
    assert!(long[1..].iter().all(|&(p, _)| p > 512 && p <= 4 * 512));
    let covered: usize = long.iter().map(|&(p, c)| p * c).sum();
    assert!(covered >= 240_000);
    let plan = plan_levels(240_000, 512);
    let uniform = [Plan {
        part: 512,
        start: 0,
        count: 240_000usize.div_ceil(512),
    }];
    assert!(plan_cost(&plan) <= MIN_GAIN * plan_cost(&uniform));
}

/// No host block carries two steps of the tail transforms, and a split
/// transform's steps land in different blocks. With calls of one slot
/// (the head partition over the tail levels' steps plus one), no call
/// holds two steps of one level or steps of two levels, and the head's
/// two (it has no slack) share a call with nothing else. The last two
/// layouts split their tail transforms into up to eight and sixteen
/// sub-FFTs, so each transform's steps are spread over that many calls.
#[test]
fn transforms_land_in_different_host_blocks() {
    let steps_run = |st: &LevelState, lv: &Level| {
        ((st.base - lv.part as u64) / lv.part as u64) as usize * lv.steps.len() + st.step
    };
    for (head, parts, max_sub) in [
        (64usize, &[64usize, 128, 256][..], 1024usize),
        (64, &[64, 256], 1024),
        (32, &[32, 64, 128], 1024),
        (512, &[512, 1024, 2048], 256),
        (128, &[128, 256, 512], 32),
    ] {
        let ir = flat_ir(3000);
        let mut c = Convolver::with_parts_split(&ir, head, parts, max_sub);
        let split = c.spectra.levels.iter().any(|l| l.forward_steps > 1);
        assert_eq!(split, max_sub < 1024, "{parts:?}");
        let tail: usize = c.spectra.levels[1..].iter().map(|l| l.steps.len()).sum();
        let call = head / (tail + 1);
        let x = vec![0.1f32; call];
        let mut out = vec![0.0f32; call];
        let mut busy_calls = 0;
        for k in 0..20 * 4096 / call {
            let before: Vec<usize> = c
                .levels
                .iter()
                .zip(&c.spectra.levels)
                .map(|(st, lv)| steps_run(st, lv))
                .collect();
            c.process_block(&x, &mut out);
            let events: Vec<usize> = c
                .levels
                .iter()
                .zip(&c.spectra.levels)
                .zip(&before)
                .map(|((st, lv), &b)| steps_run(st, lv) - b)
                .collect();
            let total: usize = events.iter().sum();
            if total > 0 {
                busy_calls += 1;
            }
            if events[0] > 0 {
                assert_eq!(total, 2, "call {k}: the head's pair is alone: {events:?}");
            } else {
                assert!(total <= 1, "call {k}: {events:?}");
            }
        }
        assert!(busy_calls > 20);
    }
}

/// Layouts whose tail transforms are cut into sub-FFTs: head, partitions
/// and the largest sub-FFT. The last cuts a 64-frame partition into
/// sixteen, more steps than it has frames of slack, so its steps share
/// frames.
const SPLIT_LAYOUTS: [(usize, &[usize], usize); 4] = [
    (64, &[64, 256], 64),
    (32, &[32, 64, 128], 16),
    (16, &[16, 64], 8),
    (16, &[16, 64], 4),
];

/// A split transform is a convolution: every split layout, at IR lengths
/// on and around each level's first and last tap, equals the direct
/// convolution within [`tolerance`] (the splitting adds a combine pass,
/// not a transform stage, so the bound is the unsplit one's), and equals
/// the same layout unsplit within twice that, being off from the exact
/// sum by at most the tolerance each. Call lengths divide a partition,
/// straddle the steps' frames, and exceed the longest partition.
#[test]
fn split_output_equals_direct_convolution() {
    let x = noise(900);
    let patterns: [&[usize]; 4] = [&[1], &[7, 13], &[64, 3, 127], &[5000]];
    for (head, parts, max_sub) in SPLIT_LAYOUTS {
        for len in boundary_lengths(head, parts) {
            let ir = flat_ir(len);
            let want = direct_convolve(&x, &ir);
            let tol = tolerance(&ir, &x, 2 * parts[parts.len() - 1]);
            for lens in patterns {
                let mut c = Convolver::with_parts_split(&ir, head, parts, max_sub);
                assert!(len <= parts[1] || c.spectra.levels.iter().any(|l| l.forward_steps > 1));
                let got = render(&mut c, &x, lens);
                let unsplit = render(&mut Convolver::with_parts(&ir, head, parts), &x, lens);
                for (i, w) in want.iter().enumerate() {
                    assert!(
                        (got[i] - w).abs() <= tol && (got[i] - unsplit[i]).abs() <= 2.0 * tol,
                        "parts {parts:?}/{max_sub}, {len} taps, calls {lens:?}: frame {i} is {} \
                         want {w} (unsplit {}, tol {tol})",
                        got[i],
                        unsplit[i]
                    );
                }
                let rest = got[want.len()..]
                    .iter()
                    .fold(0.0f32, |a, &s| a.max(s.abs()));
                assert!(
                    rest <= tol,
                    "parts {parts:?}/{max_sub}, {len} taps: ringing after the tail {rest}"
                );
            }
        }
    }
}

/// The render of a split layout does not depend on how the host cuts its
/// calls: one frame at a time, odd lengths, and calls that hold several
/// steps or none give the same bits, though the steps of a transform
/// land in different calls.
#[test]
fn a_split_render_is_independent_of_the_call_lengths() {
    let x = noise(2500);
    for (head, parts, max_sub) in SPLIT_LAYOUTS {
        let ir = flat_ir(700);
        let mut per_sample = Convolver::with_parts_split(&ir, head, parts, max_sub);
        let want: Vec<f32> = x.iter().map(|&s| per_sample.process_sample(s)).collect();
        let patterns: [&[usize]; 5] = [
            &[3],
            &[7, 13, 1],
            &[head - 1, head + 1],
            &[61, 64, 127],
            &[5000],
        ];
        for lens in patterns {
            let mut c = Convolver::with_parts_split(&ir, head, parts, max_sub);
            let got = run_blocks(&mut c, &x, lens);
            assert!(
                got.iter()
                    .zip(&want)
                    .all(|(g, w)| g.to_bits() == w.to_bits()),
                "parts {parts:?}/{max_sub}, calls {lens:?}"
            );
        }
    }
}

/// An empty IR is silence at the declared latency, with no panic.
#[test]
fn an_empty_ir_is_silence() {
    let mut c = Convolver::new(&[], 64);
    let x = noise(300);
    let got = run_blocks(&mut c, &x, &[7, 100]);
    assert!(got.iter().all(|&s| s == 0.0));
    assert_eq!(c.latency(), 64);
}

/// `reset` clears every level, not just the head: after a reset, silence
/// in is silence out, with a tail level mid-flight.
#[test]
fn reset_clears_every_level() {
    let ir = flat_ir(400);
    let mut c = Convolver::with_parts(&ir, 16, &[16, 64]);
    let x = noise(500);
    run_blocks(&mut c, &x, &[13]);
    c.reset();
    let silence = vec![0.0f32; 1000];
    let got = run_blocks(&mut c, &silence, &[29]);
    assert!(got.iter().all(|&s| s == 0.0));
}

/// After `reset`, a convolver renders a signal bit for bit as a fresh one
/// does, whatever junk it had taken in before: the reset restores the
/// whole running state (every level's schedule, sums, history and
/// overlap, and both rings), not just the silence. Junk lengths cross
/// slots, level partitions and ring wraps, with call lengths that
/// straddle them; layouts are non-uniform, uniform with several
/// partitions, and the cost model's own pick for a long IR.
#[test]
fn a_reset_convolver_renders_as_a_fresh_one() {
    let x = noise(1500);
    let ir = flat_ir(1200);
    let cases: Vec<(&str, Box<dyn Fn() -> Convolver>)> = vec![
        (
            "non-uniform [16, 64]",
            Box::new(|| Convolver::with_parts(&ir, 16, &[16, 64])),
        ),
        (
            "non-uniform [8, 16, 32, 64]",
            Box::new(|| Convolver::with_parts(&ir, 8, &[8, 16, 32, 64])),
        ),
        (
            "uniform [16]",
            Box::new(|| Convolver::with_parts(&ir, 16, &[16])),
        ),
        (
            "uniform [64]",
            Box::new(|| Convolver::with_parts(&ir, 64, &[64])),
        ),
        (
            "split [16, 64] in eight",
            Box::new(|| Convolver::with_parts_split(&ir, 16, &[16, 64], 8)),
        ),
        (
            "cost model, 512",
            Box::new(|| Convolver::new(&flat_ir(100_000), 512)),
        ),
    ];
    for (name, make) in &cases {
        let want = render(&mut make(), &x, &[61]);
        for n in [1, 37, 700, 2049, 8191] {
            for lens in [&[1usize][..], &[13, 100], &[5000]] {
                let mut c = make();
                run_blocks(&mut c, &noise(n + 3)[3..], lens);
                c.reset();
                let got = render(&mut c, &x, &[61]);
                assert!(
                    got.iter()
                        .zip(&want)
                        .all(|(a, b)| a.to_bits() == b.to_bits()),
                    "{name}: after {n} junk frames in calls {lens:?} the render differs from a fresh one's"
                );
            }
        }
    }
}
