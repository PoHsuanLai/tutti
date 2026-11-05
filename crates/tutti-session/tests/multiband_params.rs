//! The multiband compressor's indexed params through the session (doc 013,
//! "Param addresses"): `Graph::set_param`, a modulation route and a fork of
//! the graph reach one band's address and leave the others alone.
//!
//! The rig is a stereo node at unity ratio with a 100 Hz tone in the left
//! channel and a 6 kHz tone in the right, across a 1 kHz crossover. Each
//! channel is then one band's: the bands are phase-aligned, so a tone of
//! amplitude `A` leaves at `A * (lp * g0 + hp * g1)`, with `lp = 1 / (1 + x^4)`
//! and `hp = x^4 / (1 + x^4)` for `x` the warped ratio of the tone to the
//! split, and `g0`, `g1` the bands' make-up gains. The left tone is band 0's
//! (`hp` is 1e-4 of it) and the right tone band 1's (`lp` is 6e-4).

use std::f64::consts::{PI, TAU};

use tutti_core::{
    Amplitude, AudioNode, ChannelLayout, InterleavedMut, MotionEvent, SampleRate, Samples,
};
#[cfg(feature = "modulation")]
use tutti_mod::Lfo;
use tutti_nodes::testing::Osc;
use tutti_nodes::{MultibandCompressorNode, MultibandControls};
#[cfg(feature = "modulation")]
use tutti_session::ParamRef;
use tutti_session::{GraphSource, Session};
#[cfg(feature = "modulation")]
use tutti_types::{BeatDuration, Depth};
use tutti_types::{Db, Hz, UnitAddr, UnitParam};

const RATE: f64 = 48_000.0;
/// The tones' amplitude.
const A: f64 = 0.4;
const LOW: f64 = 100.0;
const HIGH: f64 = 6_000.0;
const SPLIT: f64 = 1_000.0;

const MAKEUP0: UnitAddr = UnitAddr::new(UnitParam::GainDb, 0);
const MAKEUP1: UnitAddr = UnitAddr::new(UnitParam::GainDb, 1);

fn warp(f: f64) -> f64 {
    (PI * f / RATE).tan() / (PI * SPLIT / RATE).tan()
}

/// Band 0's and band 1's share of a tone at `hz`.
fn shares(hz: f64) -> (f64, f64) {
    let x4 = warp(hz).powi(4);
    (1.0 / (1.0 + x4), x4 / (1.0 + x4))
}

