//! Analytic properties of the binaural renderer, each pinned against a figure
//! derived from the sphere it renders rather than against another render:
//!
//! - an impulse at a vertex's direction comes out as that vertex's HRIR,
//!   within the FFT's round-off;
//! - a mirror-symmetric sphere renders a source straight ahead with equal
//!   ears, and one hard left with the left ear louder;
//! - a direction no face covers takes the nearest vertex instead of failing;
//! - resampling the sphere keeps its DC gain, and downsampling low-passes
//!   below the new Nyquist;
//! - a malformed sphere is refused when the node is built, and so is one
//!   whose rate or response length would make preparing it unbounded; the
//!   rate it is prepared at is held to a ceiling.

use tutti_core::{Azimuth, Elevation, SampleRate, Samples, Tail};
use tutti_graph::contract::{drive, prepared};
use tutti_graph::{Node, Prepare};
use tutti_spatial::{
    HrtfBinauralError, HrtfBinauralNode, MAX_PREPARED_RATE, MAX_SPHERE_RATE, MIN_SPHERE_RATE,
};

const RATE: u32 = 48_000;
/// The convolution step (`STEP` in `panner.rs`).
const STEP: usize = 128;

/// A sphere in the HRIR byte format. `response(v, ear)` gives vertex `v`'s
/// response for `ear` (0 left, 1 right).
fn sphere_bytes(
    rate: u32,
    len: usize,
    verts: &[[f32; 3]],
    faces: &[[u32; 3]],
    response: impl Fn(usize, usize) -> Vec<f32>,
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"HRIR");
    for word in [
        rate,
        len as u32,
        verts.len() as u32,
        (faces.len() * 3) as u32,
    ] {
        b.extend_from_slice(&word.to_le_bytes());
    }
    for f in faces {
        for idx in f {
            b.extend_from_slice(&idx.to_le_bytes());
        }
    }
    for (v, p) in verts.iter().enumerate() {
        for c in p {
            b.extend_from_slice(&c.to_le_bytes());
        }
        for ear in 0..2 {
            let r = response(v, ear);
            assert_eq!(r.len(), len);
            for s in r {
                b.extend_from_slice(&s.to_le_bytes());
            }
        }
    }
    b
}

/// A regular icosahedron. It is symmetric under `x → -x` (left/right), and
/// straight ahead (`-z`) lies on the edge between vertices 6 and 7, both on
/// the median plane.
fn icosahedron() -> (Vec<[f32; 3]>, Vec<[u32; 3]>) {
    let p = (1.0 + 5.0f32.sqrt()) / 2.0;
    let verts = vec![
        [-1.0, p, 0.0],
        [1.0, p, 0.0],
        [-1.0, -p, 0.0],
        [1.0, -p, 0.0],
        [0.0, -1.0, p],
        [0.0, 1.0, p],
        [0.0, -1.0, -p],
        [0.0, 1.0, -p],
        [p, 0.0, -1.0],
        [p, 0.0, 1.0],
        [-p, 0.0, -1.0],
        [-p, 0.0, 1.0],
    ];
    let faces = vec![
        [0, 11, 5],
        [0, 5, 1],
        [0, 1, 7],
        [0, 7, 10],
        [0, 10, 11],
        [1, 5, 9],
        [5, 11, 4],
        [11, 10, 2],
        [10, 7, 6],
        [7, 1, 8],
        [3, 9, 4],
        [3, 4, 2],
        [3, 2, 6],
        [3, 6, 8],
        [3, 8, 9],
        [4, 9, 5],
        [2, 4, 11],
        [6, 2, 10],
        [8, 6, 7],
        [9, 8, 1],
    ];
    (verts, faces)
}

/// The vertex at `v`'s mirror image across the median plane.
fn mirror(verts: &[[f32; 3]], v: usize) -> usize {
    let [x, y, z] = verts[v];
    verts
        .iter()
        .position(|q| q[0] == -x && q[1] == y && q[2] == z)
        .expect("the icosahedron is left/right symmetric")
}

