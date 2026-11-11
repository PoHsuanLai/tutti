use super::*;

fn frame(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i * 7 + 3) % 11) as f32 * 0.25 - 1.0)
        .collect()
}

/// The textbook DFT of bin `k`, in f64.
fn dft(frame: &[f32], k: usize) -> (f64, f64) {
    let n = frame.len();
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (t, &x) in frame.iter().enumerate() {
        let angle = -2.0 * PI * (k * t) as f64 / n as f64;
        re += x as f64 * angle.cos();
        im += x as f64 * angle.sin();
    }
    (re, im)
}

/// The transform pair against the textbook DFT, so the passes are checked
/// against an analytic figure and not against another FFT. Every size up
/// to 64, even and odd: the even ones include those that are not a
/// multiple of four (an odd bin count, a lone centre bin).
#[test]
fn the_transform_is_the_dft_for_every_size() {
    for n in 1..=64usize {
        let frame = frame(n);
        let fft = RealFft::new(n);
        let mut scratch = fft.new_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
        fft.forward(&frame, &mut bins, &mut scratch);
        for (k, bin) in bins.iter().enumerate() {
            let (re, im) = dft(&frame, k);
            assert!(
                (bin.re as f64 - re).abs() < 1e-4 && (bin.im as f64 - im).abs() < 1e-4,
                "size {n} bin {k}: {bin} != {re} + {im}i"
            );
        }
        assert_eq!(bins[0].im, 0.0, "size {n}: DC is real");
        if n % 2 == 0 {
            assert_eq!(bins[n / 2].im, 0.0, "size {n}: Nyquist is real");
        }

        let mut back = vec![0.0f32; n];
        fft.inverse(&mut bins, &mut back, &mut scratch);
        for (i, (&got, &want)) in back.iter().zip(&frame).enumerate() {
            assert!(
                (got / n as f32 - want).abs() < 1e-5,
                "size {n} sample {i}: {got} / {n} != {want}"
            );
        }
    }
}

/// Odd sizes the analysis kernels run (7, 255, 301) and a prime large
/// enough to leave the small butterflies, against the DFT.
#[test]
fn larger_odd_sizes_match_the_dft() {
    for n in [127usize, 255, 301] {
        let frame: Vec<f32> = (0..n).map(|i| (i as f32 * 0.37).sin() + 0.1).collect();
        let fft = RealFft::new(n);
        let mut scratch = fft.new_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
        fft.forward(&frame, &mut bins, &mut scratch);
        for (k, bin) in bins.iter().enumerate() {
            let (re, im) = dft(&frame, k);
            assert!(
                (bin.re as f64 - re).abs() < 2e-3 && (bin.im as f64 - im).abs() < 2e-3,
                "size {n} bin {k}: {bin} != {re} + {im}i"
            );
        }
    }
}

/// A non-zero imaginary DC or Nyquist is not part of a real signal: the
/// inverse must neither panic nor let those parts reach the signal. Any
/// non-zero value does this; NaN is the worst of them. Even and odd.
#[test]
fn the_inverse_ignores_dc_and_nyquist_imaginary_parts() {
    for n in [64usize, 63] {
        let fft = RealFft::new(n);
        let mut scratch = fft.new_scratch();
        let mut tone = |im_dc: f32, im_ny: f32| {
            let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
            bins[0] = Complex32::new(1.0, im_dc);
            bins[3] = Complex32::new(2.0, -1.0);
            if n % 2 == 0 {
                bins[n / 2] = Complex32::new(0.5, im_ny);
            }
            let mut out = vec![0.0f32; n];
            fft.inverse(&mut bins, &mut out, &mut scratch);
            out
        };
        let clean = tone(0.0, 0.0);
        assert_eq!(clean, tone(0.25, -0.75), "size {n}");
        // The DC bin alone contributes 1.0 to every sample's mean.
        let mean = clean.iter().sum::<f32>() / n as f32;
        assert!((mean - 1.0).abs() < 1e-5, "size {n}: {mean}");
        assert!(tone(f32::NAN, f32::INFINITY).iter().all(|x| x.is_finite()));
    }
}

