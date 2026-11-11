use super::*;
use tutti_types::{Azimuth, Elevation};

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
#[test]
fn a_clone_shares_the_sphere_and_its_spectra() {
    let rate = SampleRate(48_000.0);
    let front = Direction::new(Azimuth::FRONT, Elevation::LEVEL);
    let mut a = HrtfBinaural::new(&tetrahedron(48_000), rate, front).expect("parses");
    let b = a.fresh(front);
    assert!(Arc::ptr_eq(&a.sphere, &b.sphere));
    assert!(Arc::ptr_eq(&a.set, &b.set));

    a.set_sample_rate(rate, || front);
    assert!(Arc::ptr_eq(&a.set, &b.set), "a same-rate prepare rebuilt");
    a.set_sample_rate(SampleRate(96_000.0), || front);
    assert!(Arc::ptr_eq(&a.sphere, &b.sphere));
    assert!(!Arc::ptr_eq(&a.set, &b.set));
    assert_eq!(a.set.rate, 96_000);
}

/// Two graph rates above [`MAX_PREPARED_RATE`] share one prepared set, keyed
/// at the ceiling; a set above it holds at most 50 ms at the ceiling.
#[test]
fn a_prepare_above_the_ceiling_keeps_one_set() {
    let front = Direction::new(Azimuth::FRONT, Elevation::LEVEL);
    let mut a =
        HrtfBinaural::new(&tetrahedron(48_000), SampleRate(1_536_000.0), front).expect("parses");
    let set = a.set.clone();
    a.set_sample_rate(SampleRate(1_024_000.0), || front);
    assert!(Arc::ptr_eq(&a.set, &set), "both rates key to one set");
    assert_eq!(a.set.rate, MAX_PREPARED_RATE);
}

/// A direction that is NaN, infinite or zero finds no face and takes a
/// real vertex: the weights name vertices the sphere has and sum to one.
/// The renderer is then fed NaN and infinite targets (the node holds the
/// last finite one, so only the panner sees them; a zero direction
/// cannot come from angles, so only the weights are checked with it) and
/// must keep producing finite, non-silent audio: the tetrahedron's
/// deltas pass the constant input through at every vertex.
#[test]
fn a_degenerate_direction_takes_a_real_vertex_and_renders_finite_audio() {
    let rate = SampleRate(48_000.0);
    let mut p = HrtfBinaural::new(
        &tetrahedron(48_000),
        rate,
        Direction::new(Azimuth::FRONT, Elevation::LEVEL),
    )
    .expect("parses");
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
        Direction::new(Azimuth(f32::NAN), Elevation(f32::NAN)),
        Direction::new(Azimuth(f32::INFINITY), Elevation(0.0)),
    ] {
        let mut peak = 0.0f32;
        for _ in 0..20 {
            p.process(Mix::WET, (&input, &input), (&mut left, &mut right), || {
                target
            });
            assert!(left.iter().chain(&right).all(|s| s.is_finite()));
            peak = left.iter().chain(&right).fold(peak, |m, s| m.max(s.abs()));
        }
        assert!(peak > 0.1, "{target:?} rendered silence (peak {peak})");
    }
}