/// Deterministic decaying noise, distinct per `seed`.
fn shape(seed: usize, len: usize) -> Vec<f32> {
    let mut state = 0x9e37_79b9_u32.wrapping_mul(seed as u32 + 1);
    (0..len)
        .map(|i| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            noise * (-(i as f32) / 40.0).exp()
        })
        .collect()
}

/// The icosahedron with dense, mirror-symmetric responses: vertex `v`'s
/// left ear is `shape(v)` scaled by `1 + s/2`, its right ear `shape(mirror
/// v)` scaled by `1 - s/2`, where `s` is how far left the vertex is (`-x`
/// over its radius). So the right ear at `v` is the left ear at `v`'s
/// mirror, and the near ear is the louder.
fn mirrored_response(verts: &[[f32; 3]], len: usize) -> impl Fn(usize, usize) -> Vec<f32> + '_ {
    move |v, ear| {
        let p = verts[v];
        let s = -p[0] / (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
        let (seed, gain) = if ear == 0 {
            (v, 1.0 + s / 2.0)
        } else {
            (mirror(verts, v), 1.0 - s / 2.0)
        };
        shape(seed, len).into_iter().map(|x| x * gain).collect()
    }
}

/// The bearing and height that point at `p`: the inverse of the panner's
/// `direction_from_degrees` (0 = front/-z, +90 = left/-x, +90 up = +y).
fn bearing_of(p: [f32; 3]) -> (Azimuth, Elevation) {
    let [x, y, z] = p.map(f64::from);
    let r = (x * x + y * y + z * z).sqrt();
    let az = (-x).atan2(-z).to_degrees();
    let el = (y / r).asin().to_degrees();
    (Azimuth(az as f32), Elevation(el as f32))
}

fn node(bytes: &[u8], rate: u32) -> HrtfBinauralNode {
    let rate = SampleRate(f64::from(rate));
    prepared(
        HrtfBinauralNode::new(bytes, rate).expect("the sphere parses"),
        rate,
        64,
    )
}

/// The stereo response to a `[1, 1]` impulse at a settled position, `len`
/// frames read from the declared latency after the impulse.
///
/// The impulse lands 100 frames into a step. Its response then runs into the
/// step after next, whose lags reach back further than the step it landed
/// in, so a history too short for the response loses taps there. (An
/// impulse early in its step would have its whole response rendered before
/// it left even a short history.)
fn ir_of(
    node: &mut HrtfBinauralNode,
    (azimuth, elevation): (Azimuth, Elevation),
    rate: u32,
    len: usize,
) -> (Vec<f32>, Vec<f32>) {
    node.set_position(azimuth, elevation);
    Node::reset(node);
    let latency = node.shape().latency.samples().get();
    let lead = STEP + 100;
    let mut input = vec![0.0f32; lead + latency + len];
    input[lead] = 1.0;
    let (mut left, mut right) = (Vec::new(), Vec::new());
    for chunk in input.chunks(64) {
        let out = drive(node, SampleRate(f64::from(rate)), &[chunk, chunk], &[]);
        left.extend_from_slice(&out[0]);
        right.extend_from_slice(&out[1]);
    }
    let from = lead + latency;
    (
        left[from..from + len].to_vec(),
        right[from..from + len].to_vec(),
    )
}

fn energy(x: &[f32]) -> f32 {
    x.iter().map(|s| s * s).sum()
}

