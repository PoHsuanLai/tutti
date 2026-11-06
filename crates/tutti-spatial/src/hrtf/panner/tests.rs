use super::*;

/// A tetrahedron with delta responses: the smallest sphere that parses.
fn tetrahedron(rate: u32) -> Vec<u8> {
    let mut b = b"HRIR".to_vec();
    for word in [rate, 4, 4, 12] {
        b.extend_from_slice(&word.to_le_bytes());
    }
    for idx in [0u32, 1, 2, 0, 3, 1, 0, 2, 3, 1, 3, 2] {
        b.extend_from_slice(&idx.to_le_bytes());
    }
    for v in [
        [1.0f32, 1.0, 1.0],
        [-1.0, -1.0, 1.0],
        [-1.0, 1.0, -1.0],
        [1.0, -1.0, -1.0],
    ] {
        for x in v
            .into_iter()
            .chain([1.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0])
        {
            b.extend_from_slice(&x.to_le_bytes());
        }
    }
    b
}

/// A clone (the fork's template, and every fork) shares the parsed
/// sphere and its prepared spectra; only a new rate prepares a new set,
/// and a re-prepare at the same rate keeps the one it has.
///
/// Mutations (run): `Clone` preparing its own set from the shared sphere
/// → the first set `ptr_eq` fails; dropping `set_sample_rate`'s
/// same-rate early return → the same-rate assertion fails.
#[test]
fn a_clone_shares_the_sphere_and_its_spectra() {
    let rate = SampleRate(48_000.0);
    let front = (Azimuth::FRONT, Elevation::LEVEL);
    let mut a = HrtfBinaural::new(&tetrahedron(48_000), rate).expect("parses");
    let b = a.clone();
    assert!(Arc::ptr_eq(&a.sphere, &b.sphere));
    assert!(Arc::ptr_eq(&a.set, &b.set));

    a.set_sample_rate(rate, front);
    assert!(Arc::ptr_eq(&a.set, &b.set), "a same-rate prepare rebuilt");
    a.set_sample_rate(SampleRate(96_000.0), front);
    assert!(Arc::ptr_eq(&a.sphere, &b.sphere));
    assert!(!Arc::ptr_eq(&a.set, &b.set));
    assert_eq!(a.set.rate, 96_000);
}

/// Two graph rates above [`MAX_PREPARED_RATE`] share one prepared set,
/// but the de-zipper still glides in the graph's own time: built at
/// 1.536 MHz and prepared at 1.024 MHz, 50 ms is 51 200 samples, exactly
/// 400 steps, and leaves `e^-1` of a move. Left at 1.536 MHz's
/// coefficient (600 steps per 50 ms) it would leave `e^-(2/3)` = 0.51.
///
/// Mutation (run): drop `self.aim.set_sample_rate(sample_rate)` from
/// `set_sample_rate`'s same-set branch → 0.513 of the move is left and
/// the glide assertion fails.
#[test]
fn a_prepare_above_the_ceiling_retunes_the_glide() {
    assert_eq!(STEP, 128, "the rates below assume 128-sample steps");
    let front = (Azimuth::FRONT, Elevation::LEVEL);
    let mut a = HrtfBinaural::new(&tetrahedron(48_000), SampleRate(1_536_000.0)).expect("parses");
    let set = a.set.clone();
    a.set_sample_rate(SampleRate(1_024_000.0), front);
    assert!(Arc::ptr_eq(&a.set, &set), "both rates key to one set");
    assert_eq!(a.set.rate, MAX_PREPARED_RATE);

    let e_inv = (-1.0f32).exp();
    let left = remaining_after(&mut a.aim, 400);
    assert!(
        (left - e_inv).abs() < 1e-4,
        "after 50 ms at 1.024 MHz {left} of the move is left, expected e^-1 = {e_inv}"
    );
}