/// The scratch is shared by both directions, so it must cover the larger
/// of the two plans' needs, at every size.
///
/// Not covered: rustfft 6.4's scalar out-of-place scratch length
/// is 0 at every size, so sizing it from one direction alone still passes.
/// This is a cheap guard for a future rustfft whose scratch is nonzero or
/// differs by direction; it cannot fail today.
#[test]
fn the_scratch_covers_both_directions() {
    // Under miri the powers of two stop at 2^10: planning a 32k transform
    // there takes tens of seconds and adds no new scratch rule.
    let top = if cfg!(miri) { 10 } else { 15 };
    for n in (1..=33usize).chain((6..=top).map(|p| 1usize << p)) {
        let fft = RealFft::new(n);
        assert!(fft.scratch_len() >= fft.forward_scratch_len(), "size {n}");
        assert!(fft.scratch_len() >= fft.inverse_scratch_len(), "size {n}");
        assert_eq!(fft.new_scratch().len(), fft.scratch_len());
    }
}

/// Pins that the FFT is rustfft's *scalar* one, which is what makes a
/// result bit-identical on every CPU. The auto planner (AVX, SSE or NEON)
/// differs from scalar bitwise in nearly every bin of this frame, so a swap
/// to it moves the digest on any machine with SIMD.
///
/// The digest is a self-recorded golden, asserted only where it was
/// recorded: the twiddles come from libm (`sin`, `cos` in f64), which can
/// differ in the last ulp between CRTs. The input is integer-valued, with
/// no libm in it. (It is the digest the time-stretch vocoder's copy of
/// this code recorded, so moving the code here changed no bit.)
///
/// Only the x86_64 Linux/glibc golden exists: aarch64 and the other
/// platforms are covered by the analytic DFT tests alone, which pin
/// correctness but not the scalar choice.
#[cfg(all(target_os = "linux", target_env = "gnu", target_arch = "x86_64"))]
#[test]
fn the_fft_is_scalar_so_results_match_across_cpus() {
    let n = 1024usize;
    let mut state = 12345u32;
    let frame: Vec<f32> = (0..n)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 16) % 2001) as f32 / 1000.0 - 1.0
        })
        .collect();
    let fft = RealFft::new(n);
    let mut scratch = fft.new_scratch();
    let mut bins = vec![Complex32::new(0.0, 0.0); n / 2 + 1];
    fft.forward(&frame, &mut bins, &mut scratch);
    let mut back = vec![0.0f32; n];
    fft.inverse(&mut bins.clone(), &mut back, &mut scratch);

    let mut digest = 0xcbf2_9ce4_8422_2325u64;
    let mut eat = |x: f32| {
        digest = (digest ^ x.to_bits() as u64).wrapping_mul(0x0000_0100_0000_01b3);
    };
    for b in &bins {
        eat(b.re);
        eat(b.im);
    }
    back.iter().copied().for_each(&mut eat);
    assert_eq!(digest, 1_168_166_466_409_112_443);
}

/// The odd path is scalar too: a digest of an odd-size round trip, gated
/// like the even one.
///
/// Like the even one, it exists only on x86_64 Linux/glibc; aarch64 and the
/// other platforms are covered by the analytic DFT tests alone.
#[cfg(all(target_os = "linux", target_env = "gnu", target_arch = "x86_64"))]
#[test]
fn the_odd_fft_is_scalar_too() {
    let n = 255usize;
    let mut state = 777u32;
    let frame: Vec<f32> = (0..n)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 16) % 2001) as f32 / 1000.0 - 1.0
        })
        .collect();
    let fft = RealFft::new(n);
    let mut scratch = fft.new_scratch();
    let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
    fft.forward(&frame, &mut bins, &mut scratch);
    let mut back = vec![0.0f32; n];
    fft.inverse(&mut bins.clone(), &mut back, &mut scratch);

    let mut digest = 0xcbf2_9ce4_8422_2325u64;
    let mut eat = |x: f32| {
        digest = (digest ^ x.to_bits() as u64).wrapping_mul(0x0000_0100_0000_01b3);
    };
    for b in &bins {
        eat(b.re);
        eat(b.im);
    }
    back.iter().copied().for_each(&mut eat);
    assert_eq!(digest, 9_419_405_582_622_979_040);
}