/// At a vertex's own direction the blend is that vertex alone, and the
/// convolution of a unit impulse with it is the response itself. The only
/// error is round-off.
///
/// The tolerance is the standard bound for a convolution through a radix-2
/// FFT pair: an error of order `ε · log2(N)` relative to `‖x‖₂ · ‖h‖₂` per
/// output sample, here with `‖x‖₂ = 1` (the impulse) and
/// `N = 512` (the power of two at or above `128 + 200 - 1`). A factor of 8
/// covers the constant of the two transforms and the multiply. The responses
/// are 200 taps, longer than a step, so the history the overlap-save keeps
/// is exercised too.
///
/// Mutations (run): an FFT length of `(step + ir_len / 2).next_power_of_two()`
/// (256, too short: the history no longer reaches back over the response)
/// → off by 0.025 against a tolerance of 3e-5; dropping the `1 / fft_len` normalization → fails (every
/// sample 512 times too large); writing `y.re` to both ears → the right ear
/// fails.
#[test]
fn an_impulse_at_a_vertex_comes_out_as_that_vertexs_hrir() {
    const LEN: usize = 200;
    let (verts, faces) = icosahedron();
    let response = mirrored_response(&verts, LEN);
    let bytes = sphere_bytes(RATE, LEN, &verts, &faces, &response);
    let mut node = node(&bytes, RATE);
    let fft_len = (STEP + LEN - 1).next_power_of_two() as f32;
    for (v, &vertex) in verts.iter().enumerate() {
        let (left, right) = ir_of(&mut node, bearing_of(vertex), RATE, LEN);
        for (ear, got) in [(0, &left), (1, &right)] {
            let want = response(v, ear);
            let tol = 8.0 * f32::EPSILON * fft_len.log2() * energy(&want).sqrt();
            let worst = got
                .iter()
                .zip(&want)
                .map(|(g, w)| (g - w).abs())
                .fold(0.0f32, f32::max);
            assert!(
                worst <= tol,
                "vertex {v} ear {ear}: off the HRIR by {worst}, tolerance {tol}"
            );
        }
    }
}

/// On a left/right-symmetric sphere, straight ahead blends two median-plane
/// vertices whose ears are identical, so the ears are equal up to round-off.
/// Hard left, the near (left) ear is the louder: every vertex near there has
/// `1 + s/2` against `1 - s/2` with `s` above 0.5, an energy ratio above 3.
///
/// Mutations (run): writing `y.re` to both ears → hard left is balanced and
/// fails; swapping `re` and `im` (left and right) → hard left fails the other
/// way; scaling the right ear by 0.9 → straight ahead fails.
#[test]
fn straight_ahead_is_balanced_and_hard_left_favours_the_left_ear() {
    const LEN: usize = 64;
    let (verts, faces) = icosahedron();
    let bytes = sphere_bytes(RATE, LEN, &verts, &faces, mirrored_response(&verts, LEN));
    let mut node = node(&bytes, RATE);

    let (l, r) = ir_of(&mut node, (Azimuth::FRONT, Elevation::LEVEL), RATE, LEN);
    let (el, er) = (energy(&l), energy(&r));
    assert!(el > 0.1, "straight ahead rendered nothing: {el}");
    assert!(
        (el - er).abs() <= 1e-4 * el,
        "straight ahead: left energy {el}, right {er}"
    );

    let (l, r) = ir_of(&mut node, (Azimuth(90.0), Elevation::LEVEL), RATE, LEN);
    let (el, er) = (energy(&l), energy(&r));
    assert!(
        el > 2.0 * er,
        "hard left: left energy {el} should be well above right {er}"
    );
}

/// A sphere that does not enclose the listener: one front face, and a
/// fourth vertex behind that no face uses. A source behind crosses no face,
/// so the renderer takes the nearest vertex (the one behind) rather than
/// failing on the audio thread. A non-finite bearing renders finite audio.
///
/// Mutations (run): falling back to vertex 0 instead of the nearest → the
/// response behind is the front vertex's and fails; `unreachable!()` in
/// place of the fallback → the test panics.
#[test]
fn a_direction_no_face_covers_takes_the_nearest_vertex() {
    const LEN: usize = 16;
    let verts = [
        [0.0, 0.0, -1.0],
        [-0.5, 0.0, -0.866],
        [0.0, 0.5, -0.866],
        [0.0, 0.0, 1.0],
    ];
    let faces = [[0, 1, 2]];
    // Vertex v: a delta at tap v, of gain v + 1 on the left, half on the right.
    let response = |v: usize, ear: usize| {
        let mut r = vec![0.0f32; LEN];
        r[v] = (v + 1) as f32 / if ear == 0 { 1.0 } else { 2.0 };
        r
    };
    let bytes = sphere_bytes(RATE, LEN, &verts, &faces, response);
    let mut node = node(&bytes, RATE);

    let (l, r) = ir_of(&mut node, (Azimuth(180.0), Elevation::LEVEL), RATE, LEN);
    for (ear, got) in [(0, &l), (1, &r)] {
        let want = response(3, ear);
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert!(
                (g - w).abs() < 1e-5,
                "behind, ear {ear} tap {i}: {g}, the vertex behind has {w}"
            );
        }
    }

    let (l, r) = ir_of(&mut node, (Azimuth(f32::NAN), Elevation::LEVEL), RATE, LEN);
    assert!(
        l.iter().chain(&r).all(|s| s.is_finite()),
        "a non-finite bearing rendered a non-finite sample"
    );
}