/// A direction that is NaN, infinite or zero finds no face and takes a
/// real vertex: the weights name vertices the sphere has and sum to one.
/// The renderer is then fed NaN and infinite targets (the node holds the
/// last finite one, so only the panner sees them; a zero direction
/// cannot come from angles, so only the weights are checked with it) and
/// must keep producing finite, non-silent audio: the tetrahedron's
/// deltas pass the constant input through at every vertex.
///
/// Mutations (run), each of which fails the render half on its own
/// (the weights loop disabled): in `HrtfSet::weights`, starting the
/// fallback at `nearest = self.directions.len() as u32` and never
/// updating it → the render hits `spectrum`'s `debug_assert`; and
/// returning zero weights `w: [0.0; 3]` from the fallback → the render
/// is silent and the audibility assertion fails. With the weights loop
/// on, the first mutation also fails the vertex assertion.
#[test]
fn a_degenerate_direction_takes_a_real_vertex_and_renders_finite_audio() {
    let rate = SampleRate(48_000.0);
    let mut p = HrtfBinaural::new(&tetrahedron(48_000), rate).expect("parses");
    let mut hint = 0;
    for dir in [
        [f32::NAN; 3],
        [f32::INFINITY, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [f32::NAN, 1.0, 0.0],
    ] {
        let w = p.set.weights(dir, &mut hint);
        assert!(w.verts.iter().all(|&v| (v as usize) < 4), "{dir:?}: {w:?}");
        assert!(w.w.iter().all(|x| x.is_finite()) && (w.w.iter().sum::<f32>() - 1.0).abs() < 1e-6);
    }
    let input = [0.5f32; 64];
    let (mut left, mut right) = ([0.0f32; 64], [0.0f32; 64]);
    for target in [
        (Azimuth(f32::NAN), Elevation(f32::NAN)),
        (Azimuth(f32::INFINITY), Elevation(0.0)),
    ] {
        let mut peak = 0.0f32;
        for _ in 0..20 {
            p.process(target, Mix::WET, (&input, &input), (&mut left, &mut right));
            assert!(left.iter().chain(&right).all(|s| s.is_finite()));
            peak = left.iter().chain(&right).fold(peak, |m, s| m.max(s.abs()));
        }
        assert!(peak > 0.1, "{target:?} rendered silence (peak {peak})");
    }
}

/// Remaining fraction of a step from 0 to 90 degrees of height after
/// `steps` rendered steps.
fn remaining_after(aim: &mut AngleSmoother, steps: usize) -> f32 {
    aim.reset_to_target(Azimuth::FRONT, Elevation(0.0));
    for _ in 0..steps {
        aim.step(Azimuth::FRONT, Elevation(90.0));
    }
    let (_, el) = aim.current();
    (90.0 - el.get()) / 90.0
}

/// The de-zipper's time constant is 50 ms of audio: after 50 ms of
/// steps a move has `e^-1` of its distance left. At 51.2 kHz, 50 ms is
/// 2560 samples, exactly twenty 128-sample steps; at 102.4 kHz (after a
/// retune) it is forty.
///
/// Mutations (run):
/// - build with a stride of one (`AngleSmoother::new(sample_rate,
///   Samples(1))`) → after twenty steps almost all of the move is left
///   and the assertion fails.
/// - drop `self.stride` from `ExponentialSmoother::set_sample_rate`
///   (retune with a stride of one) → the 102.4 kHz half fails.
#[test]
fn the_glide_has_a_50_ms_time_constant_in_audio_time() {
    assert_eq!(STEP, 128, "the rates below assume 128-sample steps");
    let e_inv = (-1.0f32).exp();

    let mut aim = AngleSmoother::new(SampleRate(51_200.0), Samples(STEP));
    let left = remaining_after(&mut aim, 20);
    assert!(
        (left - e_inv).abs() < 1e-4,
        "after 50 ms at 51.2 kHz {left} of the move is left, expected e^-1 = {e_inv}"
    );

    aim.set_sample_rate(SampleRate(102_400.0));
    let left = remaining_after(&mut aim, 40);
    assert!(
        (left - e_inv).abs() < 1e-4,
        "after 50 ms at 102.4 kHz {left} of the move is left, expected e^-1 = {e_inv}"
    );
}
