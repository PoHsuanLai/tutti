use super::*;

fn signal(n: usize) -> Vec<f32> {
    let mut state = 0x9e37_79b9_u32;
    (0..n)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        })
        .collect()
}

fn norm(x: &[f32]) -> f64 {
    x.iter()
        .map(|&v| f64::from(v) * f64::from(v))
        .sum::<f64>()
        .sqrt()
}

fn run_forward(f: &StagedRealFft, x: &[f32]) -> Vec<Complex32> {
    let mut scratch = f.new_scratch();
    let mut bins = vec![Complex32::new(0.0, 0.0); f.bins()];
    for step in 0..f.forward_steps() {
        f.forward_step(step, x, &mut bins, &mut scratch);
    }
    bins
}

fn run_inverse(f: &StagedRealFft, mut bins: Vec<Complex32>) -> Vec<f32> {
    let mut scratch = f.new_scratch();
    let mut out = vec![0.0f32; f.size()];
    for step in 0..f.inverse_steps() {
        f.inverse_step(step, &mut bins, &mut out, &mut scratch);
    }
    out
}

/// One step is [`RealFft`], bit for bit, both ways: a layout that does
/// not split keeps its transform.
#[test]
fn one_way_is_the_unsplit_transform() {
    // Under miri the sizes stop at 1024: the property is size-independent
    // and miri runs this about a thousand times slower than native.
    let sizes: &[usize] = if cfg!(miri) {
        &[4, 64, 1024]
    } else {
        &[4, 64, 1024, 4096]
    };
    for &n in sizes {
        let x = signal(n);
        let f = StagedRealFft::new(n, 1);
        assert_eq!((f.forward_steps(), f.inverse_steps()), (1, 1));
        let plain = RealFft::new(n);
        let mut scratch = plain.new_scratch();
        let mut want = vec![Complex32::new(0.0, 0.0); plain.bins()];
        plain.forward(&x, &mut want, &mut scratch);
        let got = run_forward(&f, &x);
        assert!(
            got.iter()
                .zip(&want)
                .all(|(a, b)| a.re.to_bits() == b.re.to_bits() && a.im.to_bits() == b.im.to_bits())
        );
        let mut back = vec![0.0f32; n];
        plain.inverse(&mut want, &mut back, &mut scratch);
        let again = run_inverse(&f, got);
        assert!(
            again
                .iter()
                .zip(&back)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
    }
}

/// A split transform is the DFT (in f64) to the unsplit transform's
/// accuracy: every bin within `2 * eps * log2(n) * |x|_2`, which the
/// unsplit transform meets too (a bin of a float FFT is off by about
/// `eps * log2(n)` of the signal's norm), and the split inverse of the
/// split forward is `n` times the signal within the same bound.
#[test]
fn a_split_transform_is_the_dft() {
    // The DFT reference is O(n^2) in f64 sin/cos. Under miri the 2048
    // cases are left to the native run: every way count and the
    // decimation/combine code are already hit by the smaller sizes.
    let cases: &[(usize, usize)] = if cfg!(miri) {
        &[(16, 2), (64, 2), (64, 4), (256, 4), (512, 8)]
    } else {
        &[
            (16, 2),
            (64, 2),
            (64, 4),
            (256, 4),
            (512, 8),
            (2048, 2),
            (2048, 4),
        ]
    };
    for &(n, ways) in cases {
        let x = signal(n);
        let f = StagedRealFft::new(n, ways);
        let steps = ways + ways.trailing_zeros() as usize;
        assert_eq!(f.forward_steps(), steps);
        assert_eq!(f.inverse_steps(), steps + 1);
        let bound = 2.0 * f64::from(f32::EPSILON) * (n as f64).log2() * norm(&x);
        let bins = run_forward(&f, &x);
        for (k, bin) in bins.iter().enumerate() {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (t, &v) in x.iter().enumerate() {
                let angle = -2.0 * PI * ((k * t) % n) as f64 / n as f64;
                re += f64::from(v) * angle.cos();
                im += f64::from(v) * angle.sin();
            }
            let err = (f64::from(bin.re) - re).hypot(f64::from(bin.im) - im);
            assert!(err <= bound, "n {n} ways {ways} bin {k}: {err} > {bound}");
        }
        let back = run_inverse(&f, bins);
        for (i, (&b, &v)) in back.iter().zip(&x).enumerate() {
            let err = (f64::from(b) - n as f64 * f64::from(v)).abs();
            assert!(
                err <= bound * n as f64 / (n as f64).sqrt() * 2.0,
                "n {n} ways {ways} sample {i}: {err}"
            );
        }
    }
}

/// An empty frame is refused at construction, split or not, with a clear
/// message and not a later index error.
#[test]
#[should_panic(expected = "a transform has at least one sample")]
fn a_zero_size_is_refused() {
    let _ = StagedRealFft::new(0, 4);
}

/// All the transform's state is in its scratch: two transforms stepped
/// alternately, on a scratch each, give the bits of each run alone, so a
/// caller may run one step per host block with anything between.
#[test]
fn the_steps_keep_their_state_in_the_scratch() {
    let f = StagedRealFft::new(512, 4);
    let (x, y) = (signal(512), signal(1024)[512..].to_vec());
    let (alone_x, alone_y) = (run_forward(&f, &x), run_forward(&f, &y));
    let (mut sx, mut sy) = (f.new_scratch(), f.new_scratch());
    let zero = Complex32::new(0.0, 0.0);
    let (mut bx, mut by) = (vec![zero; f.bins()], vec![zero; f.bins()]);
    for step in 0..f.forward_steps() {
        f.forward_step(step, &x, &mut bx, &mut sx);
        f.forward_step(step, &y, &mut by, &mut sy);
    }
    let same = |a: &[Complex32], b: &[Complex32]| {
        a.iter()
            .zip(b)
            .all(|(a, b)| a.re.to_bits() == b.re.to_bits() && a.im.to_bits() == b.im.to_bits())
    };
    assert!(same(&bx, &alone_x) && same(&by, &alone_y));
}