/// Resampling a response keeps its frequency response, so its DC gain (the
/// sum of its taps) is the same at the new rate. A sphere measured at
/// 44.1 kHz with a delta of gain `g` mid-response, rendered at 88.2 kHz,
/// turns a constant input into `g` at both rates; unscaled, the doubled tap
/// count would double it.
///
/// The tolerance (2 %) covers the windowed-sinc kernel's own DC error and
/// its truncation at the response's start: the delta sits 24 source taps
/// in, inside the kernel's 32-crossing half-width, where the Blackman window
/// has already taken the cut-off ripple to well under 1 %.
///
/// The response itself is pinned too: the kernel is centred, so the delta
/// at source tap 24 peaks at output tap 48, and its value there is the
/// kernel's centre, `cutoff × from / to` = `0.95 × 0.5` of the gain.
///
/// Mutations (run): dropping the `from / to` scale → the DC gain is 2 g and
/// fails; evaluating the kernel at `t - m + 1.0` (a one-source-tap shift)
/// keeps the gain but moves the peak to tap 46 and fails; a cutoff with no
/// `.min(1.0)` (1.9 when upsampling: no low-pass below the source's
/// Nyquist) keeps the gain but doubles the peak and fails.
#[test]
fn resampling_the_sphere_keeps_its_dc_gain() {
    const LEN: usize = 64;
    const FROM: u32 = 44_100;
    const TO: u32 = 88_200;
    let (verts, faces) = icosahedron();
    let gain = |v: usize, ear: usize| 0.5 + 0.05 * v as f32 + 0.25 * ear as f32;
    let bytes = sphere_bytes(FROM, LEN, &verts, &faces, |v, ear| {
        let mut r = vec![0.0f32; LEN];
        r[24] = gain(v, ear);
        r
    });
    let mut node = node(&bytes, TO);
    // A vertex's own direction, so the blend is that vertex alone.
    let v = 8;
    node.set_position(bearing_of(verts[v]).0, bearing_of(verts[v]).1);
    Node::reset(&mut node);
    let ones = vec![1.0f32; 64];
    let mut last = [0.0f32; 2];
    for _ in 0..64 {
        let out = drive(&mut node, SampleRate(f64::from(TO)), &[&ones, &ones], &[]);
        last = [out[0][63], out[1][63]];
    }
    for (ear, dc) in last.into_iter().enumerate() {
        let want = gain(v, ear);
        assert!(
            (dc - want).abs() <= 0.02 * want,
            "ear {ear}: DC gain {dc} at {TO} Hz, {want} at {FROM} Hz"
        );
    }

    let (l, r) = ir_of(&mut node, bearing_of(verts[v]), TO, 2 * LEN);
    for (ear, ir) in [(0, &l), (1, &r)] {
        let (at, peak) = ir
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, p)| (i, *p))
            .expect("non-empty");
        let want = 0.95 * 0.5 * gain(v, ear);
        assert_eq!(at, 48, "ear {ear}: the resampled delta peaks at tap {at}");
        assert!(
            (peak - want).abs() <= 1e-3 * want,
            "ear {ear}: the resampled delta's peak is {peak}, the kernel's centre {want}"
        );
    }
}