fn gain(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// What the tone at `hz` leaves at, for make-ups `g0` and `g1` in dB.
fn expected(hz: f64, g0: f64, g1: f64) -> f64 {
    let (lp, hp) = shares(hz);
    A * (lp * gain(g0) + hp * gain(g1))
}

/// An offline stereo session with the rig wired to the outputs, and the
/// node's controls.
fn rig() -> (Session, AudioNode, MultibandControls) {
    let mut s = Session::builder()
        .build_offline(SampleRate(RATE), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds");
    let g = s.graph();
    let (low, _) = g.insert(Osc::sine(Hz(LOW as f32)).with_amplitude(Amplitude(A as f32)));
    let (high, _) = g.insert(Osc::sine(Hz(HIGH as f32)).with_amplitude(Amplitude(A as f32)));
    let (mb, controls) = g.add(MultibandCompressorNode::new(ChannelLayout::STEREO, 2).unwrap());
    g.set_source(mb, 0, GraphSource::Node(low, 0));
    g.set_source(mb, 1, GraphSource::Node(high, 0));
    g.set_output_source(0, GraphSource::Node(mb, 0));
    g.set_output_source(1, GraphSource::Node(mb, 1));
    // Unity ratio on both bands: nothing is reduced, the make-ups are the gains.
    for band in 0..2 {
        assert!(
            controls
                .params()
                .set(UnitAddr::new(UnitParam::Ratio, band), 1.0)
        );
    }
    s.update();
    (s, mb, controls)
}

/// Renders `frames` frames from a rolling transport, in blocks of 128.
fn render(s: &mut Session, frames: usize) -> [Vec<f32>; 2] {
    s.transport()
        .motion
        .try_send(MotionEvent::Play)
        .expect("room");
    let mut all = [Vec::new(), Vec::new()];
    let mut buf = vec![0.0f32; 128 * 2];
    for _ in 0..frames / 128 {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        for frame in buf.as_chunks::<2>().0 {
            all[0].push(frame[0]);
            all[1].push(frame[1]);
        }
    }
    all
}

/// The amplitude of a tone of `hz` in `x`, by lock-in over whole periods.
fn amplitude(x: &[f32], hz: f64) -> f64 {
    let (mut c, mut s) = (0.0, 0.0);
    for (n, &v) in x.iter().enumerate() {
        let p = TAU * hz * n as f64 / RATE;
        c += f64::from(v) * p.cos();
        s += f64::from(v) * p.sin();
    }
    2.0 * c.hypot(s) / x.len() as f64
}

/// Both channels' steady amplitudes after a settle, over whole periods of
/// both tones (9 600 frames hold 20 of 100 Hz and 1 200 of 6 kHz).
fn measure(s: &mut Session) -> (f64, f64) {
    render(s, 24_064);
    let [l, r] = render(s, 9_600);
    (amplitude(&l, LOW), amplitude(&r, HIGH))
}

/// **A `set_param` at index 1 reaches band 1 and leaves band 0 alone**, and
/// the other way: the right channel (band 1's tone) follows `GainDb` at
/// index 1 by the analytic figure, the left (band 0's) follows index 0.
///
/// Mutations (run), each fails:
/// - `MultibandCompressorNode::param_set` addressing band 1's make-up with
///   band 0's cell → the right channel does not move;
/// - `Graph::set_param` writing the kind's index 0 whatever the address →
///   the left channel moves with the write meant for band 1.
#[test]
fn a_write_to_band_1_moves_band_1_only() {
    let (mut s, mb, _) = rig();
    let (l, r) = measure(&mut s);
    assert!((l - expected(LOW, 0.0, 0.0)).abs() < 1e-5, "{l}");
    assert!((r - expected(HIGH, 0.0, 0.0)).abs() < 1e-5, "{r}");

    s.graph().set_param(mb, MAKEUP1, 6.0);
    let (l, r) = measure(&mut s);
    assert!(
        (l - expected(LOW, 0.0, 6.0)).abs() < 1e-5,
        "band 1 reached the left: {l}"
    );
    assert!((r - expected(HIGH, 0.0, 6.0)).abs() < 1e-5, "{r}");

    s.graph().set_param(mb, MAKEUP0, -9.0);
    let (l, r) = measure(&mut s);
    assert!((l - expected(LOW, -9.0, 6.0)).abs() < 1e-5, "{l}");
    assert!(
        (r - expected(HIGH, -9.0, 6.0)).abs() < 1e-5,
        "band 0 reached the right: {r}"
    );
}

/// The graph declares every band's ports, each modulatable on its own, and
/// no more: 6 per band and one per crossover, none for the toggles.
///
/// Mutation (run): `port_addrs` listing every band at index 0 → the shape
/// refuses the duplicate → fails; dropping the crossover ports → fails.
#[test]
fn the_graph_declares_each_band_and_crossover() {
    let (mut s, mb, _) = rig();
    let g = s.graph();
    for band in 0..2u8 {
        for kind in tutti_nodes::BAND_PARAMS {
            assert!(
                g.declares_param(mb, UnitAddr::new(kind, band)),
                "{kind:?} {band}"
            );
        }
        for kind in [UnitParam::Bypass, UnitParam::Solo] {
            assert!(!g.declares_param(mb, UnitAddr::new(kind, band)), "{kind:?}");
        }
    }
    assert!(g.declares_param(mb, UnitAddr::new(UnitParam::Cutoff, 0)));
    assert!(!g.declares_param(mb, UnitAddr::new(UnitParam::Cutoff, 1)));
    assert!(!g.declares_param(mb, UnitAddr::new(UnitParam::Threshold, 2)));
}

/// A `set_param` at index 1 reaches the band meter of band 1 through the
/// controls the insert returned: a tone over the threshold is reduced by a
/// band-1 threshold written low and not by one written high, band 0 never.
///
/// The right tone (0.4, -8 dB) is over a -30 dB threshold at 2:1 (the reduction
/// is positive) and under a 0 dB one (none, exactly, with the ratio back at
/// 2).
///
/// Mutation (run): `MultibandControls::gain_reduction_db` reading band 0's
/// meter for every band → the band-1 figures swap → fails.
#[test]
fn the_meter_of_band_1_follows_its_threshold() {
    let (mut s, mb, controls) = rig();
    let at = |kind, band| UnitAddr::new(kind, band);
    for band in 0..2 {
        s.graph().set_param(mb, at(UnitParam::Ratio, band), 2.0);
        s.graph().set_param(mb, at(UnitParam::Release, band), 0.001);
        s.graph().set_param(mb, at(UnitParam::Attack, band), 0.001);
    }
    s.graph().set_param(mb, at(UnitParam::Threshold, 0), 0.0);
    s.graph().set_param(mb, at(UnitParam::Threshold, 1), -30.0);
    render(&mut s, 9_600);
    assert!(
        controls.gain_reduction_db(1) > Db(3.0),
        "{:?}",
        controls.gain_reduction_db(1)
    );
    assert!(
        controls.gain_reduction_db(0) < Db(1e-2),
        "{:?}",
        controls.gain_reduction_db(0)
    );
    s.graph().set_param(mb, at(UnitParam::Threshold, 1), 0.0);
    render(&mut s, 9_600);
    assert!(
        controls.gain_reduction_db(1) < Db(1e-4),
        "{:?}",
        controls.gain_reduction_db(1)
    );
}

/// **A route to `GainDb` at index 1 moves band 1's tone and leaves band 0's
/// alone.** An LFO on a `-24..=24` dB target (the cell is 0 dB) at depth 0.25
/// swings band 1's make-up by `12 * sin` dB at 4 Hz past the connect's
/// declick: the right channel's amplitude, measured over each 8-frame period
/// of the 6 kHz tone, is `A * (lp + hp * g1(t))`, while the left stays at its
/// steady figure.
///
/// The lane steps every 32 frames, so `g1` is within 0.2 dB (2.4%) of the
/// sine; the tolerance is 4%, and the swing (a 24 dB span) is asserted
/// whole.
///
/// Mutations (run), each fails:
/// - the route resolved to band 0's make-up (index dropped) → the left
///   swings and the right does not;
/// - the lane's events addressed to index 0 → the same.
#[cfg(feature = "modulation")]
#[test]
fn a_route_to_band_1_modulates_band_1_only() {
    let (mut s, mb, _) = rig();
    let band1 = ParamRef::unit_at(mb, UnitParam::GainDb, 1);
    let mut m = s.mods();
    m.range(band1, -24.0, 24.0);
    let lfo = m.lfo(Lfo::sine().rate(BeatDuration(0.5)));
    m.route(lfo, band1, Depth(0.25));
    for _ in 0..4 {
        s.update();
    }
    assert_eq!(s.mod_state().refusal(), None, "the declarations apply");
    assert_eq!(s.mod_state().unresolved_targets(), &[], "and resolve");

    // 120 BPM: one cycle per half beat is 4 Hz. Past the declick and the
    // make-up ramp's first blocks.
    let [l, r] = render(&mut s, 24_000);
    let skip = 1_024;
    let (lp0, hp0) = shares(LOW);
    let steady = A * lp0;
    for w in l[skip..].chunks(480).filter(|w| w.len() == 480) {
        let a = amplitude(w, LOW);
        // The leak of band 1 (1e-4) swings by up to 4x; band 0 itself is flat.
        assert!(
            (a - steady).abs() < 0.01 * steady + A * hp0 * 4.0,
            "left {a} vs {steady}"
        );
    }
    let (lp1, hp1) = shares(HIGH);
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for (k, w) in r.as_chunks::<8>().0.iter().enumerate().skip(skip / 8) {
        let rms2 = w.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / 8.0;
        let a = (2.0 * rms2).sqrt();
        let t = (k * 8 + 4) as f64 / RATE;
        let db = 12.0 * (TAU * 4.0 * t).sin();
        let want = A * (lp1 + hp1 * gain(db));
        assert!(
            (a - want).abs() < 0.04 * want,
            "right, window {k}: {a} vs {want}"
        );
        let measured_db = 20.0 * ((a / A - lp1) / hp1).log10();
        lo = lo.min(measured_db);
        hi = hi.max(measured_db);
    }
    assert!(
        lo < -9.0 && hi > 9.0,
        "band 1 swings about 0 dB: {lo}..{hi}"
    );
}

/// **A fork takes each index's value as it stands and shares no cell with
/// the live graph at any index**: after the fork, the live make-ups are
/// moved the other way (each band), and each render keeps its own figures.
///
/// Mutation (run): drop `b.core.detach()` in `MultibandCompressorNode::
/// fork_fresh` → the fork follows the live writes → fails.
#[test]
fn a_fork_keeps_each_index_to_itself() {
    let (mut s, mb, _) = rig();
    s.graph().set_param(mb, MAKEUP0, -6.0);
    s.graph().set_param(mb, MAKEUP1, 6.0);
    render(&mut s, 128);
    let mut fork = s.graph_ref().fork_live().expect("the node forks");
    s.graph().set_param(mb, MAKEUP0, 12.0);
    s.graph().set_param(mb, MAKEUP1, -12.0);

    let mut out = [Vec::new(), Vec::new()];
    fork.render(24_064 + 9_600, 128, &mut out);
    let [l, r] = out;
    let (l, r) = (amplitude(&l[24_064..], LOW), amplitude(&r[24_064..], HIGH));
    assert!((l - expected(LOW, -6.0, 6.0)).abs() < 1e-5, "fork left {l}");
    assert!(
        (r - expected(HIGH, -6.0, 6.0)).abs() < 1e-5,
        "fork right {r}"
    );

    let (l, r) = measure(&mut s);
    assert!(
        (l - expected(LOW, 12.0, -12.0)).abs() < 1e-5,
        "live left {l}"
    );
    assert!(
        (r - expected(HIGH, 12.0, -12.0)).abs() < 1e-5,
        "live right {r}"
    );
}

/// A crossover write through the session moves the split: the left tone is
/// band 0's by `1 / (1 + x^4)` at the new split, so dropping the split from
/// 1 kHz to 150 Hz takes the 100 Hz tone through the low-pass at
/// `x = warp(100) / warp(150)` and the make-up of band 1 reaches it.
///
/// Mutation (run): `Graph::set_param` for `Cutoff` at index 0 writing a cell
/// the node does not read → the left tone stays band 0's → fails.
#[test]
fn a_crossover_write_moves_the_split() {
    let (mut s, mb, _) = rig();
    s.graph().set_param(mb, MAKEUP1, -60.0);
    let (l, _) = measure(&mut s);
    // Band 1 muted by 60 dB: the left is band 0's share alone.
    let (lp, hp) = shares(LOW);
    assert!((l - A * (lp + hp * gain(-60.0))).abs() < 1e-5, "{l}");

    s.graph()
        .set_param(mb, UnitAddr::new(UnitParam::Cutoff, 0), 150.0);
    let (l, _) = measure(&mut s);
    let x4 = ((PI * LOW / RATE).tan() / (PI * 150.0 / RATE).tan()).powi(4);
    let want = A * (1.0 / (1.0 + x4) + x4 / (1.0 + x4) * gain(-60.0));
    assert!((l - want).abs() < 1e-5, "{l} vs {want}");
    assert!(l < A * lp * 0.9, "the split moved: {l}");
}