/// Downsampling low-passes below the new Nyquist rate. A sphere measured
/// at 96 kHz with a delta of gain `g` at source tap 48, rendered at 48 kHz:
/// the kernel's cutoff is `0.95 × to / from` = 0.475 of the source rate, so
/// the delta peaks at output tap 24 at the kernel's centre, `cutoff × from /
/// to` = `0.475 × 2` of the gain, and the DC gain (the sum of the taps)
/// stays `g`. Tolerances as in the upsampling test above.
///
/// Mutation (run): a cutoff of a constant `0.95` (no low-pass when
/// `from > to`: the source's whole band aliases into the output's) fails
/// the DC check; with that check disabled it keeps the peak's tap but
/// doubles its value (1.71 against 0.855) and fails there too.
#[test]
fn downsampling_low_passes_below_the_new_nyquist() {
    const LEN: usize = 128;
    const FROM: u32 = 96_000;
    const TO: u32 = 48_000;
    let (verts, faces) = icosahedron();
    let gain = |v: usize, ear: usize| 0.5 + 0.05 * v as f32 + 0.25 * ear as f32;
    let bytes = sphere_bytes(FROM, LEN, &verts, &faces, |v, ear| {
        let mut r = vec![0.0f32; LEN];
        r[48] = gain(v, ear);
        r
    });
    let mut node = node(&bytes, TO);
    let v = 8;
    node.set_position(bearing_of(verts[v]).0, bearing_of(verts[v]).1);
    Node::reset(&mut node);
    let ones = vec![1.0f32; 64];
    let mut last = [0.0f32; 2];
    for _ in 0..64 {
        let out = drive(&mut node, SampleRate(f64::from(TO)), &[&ones, &ones], &[]);
        last = [out[0][63], out[1][63]];
    }
    for (ear, dc) in last.into_iter().enumerate() {
        let want = gain(v, ear);
        assert!(
            (dc - want).abs() <= 0.02 * want,
            "ear {ear}: DC gain {dc} at {TO} Hz, {want} at {FROM} Hz"
        );
    }

    let (l, r) = ir_of(&mut node, bearing_of(verts[v]), TO, LEN / 2);
    for (ear, ir) in [(0, &l), (1, &r)] {
        let (at, peak) = ir
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, p)| (i, *p))
            .expect("non-empty");
        let want = 0.475 * 2.0 * gain(v, ear);
        assert_eq!(at, 24, "ear {ear}: the resampled delta peaks at tap {at}");
        assert!(
            (peak - want).abs() <= 1e-3 * want,
            "ear {ear}: the resampled delta's peak is {peak}, the kernel's centre {want}"
        );
    }
}

/// A regular tetrahedron: the smallest sphere that encloses the listener.
fn tetrahedron() -> (Vec<[f32; 3]>, Vec<[u32; 3]>) {
    let verts = vec![
        [1.0, 1.0, 1.0],
        [-1.0, -1.0, 1.0],
        [-1.0, 1.0, -1.0],
        [1.0, -1.0, -1.0],
    ];
    let faces = vec![[0, 1, 2], [0, 3, 1], [0, 2, 3], [1, 3, 2]];
    (verts, faces)
}

/// The declared tail is the ring-out once the latency is compensated: an
/// impulse through a response whose last tap is non-zero sounds for the
/// last time exactly latency + tail frames after it, and never after. Every
/// vertex has the same response, so the blend cannot move the last tap.
///
/// Mutation (run): `ring_out` counting the latency again (`LATENCY +
/// ir_len - 1`, the old figure) → the declared tail is 327 against the
/// 199 the render shows, and the first assertion fails.
#[test]
fn the_tail_is_the_ring_out_after_the_latency() {
    const LEN: usize = 200;
    let (verts, faces) = tetrahedron();
    let bytes = sphere_bytes(RATE, LEN, &verts, &faces, |_, _| {
        let mut r = vec![0.0f32; LEN];
        r[0] = 1.0;
        r[LEN - 1] = 0.5;
        r
    });
    let mut node = node(&bytes, RATE);
    let (azimuth, elevation) = bearing_of(verts[0]);
    node.set_position(azimuth, elevation);
    Node::reset(&mut node);
    let shape = node.shape();
    let latency = shape.latency.samples().get();
    let lead = STEP + 100;
    let mut input = vec![0.0f32; lead + 4 * (latency + LEN)];
    input[lead] = 1.0;
    let mut last = None;
    let mut at = 0;
    for chunk in input.chunks(64) {
        let out = drive(&mut node, SampleRate(f64::from(RATE)), &[chunk, chunk], &[]);
        for (i, (l, r)) in out[0].iter().zip(&out[1]).enumerate() {
            if l.abs() > 1e-4 || r.abs() > 1e-4 {
                last = Some(at + i);
            }
        }
        at += chunk.len();
    }
    let rings = last.expect("the impulse sounds") - (lead + latency);
    assert_eq!(
        shape.tail,
        Tail::Finite(Samples(rings)),
        "the tail is what sounds after the compensated latency"
    );
    assert_eq!(
        rings,
        LEN - 1,
        "the response spreads the impulse LEN - 1 frames"
    );
}

/// Preparing a sphere resamples it, which scales its response length by
/// the ratio of the rates. The file's rate is held to
/// `MIN_SPHERE_RATE..=MAX_SPHERE_RATE` and its responses to 50 ms there
/// (`rate / 20` frames), so what preparing allocates and computes per
/// vertex is bounded by the prepared rate and not by the file: a 1 Hz
/// sphere with 32-tap responses (32 s) once prepared a 1.5M-tap response at
/// 48 kHz.
///
/// The bound is pinned at both edges, and at the longest response allowed
/// the tail at a new rate is exactly 50 ms of frames there, less one (a
/// tail excludes latency).
///
/// Mutations (run): no lower rate bound → the 1 Hz sphere is refused for
/// its length instead and the `Rate` match fails; `rate > MAX_SPHERE_RATE`
/// dropped → 384 001 Hz builds and fails; no length bound → the 401-tap
/// sphere builds and fails; `len >= max` → the 400-tap sphere is refused
/// and fails; a half-second bound (`rate / 2`) → the 401-tap sphere
/// builds and fails.
#[test]
fn the_spheres_rate_and_response_length_are_bounded() {
    let (verts, faces) = tetrahedron();
    let sphere = |rate: u32, len: usize| {
        sphere_bytes(rate, len, &verts, &faces, |_, _| {
            let mut r = vec![0.0f32; len];
            r[0] = 1.0;
            r
        })
    };
    let build = |bytes: &[u8]| HrtfBinauralNode::new(bytes, SampleRate(48_000.0));

    for rate in [1, MIN_SPHERE_RATE - 1, MAX_SPHERE_RATE + 1] {
        assert!(
            matches!(build(&sphere(rate, 32)).err(), Some(HrtfBinauralError::Rate { rate: r }) if r == rate),
            "a {rate} Hz sphere must be refused for its rate"
        );
    }
    for rate in [MIN_SPHERE_RATE, MAX_SPHERE_RATE] {
        assert!(build(&sphere(rate, 32)).is_ok(), "{rate} Hz is in range");
    }

    let max = MIN_SPHERE_RATE as usize / 20;
    assert_eq!(max, 400, "50 ms at 8 kHz");
    assert!(matches!(
        build(&sphere(MIN_SPHERE_RATE, max + 1)).err(),
        Some(HrtfBinauralError::ResponseTooLong { len, max: m }) if len == max + 1 && m == max
    ));
    let longest = sphere(MIN_SPHERE_RATE, max);
    let mut node = HrtfBinauralNode::new(&longest, SampleRate(f64::from(MIN_SPHERE_RATE)))
        .expect("the longest response allowed builds");
    let to = 2 * MIN_SPHERE_RATE;
    node.prepare(&Prepare::new(SampleRate(f64::from(to)), Samples(64)));
    let fifty_ms = to as usize / 20;
    assert_eq!(
        node.shape().tail,
        Tail::Finite(Samples(fifty_ms - 1)),
        "50 ms at {to} Hz is {fifty_ms} frames"
    );
}

/// The rate the sphere is prepared at is held to `MAX_PREPARED_RATE`,
/// whether it comes from `new` or from `prepare`: a node asked for more
/// prepares the set at the ceiling. Unbounded, `1e10` sized each vertex's
/// spectrum at 2^26 values and aborted on allocation.
///
/// The set's rate shows in the tail: a 48 kHz sphere's 32 taps are
/// `32 * rate / 48 000` frames at `rate`, 512 at the ceiling.
///
/// Mutation (run): dropping the ceiling in `rate_key` → twice the ceiling
/// prepares 1 024-tap responses and the first tail assertion fails.
#[test]
fn the_prepared_rate_is_held_to_its_ceiling() {
    let (verts, faces) = tetrahedron();
    let bytes = sphere_bytes(RATE, 32, &verts, &faces, |_, _| {
        let mut r = vec![0.0f32; 32];
        r[0] = 1.0;
        r
    });
    let at_ceiling = Tail::Finite(Samples(512 - 1));

    let mut node = HrtfBinauralNode::new(&bytes, SampleRate(2.0 * f64::from(MAX_PREPARED_RATE)))
        .expect("builds");
    assert_eq!(node.shape().tail, at_ceiling, "built above the ceiling");

    let mut node2 = HrtfBinauralNode::new(&bytes, SampleRate(1e10)).expect("builds");
    assert_eq!(node2.shape().tail, at_ceiling, "built at 1e10 Hz");

    node.prepare(&Prepare::new(SampleRate(f64::from(RATE)), Samples(64)));
    assert_eq!(node.shape().tail, Tail::Finite(Samples(32 - 1)));
    node.prepare(&Prepare::new(SampleRate(1e10), Samples(64)));
    assert_eq!(node.shape().tail, at_ceiling, "prepared at 1e10 Hz");

    // The set prepared at the ceiling renders: an impulse comes out.
    node2.prepare(&Prepare::new(SampleRate(1e10), Samples(1024)));
    let mut impulse = vec![0.0f32; 1024];
    impulse[0] = 1.0;
    let out = drive(&mut node2, SampleRate(1e10), &[&impulse, &impulse], &[]);
    assert!(out[0].iter().all(|s| s.is_finite()));
    assert!(
        out[0].iter().any(|s| s.abs() > 1e-3),
        "the impulse came out"
    );
}

/// A malformed sphere fails at `new`, on the control thread, with an error
/// naming what is wrong, instead of reaching the audio thread.
///
/// Mutations (run), each in `HrirSphere::parse`: dropping the face-index
/// check → the out-of-range sphere builds and fails; dropping the sample
/// bound → the 1e38 sphere builds and fails; dropping the distance bound
/// (keeping the finiteness check) → the 1e-30 vertex builds and fails;
/// dropping the pre-allocation size check → the claimed header fails on
/// its first face word instead (`needed: 24`) and the size match fails;
/// dropping the trailing-bytes check → the long sphere builds and fails;
/// dropping the usable-face check → the degenerate sphere builds and fails;
/// dropping the vertex-count check → the empty sphere is refused as
/// `NoFace` and fails; dropping the whole-triangles check → the four-index
/// header is refused as `Truncated` and fails.
#[test]
fn a_malformed_sphere_is_refused_at_construction() {
    let (verts, faces) = icosahedron();
    let delta = |_: usize, _: usize| {
        let mut r = vec![0.0f32; 8];
        r[0] = 1.0;
        r
    };
    let good = sphere_bytes(RATE, 8, &verts, &faces, delta);
    assert!(HrtfBinauralNode::new(&good, SampleRate(48_000.0)).is_ok());

    let build = |bytes: &[u8]| HrtfBinauralNode::new(bytes, SampleRate(48_000.0)).err();

    let mut bad_faces = faces.clone();
    bad_faces[3] = [0, 7, 12];
    let bytes = sphere_bytes(RATE, 8, &verts, &bad_faces, delta);
    assert!(
        matches!(
            build(&bytes),
            Some(HrtfBinauralError::FaceIndex {
                face: 3,
                vertex: 12,
                vertices: 12
            })
        ),
        "an out-of-range face index must be refused"
    );

    assert!(matches!(
        build(&good[..good.len() - 1]),
        Some(HrtfBinauralError::Truncated { .. })
    ));
    assert!(matches!(
        build(b"RIFF\0\0\0\0"),
        Some(HrtfBinauralError::NotASphere)
    ));
    let zero_len = sphere_bytes(RATE, 0, &verts, &faces, |_, _| Vec::new());
    assert!(matches!(
        build(&zero_len),
        Some(HrtfBinauralError::Header(_))
    ));
    let empty = sphere_bytes(RATE, 8, &[], &[], delta);
    assert!(
        matches!(build(&empty), Some(HrtfBinauralError::Header(m)) if m.contains("no vertices")),
        "a sphere with no vertices must be refused for it"
    );
    // Four indices: a triangle and one stray. Refused from the header,
    // before the file's size is checked.
    let mut header = b"HRIR".to_vec();
    for word in [RATE, 8, 4, 4] {
        header.extend_from_slice(&word.to_le_bytes());
    }
    assert!(
        matches!(build(&header), Some(HrtfBinauralError::Header(m)) if m.contains("whole number of triangles")),
        "an index count that is not a multiple of three must be refused for it"
    );
    let nan = sphere_bytes(RATE, 8, &verts, &faces, |v, _| {
        vec![if v == 5 { f32::NAN } else { 0.0 }; 8]
    });
    assert!(matches!(
        build(&nan),
        Some(HrtfBinauralError::Vertex { vertex: 5 })
    ));

    // A sample that would overflow the spectrum, and a position too close
    // to the origin to normalize (it became NaN).
    let huge = sphere_bytes(RATE, 8, &verts, &faces, |v, _| {
        vec![if v == 2 { 1e38 } else { 0.0 }; 8]
    });
    assert!(matches!(
        build(&huge),
        Some(HrtfBinauralError::Vertex { vertex: 2 })
    ));
    let mut tiny_verts = verts.clone();
    tiny_verts[4] = [1e-30, 0.0, 0.0];
    let tiny = sphere_bytes(RATE, 8, &tiny_verts, &faces, delta);
    assert!(matches!(
        build(&tiny),
        Some(HrtfBinauralError::Vertex { vertex: 4 })
    ));

    // A header claiming three million indices over a 20-byte file is sized
    // before anything is allocated, and the error carries the size.
    let mut header = b"HRIR".to_vec();
    for word in [RATE, 8, 4, 3_000_000] {
        header.extend_from_slice(&word.to_le_bytes());
    }
    let claimed = (4 * 8 * 2 + 4 * 3 + 3_000_000) * 4 + 20;
    assert!(
        matches!(
            build(&header),
            Some(HrtfBinauralError::Truncated { needed, found: 20 }) if needed == claimed
        ),
        "a header claiming more than the bytes hold is refused for its size"
    );

    let mut long = good.clone();
    long.push(0);
    assert!(matches!(
        build(&long),
        Some(HrtfBinauralError::Trailing { size, found })
            if size == good.len() && found == good.len() + 1
    ));

    // Every face's plane through the origin (each holds an antipodal pair),
    // and no face at all: no direction can be blended.
    let flat = [
        [1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
    ];
    let degenerate = sphere_bytes(RATE, 8, &flat, &[[0, 1, 2], [0, 1, 3]], delta);
    assert!(matches!(
        build(&degenerate),
        Some(HrtfBinauralError::NoFace)
    ));
    let faceless = sphere_bytes(RATE, 8, &flat, &[], delta);
    assert!(matches!(build(&faceless), Some(HrtfBinauralError::NoFace)));
}
