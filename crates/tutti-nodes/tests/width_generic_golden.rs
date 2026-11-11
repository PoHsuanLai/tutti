//! The width-generic SVF, ladder, delay, modulated delay and phaser: pinned
//! renders.
//!
//! The held cases pin renders checked bit-identical against separate
//! mono and stereo reference implementations. The `*_swept`, `*_automated`,
//! ring-routed and phaser cases pin behaviour those references did not have
//! (the phaser interpolates its coefficients every 16 samples, and the
//! chorus and flanger do the same to the LFO's sine; control changes ramped
//! across a block; the cross-feedback matrix).
//!
//! The nodes that blend nothing themselves (the delay, the chorus and flanger,
//! the phaser) are rendered through a graph at the mix their case names, which
//! is the graph's blend of the node's output with its input (`Mix`): the pins
//! were captured when those nodes blended internally, and the blend is the same
//! two products and sum (the ramp across a block that moves is `Piece`'s, a
//! last-ulp different arithmetic from the old node's, well inside the
//! tolerance).
//!
//! Captured with `cargo nextest run -p tutti-nodes -E 'test(print_goldens)'
//! --run-ignored only --no-capture`.
//!
//! The tolerance is 1e-5 absolute rather than bit equality only so a libm
//! `tan`/`sin`/`tanh` differing in the last ulp on another C runtime does not
//! fail a render nothing touched (see CLAUDE.md, "One live platform difference
//! remains"); the recursive filters can carry such an ulp for a while, which is
//! why this is looser than a feed-forward node would need.

use std::sync::atomic::Ordering;

use tutti_graph::contract::drive;
use tutti_graph::{GraphBuilder, Node, ParamNode, Prepare, Unforkable};
use tutti_nodes::{
    DelayLineNode, InterpolationMode, LadderFilterNode, LadderType, ModDelayNode, PhaserNode,
    SvfControls, SvfFilterNode, SvfType,
};
use tutti_types::{
    ChannelLayout, Db, Depth, Feedback, Hz, Mix, ParamCell, PhaseIncrement, Q, Resonance,
    SampleRate, Samples, Seconds, UnitParam,
};

const SR: SampleRate = SampleRate(48_000.0);
const LEN: usize = 2_048;
/// Ragged on purpose: full blocks, odd sizes, and a block of one.
const PATTERN: [usize; 6] = [64, 64, 17, 1, 64, 33];
/// Pin every `STRIDE`th frame of the first and last channel.
const STRIDE: usize = 97;
const TOL: f32 = 1e-5;

/// Deterministic broadband input in `[-1, 1)`.
fn noise(seed: u32) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..LEN)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        })
        .collect()
}

fn noise_channels(n: usize) -> Vec<Vec<f32>> {
    (0..n).map(|c| noise(c as u32 + 1)).collect()
}

/// An exponential sweep `from → to` over the render.
fn sweep(from: f32, to: f32) -> Vec<f32> {
    (0..LEN)
        .map(|i| from * (to / from).powf(i as f32 / LEN as f32))
        .collect()
}

/// Writes control values between blocks; called with the block index before
/// each `process`.
type Automation = Box<dyn Fn(usize)>;

struct Case {
    name: &'static str,
    /// The node, driven as a graph drives it (`tutti_graph::contract::drive`),
    /// its params on its param ports.
    node: Box<dyn Node>,
    inputs: Vec<Vec<f32>>,
    /// Per param of the node's feed, its values over the render when fed —
    /// what the graph's modulation hands the node's param ports. Empty:
    /// nothing fed.
    /// (These were extra input channels, `with_param_inputs`' ports, when the
    /// goldens were captured; the values and the DSP that reads them are
    /// unchanged, so the pins are too.)
    params: Vec<Option<Vec<f32>>>,
    automation: Option<Automation>,
    /// The graph's mix of the node when it is not wet only, and (for a mix that
    /// moves) its value before each block.
    mix: Option<Mix>,
    mix_at: Option<Box<dyn Fn(usize) -> Mix>>,
}

fn node_case(name: &'static str, node: impl Node, inputs: Vec<Vec<f32>>) -> Case {
    Case {
        name,
        node: Box::new(node),
        inputs,
        params: Vec::new(),
        automation: None,
        mix: None,
        mix_at: None,
    }
}

/// `node_case` blended with its input at `mix` by the graph.
fn mixed_case(name: &'static str, node: impl Node, inputs: Vec<Vec<f32>>, mix: f32) -> Case {
    Case {
        mix: Some(Mix::new(mix)),
        ..node_case(name, node, inputs)
    }
}

/// Automate `cell` block by block with `f(block)`.
fn automate(cell: impl Into<ParamCell>, f: impl Fn(usize) -> f32 + 'static) -> Option<Automation> {
    let cell = cell.into();
    Some(Box::new(move |k| cell.store(f(k), Ordering::Release)))
}

/// [`render`] for a graph node: the same blocks, through `drive`.
fn render_node(
    node: &mut dyn Node,
    inputs: &[Vec<f32>],
    params: &[Option<Vec<f32>>],
    automation: Option<&Automation>,
) -> Vec<Vec<f32>> {
    node.prepare(&Prepare::new(SR, Samples(64)));
    let mut out: Vec<Vec<f32>> = Vec::new();
    let (mut pos, mut k) = (0, 0);
    while pos < LEN {
        let n = PATTERN[k % PATTERN.len()].min(LEN - pos);
        if let Some(a) = automation {
            a(k);
        }
        k += 1;
        let ins: Vec<&[f32]> = inputs.iter().map(|c| &c[pos..pos + n]).collect();
        let params: Vec<Option<&[f32]>> = params
            .iter()
            .map(|p| p.as_ref().map(|v| &v[pos..pos + n]))
            .collect();
        let block = drive(node, SR, &ins, &params);
        out.resize(block.len(), Vec::with_capacity(LEN));
        for (o, b) in out.iter_mut().zip(block) {
            o.extend(b);
        }
        pos += n;
    }
    out
}

// One push per case, each beside the setup it needs, reads as the table it is;
// a `vec![]` literal would force the setup out of line.
#[allow(clippy::vec_init_then_push)]
fn cases() -> Vec<Case> {
    let mut v = Vec::new();

    // ── SVF ──
    v.push(node_case(
        "svf_mono_lowpass",
        SvfFilterNode::<f64>::new(SvfType::LowPass, Hz::new(1_200.0), Q::new(0.9)),
        noise_channels(1),
    ));
    v.push(node_case(
        "svf_mono_bell",
        SvfFilterNode::<f64>::with_controls(
            ChannelLayout::MONO,
            SvfControls {
                ty: SvfType::Bell,
                freq: Hz::new(900.0),
                q: Q::new(1.4),
                gain: Db::new(6.0),
            },
        ),
        noise_channels(1),
    ));
    v.push(node_case(
        "svf_stereo_f32_bandpass",
        SvfFilterNode::<f32>::with_channels(
            ChannelLayout::STEREO,
            SvfType::BandPass,
            Hz::new(300.0),
            Q::new(2.0),
        ),
        noise_channels(2),
    ));
    v.push(node_case(
        "svf_wide6_highshelf",
        SvfFilterNode::<f64>::with_controls(
            6u16,
            SvfControls {
                ty: SvfType::HighShelf,
                freq: Hz::new(2_000.0),
                q: Q::new(0.7),
                gain: Db::new(-4.0),
            },
        ),
        noise_channels(6),
    ));
    v.push(Case {
        params: vec![Some(sweep(200.0, 8_000.0)), None],
        ..node_case(
            "svf_stereo_swept",
            SvfFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                SvfType::LowPass,
                Hz::new(1_000.0),
                Q::new(0.707),
            ),
            noise_channels(2),
        )
    });
    let node = SvfFilterNode::<f64>::with_channels(
        ChannelLayout::STEREO,
        SvfType::LowPass,
        Hz::new(500.0),
        Q::new(0.8),
    );
    let automation = automate(node.param_set().cell(UnitParam::Cutoff).unwrap(), |k| {
        300.0 + 700.0 * (k % 5) as f32
    });
    v.push(Case {
        automation,
        ..node_case("svf_stereo_automated", node, noise_channels(2))
    });

    // ── Ladder ──
    let node =
        LadderFilterNode::<f64>::new(LadderType::LP24, Hz::new(1_100.0), Resonance::new(0.7));
    assert!(node.param_set().set(UnitParam::Drive, 2.5));
    v.push(node_case(
        "ladder_mono_lp24_driven",
        node,
        noise_channels(1),
    ));
    v.push(node_case(
        "ladder_stereo_f32_hp12",
        LadderFilterNode::<f32>::with_channels(
            ChannelLayout::STEREO,
            LadderType::HP12,
            Hz::new(700.0),
            Resonance::new(0.5),
        ),
        noise_channels(2),
    ));
    v.push(node_case(
        "ladder_wide6_lp12",
        LadderFilterNode::<f64>::with_channels(
            6u16,
            LadderType::LP12,
            Hz::new(1_500.0),
            Resonance::new(0.3),
        ),
        noise_channels(6),
    ));
    v.push(Case {
        params: vec![Some(sweep(200.0, 8_000.0)), None, None],
        ..node_case(
            "ladder_stereo_swept",
            LadderFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                LadderType::LP24,
                Hz::new(1_000.0),
                Resonance::new(0.6),
            ),
            noise_channels(2),
        )
    });
    let node = LadderFilterNode::<f64>::with_channels(
        ChannelLayout::STEREO,
        LadderType::LP24,
        Hz::new(800.0),
        Resonance::new(0.5),
    );
    let automation = automate(node.param_set().cell(UnitParam::Drive).unwrap(), |k| {
        1.0 + (k % 4) as f32
    });
    v.push(Case {
        automation,
        ..node_case("ladder_stereo_drive_automated", node, noise_channels(2))
    });

    // ── Delay ──
    let node = DelayLineNode::new(
        Seconds::new(0.5),
        Seconds::new(0.012_34),
        Feedback::new(0.6),
    )
    .with_interpolation(InterpolationMode::CubicHermite);
    v.push(mixed_case("delay_mono_cubic", node, noise_channels(1), 0.4));
    let node = DelayLineNode::stereo(
        Seconds::new(0.5),
        Seconds::new(0.010),
        Seconds::new(0.017),
        Feedback::new(0.5),
    );
    node.set_cross_feedback(Feedback::new(0.3));
    v.push(mixed_case(
        "delay_stereo_cross_fed",
        node,
        noise_channels(2),
        0.7,
    ));
    let node = DelayLineNode::with_channels(
        6u16,
        Seconds::new(0.5),
        Seconds::new(0.011),
        Feedback::new(0.45),
    );
    v.push(mixed_case("delay_wide6", node, noise_channels(6), 0.6));
    let feedback = (0..LEN).map(|i| 0.9 * (i as f32 / LEN as f32)).collect();
    let time = (0..LEN)
        .map(|i| 0.002 + 0.01 * (i as f32 / LEN as f32))
        .collect();
    let node = DelayLineNode::with_channels(
        ChannelLayout::STEREO,
        Seconds::new(0.5),
        Seconds::new(0.01),
        Feedback::new(0.4),
    );
    node.set_cross_feedback(Feedback::new(0.2));
    v.push(Case {
        params: vec![Some(feedback), Some(time)],
        ..node_case("delay_stereo_ports", node, noise_channels(2))
    });
    let n = 6usize;
    let mut ring = vec![0.0f32; n * n];
    for c in 0..n {
        ring[c * n + (c + n - 1) % n] = 1.0;
    }
    let node = DelayLineNode::with_channels(
        ChannelLayout::try_from(n).expect("a test width is small"),
        Seconds::new(0.1),
        Seconds::new(0.004),
        Feedback::new(0.3),
    )
    .with_cross_feedback_matrix(&ring);
    node.set_cross_feedback(Feedback::new(0.5));
    v.push(node_case("delay_wide6_ring", node, noise_channels(6)));
    let node = DelayLineNode::stereo(
        Seconds::new(0.5),
        Seconds::new(0.01),
        Seconds::new(0.013),
        Feedback::new(0.3),
    );
    let automation = automate(node.feedback(), |k| 0.1 * (k % 8) as f32);
    let times = node.delay_time();
    let automation = {
        let fb = automation.expect("feedback automation");
        Some(Box::new(move |k: usize| {
            fb(k);
            times.store((0.005 + 0.001 * (k % 6) as f32) as f64, Ordering::Release);
        }) as Automation)
    };
    v.push(Case {
        automation,
        ..node_case("delay_stereo_automated", node, noise_channels(2))
    });

    // ── Chorus / flanger ──
    let node = ModDelayNode::chorus(ChannelLayout::STEREO);
    node.set_rate(Hz::new(1.7));
    node.set_depth(Seconds::new(0.007));
    node.set_feedback(Feedback::new(0.45));
    v.push(mixed_case("chorus_stereo", node, noise_channels(2), 0.6));
    v.push(mixed_case(
        "flanger_stereo",
        ModDelayNode::flanger(ChannelLayout::STEREO),
        noise_channels(2),
        0.5,
    ));
    v.push(Case {
        mix_at: Some(Box::new(|k| Mix::new((k % 3) as f32 * 0.5))),
        ..mixed_case(
            "chorus_wide6_mix_automated",
            ModDelayNode::chorus(6u16),
            noise_channels(6),
            0.0,
        )
    });

    // ── Phaser ──
    let node = PhaserNode::new(6);
    node.set_rate(Hz::new(0.3));
    node.set_depth(Depth::new(0.8));
    node.set_feedback(Feedback::new(0.6));
    v.push(mixed_case("phaser_mono", node, noise_channels(1), 0.5));
    let node = PhaserNode::with_channels(ChannelLayout::STEREO, 4);
    node.set_rate(Hz::new(2.0));
    v.push(mixed_case("phaser_stereo", node, noise_channels(2), 0.5));
    let node = PhaserNode::with_channels(6u16, 8).with_phase_offsets(&[
        PhaseIncrement(0.0),
        PhaseIncrement(0.1),
        PhaseIncrement(0.2),
        PhaseIncrement(0.3),
        PhaseIncrement(0.4),
        PhaseIncrement(0.5),
    ]);
    node.set_rate(Hz::new(3.0));
    v.push(mixed_case(
        "phaser_wide6_staggered",
        node,
        noise_channels(6),
        0.5,
    ));

    v
}

fn render(case: &mut Case) -> Vec<Vec<f32>> {
    if let Some(mix) = case.mix {
        return render_mixed(case, mix);
    }
    render_node(
        case.node.as_mut(),
        &case.inputs,
        &case.params,
        case.automation.as_ref(),
    )
}

/// The case's node alone in a graph (as `Solo` places it) with its mix set
/// before the unit is placed (so the first block does not ramp in from wet), the
/// same ragged blocks, and the mix moved before each block when `mix_at` says.
fn render_mixed(case: &mut Case, mix: Mix) -> Vec<Vec<f32>> {
    let node = std::mem::replace(&mut case.node, Box::new(Silent));
    let shape = node.shape();
    let mut g = GraphBuilder::new(shape.audio_in, shape.audio_out);
    let key = g.add(Unforkable(node));
    for c in 0..usize::from(shape.audio_in.count()) {
        g.connect_input(c, key, c);
    }
    for c in 0..usize::from(shape.audio_out.count()) {
        g.connect_output(key, c, c);
    }
    g.spec_mut().set_mix(key, mix);
    let mut r = g
        .renderer(Prepare::new(SR, Samples(64)))
        .expect("a one-node graph builds");
    let mut out: Vec<Vec<f32>> = Vec::new();
    let (mut pos, mut k) = (0, 0);
    while pos < LEN {
        let n = PATTERN[k % PATTERN.len()].min(LEN - pos);
        if let Some(at) = &case.mix_at
            && k > 0
        {
            r.editor_mut()
                .set_mix(key, at(k))
                .expect("a mix in range, on a node in the graph");
        }
        k += 1;
        let ins: Vec<&[f32]> = case.inputs.iter().map(|c| &c[pos..pos + n]).collect();
        let block = r.render_input(&ins);
        out.resize(block.len(), Vec::with_capacity(LEN));
        for (o, b) in out.iter_mut().zip(block) {
            o.extend(b);
        }
        pos += n;
    }
    out
}

/// What a case's node is replaced with once the graph owns it.
#[derive(Debug)]
struct Silent;

impl Node for Silent {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &tutti_graph::Cx<'_>, _: tutti_graph::Io<'_>) -> tutti_graph::Status {
        tutti_graph::Status::Idle
    }
    fn reset(&mut self) {}
}

/// The pinned frames of the first and last channel, in that order.
fn pins(out: &[Vec<f32>]) -> Vec<f32> {
    let last = out.len() - 1;
    let mut v: Vec<f32> = out[0].iter().step_by(STRIDE).copied().collect();
    if last > 0 {
        v.extend(out[last].iter().step_by(STRIDE));
    }
    v
}

#[test]
#[ignore = "generator: prints the golden tables from the current implementation"]
fn print_goldens() {
    for mut c in cases() {
        let got = pins(&render(&mut c));
        println!("(\"{}\", &{:?}),", c.name, got);
    }
}

/// Every case, against its pinned frames.
///
/// Mutation (run): the graph mix of `delay_mono_cubic` set to 0.41 instead of
/// 0.4 fails its first pin.
///
/// Mutation (each run, each fails): updating the SVF's second integrator from
/// `v1`; taking the ladder's LP12 output from the fourth stage; reading the
/// delay's feedback tap at `d` instead of `d - 1`; dropping the chorus's
/// per-channel stagger; `COEFF_INTERVAL = 64`; a ramp that jumps straight to
/// its target.
#[test]
fn width_generic_nodes_match_their_goldens() {
    let goldens: std::collections::HashMap<&str, &[f32]> = GOLDENS.iter().copied().collect();
    let mut checked = 0;
    for mut c in cases() {
        let want = goldens
            .get(c.name)
            .unwrap_or_else(|| panic!("no golden for {}", c.name));
        let got = pins(&render(&mut c));
        assert_eq!(got.len(), want.len(), "{}: pinned frame count", c.name);
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            assert!(
                (g - w).abs() <= TOL,
                "{}: pin {i}: got {g}, golden {w}",
                c.name
            );
        }
        checked += 1;
    }
    assert_eq!(checked, GOLDENS.len(), "every golden has a case");
}

#[rustfmt::skip]
const GOLDENS: &[(&str, &[f32])] = &[
    ("svf_mono_lowpass", &[-0.0055749584, 0.12923843, 0.014046079, -0.025821345, 0.11423153, -0.083170906, -0.25527436, -0.0018581287, 0.091299236, 0.11037293, 0.10966332, 0.016562795, -0.0036786029, 0.2861794, -0.0826972, -0.05357377, 0.23704131, 0.11560212, -0.02621566, -0.038313564, -0.16046825, 0.18713716]),
    ("svf_mono_bell", &[-1.01262, 0.79311943, -0.57552505, -0.036748283, -0.100330755, 0.63603824, 0.3516172, -0.31967086, 0.8814049, 1.1236017, -0.06639492, -0.45402554, -0.26371077, 0.9042752, -0.9252751, -0.5124038, -0.775392, -0.17428932, 0.39397696, -0.7613463, -0.57232034, -0.6565546]),
    ("svf_stereo_f32_bandpass", &[-0.019134818, 0.11532706, -0.11734694, 0.022560598, 0.104758315, 0.10882972, 0.02482237, 0.035443697, 0.025173923, 0.018440552, 0.19615355, 0.04424132, -0.05887313, 0.18730839, -0.11333053, -0.06098309, 0.019959308, -0.028842572, -0.05436393, 0.053399302, -0.021355785, 0.07886086, -0.007014321, 0.05028124, -0.051508192, -0.17275739, -0.15014313, 0.029949065, -0.011406783, -0.09176129, 0.027455578, -0.21556571, -0.086526096, -0.018930927, 0.12067772, -0.24278289, 0.21773621, -0.17539528, -0.0015360313, -0.109832, 0.004725826, -0.031341523, 0.1320488, -0.010230849]),
    ("svf_wide6_highshelf", &[-0.64813095, 0.5549511, -0.44781056, -0.030484136, -0.03248178, 0.44886354, 0.28169686, -0.14684904, 0.566665, 0.7061787, 0.030008731, -0.2471723, -0.15286474, 0.6510076, -0.6318232, -0.34622777, -0.43079013, -0.1268124, 0.2332925, -0.4584429, -0.325481, -0.37289885, 0.08770853, 0.44731635, 0.28616968, 0.4659293, 0.64592, -0.020044822, -0.23294467, 0.4029086, 0.15674096, -0.12526676, 0.48811337, 0.13132007, 0.15792917, -0.34734926, -0.2332969, 0.48635888, -0.59049284, 0.06093507, 0.13373409, -0.3258684, -0.026755549, -0.58356655]),
    ("svf_stereo_swept", &[-0.00016558991, 0.02236907, 0.061902076, -0.026562087, 0.10390468, -0.06250358, 0.009567581, 0.10996979, 0.06714019, 0.035122264, 0.12117559, 0.008448635, 0.013168073, 0.24909842, -0.088557474, 0.070207365, -0.0121132955, -0.027379435, 0.037483443, 0.08340733, -0.25258043, -0.5435979, -6.070091e-5, 0.05196739, 0.05222616, -0.015455137, -0.024738938, -0.02281489, -0.040305562, 0.105254374, 0.109823875, -0.26332632, -0.26190445, 0.02828856, 0.09792021, -0.18339798, 0.18604887, -0.19301133, 0.058370862, -0.3552767, 0.104048625, -0.5498669, 0.26749465, 0.4197018]),
    ("svf_stereo_automated", &[-0.00037035995, 0.06252005, -0.15231699, -0.052996572, 0.23730218, -0.02431541, -0.14540914, -0.15412241, -0.0028253817, 0.21666119, 0.16455309, -0.09171576, 0.03773959, 0.2606947, -0.08419377, 0.075211145, 0.22304605, 0.1808307, -0.014498299, -0.075824834, -0.14662774, -0.042369127, -0.00013576422, 0.07653656, 0.16842861, -0.06841766, -0.28827554, 0.059663072, 0.05625909, -0.12207149, -0.0691856, -0.17316501, -0.17985442, -0.018909974, 0.11953801, -0.18481848, 0.1690132, -0.24244718, -0.08974239, -0.3851611, 0.05590026, -0.18347086, 0.22608674, 0.2671832]),
    ("ladder_mono_lp24_driven", &[-2.0179412e-5, 0.100083895, -0.09854974, -0.1159132, -0.049213927, 0.018638661, -0.009060835, 0.15012085, 0.05486078, -0.067531504, 0.08922538, 0.1094787, 0.012209649, 0.15876815, -0.058802735, -0.048392367, 0.17473558, -0.004299534, -0.06940406, 0.011137059, 0.081427634, 0.14184368]),
    ("ladder_stereo_f32_hp12", &[-0.690203, 0.47910565, -0.51979107, -0.12727581, -0.2644784, 0.46722883, 0.33551145, -0.07913022, 0.7200858, 0.4510689, -0.22492206, -0.41422105, -0.21815209, 0.37073106, -0.58786964, -0.46590775, -0.6869893, -0.2235397, 0.3171668, -0.63264006, -0.21108285, -0.48710978, -0.31628358, 0.6045121, -0.07471877, 0.78676724, 0.35075226, 0.58232695, -0.74706566, -0.34604245, 0.054983165, 0.57223296, -0.42297852, -0.21067664, 0.3420143, 0.5929157, 0.19303364, 0.56360394, -0.19031478, 0.20449221, 0.21840996, -0.029690742, -0.20075457, 0.046593554]),
    ("ladder_wide6_lp12", &[-0.00606899, 0.05634594, 0.02745128, 0.04599959, 0.0994053, -0.040716633, -0.1526114, -0.09967061, 0.015973408, 0.14774323, 0.053282265, -0.036404334, 0.015576623, 0.14496636, -0.04112079, 0.022157159, 0.09159956, 0.098896235, 0.032722678, -0.052787688, -0.16119626, 0.073609464, 0.0010645648, 0.09411904, -0.07064848, 0.06390043, 0.18078318, 0.027059404, 0.04648643, 0.06987895, -0.000731159, 0.026027085, -0.1126548, -0.04284995, 0.05787807, 0.11421903, 0.018580182, -0.064215325, 0.11317414, 0.01909905, 0.18314043, -0.040072434, 0.030136323, 0.09538782]),
    ("ladder_stereo_swept", &[-2.104619e-8, -0.0029920912, 0.03906977, -0.024086986, 0.0386402, -0.06114789, 0.028644845, 0.038387977, -0.0008655457, -0.05707053, 0.039528124, 0.0445821, -0.02764728, 0.09697997, -0.07641802, -0.020018412, -0.00096891937, 0.11035802, -0.1429796, -0.15657818, 0.014861722, -0.19050188, -9.644359e-9, 0.007834277, 0.028369863, 0.017358521, 0.02167516, 0.0016228475, -0.03888741, 0.018873569, -0.0020914727, -0.097882934, -0.07894985, -0.01161632, 0.06821678, -0.03525777, 0.045063652, -0.124592565, 0.055714305, -0.12416383, 0.05868701, -0.1638069, 0.2240841, 0.0071434784]),
    ("ladder_stereo_drive_automated", &[-4.6425857e-6, 0.038767397, 0.02824506, -0.06409211, 0.110394076, -0.020661877, 0.1016407, 0.059986312, -0.038001183, 0.014598745, 0.08371767, 0.060975064, -0.09257489, 0.081430435, -0.06570772, -0.15653542, 0.11522407, -0.012719619, -0.08738649, -0.067198925, 0.0661214, -0.0071789655, -2.127452e-6, 0.08973086, 0.035773955, -0.07692991, -0.05638536, -0.028649263, 0.007531957, 0.11238904, 0.019382467, -0.23366427, -0.08416329, 0.0164322, -0.07146005, -0.07543534, -0.06093728, -0.036594022, 0.117221326, -0.04506661, -0.014751117, 0.014663172, 0.08743575, -0.005499555]),
    ("delay_mono_cubic", &[-0.5906077, 0.46551913, -0.40322366, -0.06945842, -0.102913864, 0.39537108, 0.24493125, -0.38860905, 0.51814145, 0.28469166, -0.121706754, 0.07076211, -0.2888013, 0.53743225, -0.678148, -0.4864468, -0.21962026, -0.13651243, 0.22279195, -0.005669415, -0.5924424, -0.6400535]),
    ("delay_stereo_cross_fed", &[-0.29530385, 0.23275957, -0.20161183, -0.03472921, -0.051456932, -0.054749966, -0.1317821, 0.20453398, -0.24021778, 0.18635175, -0.5241971, -0.046079382, 0.03347417, -0.33164853, -0.21952854, -0.7818478, -0.30711642, -0.07471803, 0.9419783, 0.050305843, 0.4469054, -0.06096995, -0.10825063, 0.2248193, -0.012494982, 0.2537414, 0.016921878, 0.27224883, -0.28439242, -0.116011836, 0.0016071439, -0.12755562, 0.045469046, -0.7234341, 0.74952376, -0.52449846, -0.71518046, 0.24835399, 0.70498145, 0.38631904, 0.7192677, 0.4470889, 0.6435833, -0.7979806]),
    ("delay_wide6", &[-0.39373845, 0.31034607, -0.26881573, -0.046305604, -0.06860923, 0.26358068, 0.4626307, 0.25507838, 0.090592116, 0.5930089, 0.14661828, -0.6798482, -0.11818276, -0.08466637, -0.3071038, -0.772961, 0.5223354, -0.50025976, -0.21975197, -0.07008763, 0.37599307, -0.44076127, 0.053282782, 0.25741094, 0.19196299, 0.2768318, 0.3872496, -0.039330576, 0.22841968, 0.60865396, -0.39452612, 0.19165376, 0.13541426, 0.060054373, -0.14876334, 0.041655213, 0.14157876, 0.6579304, -0.09727675, -0.86815995, -0.32513162, 0.26421654, 0.2521767, -0.110800505]),
    ("delay_stereo_ports", &[0.0, 0.0, -0.057941765, -0.18363556, 0.6873832, 0.2665431, 0.75175357, 0.07506809, 0.078468055, 0.13862559, -0.36369023, 0.65552455, -0.64108557, 0.26990336, -0.6770028, 0.49753568, -0.13420725, 0.79287636, -0.68734205, 0.15477514, 0.7163779, -0.10325426, 0.0, 0.0, -0.7975454, 0.3070774, 0.12343031, -0.23180485, 0.21826693, -0.5281636, 0.10300796, 0.015124828, -0.11815944, 0.45168746, -0.56550276, 0.02702251, -0.8583219, -0.19538368, 0.15064603, -0.12742636, -0.0026099645, -0.67666054, 0.02289322, -0.65968674]),
    ("delay_wide6_ring", &[0.0, 0.0, 0.67123896, 0.4526207, 0.93167233, -0.16448085, -0.7925237, 0.5325172, 0.8804439, 0.35766163, 1.5407981, -0.2997759, -0.7334603, -0.9944951, 0.929167, 1.3316729, 0.6390133, -0.043078505, 0.7337201, -0.8159842, -0.93990606, -0.11810403, 0.0, 0.0, -0.034278207, 0.67135245, -0.9025614, -0.35830835, -0.97840494, 1.3685048, 0.5557031, 0.6384473, 1.1626849, 0.03449375, -0.18275714, 0.009866723, 1.1915628, 0.052337658, -0.1392111, -0.17076084, 1.1717721, -0.18251248, -0.09306131, -2.092069]),
    ("delay_stereo_automated", &[0.0, 0.0, 0.0, 0.0, 0.5499736, -0.36062217, -0.9604371, -0.04295793, -0.95751333, -0.64748836, -0.4890436, -0.6326039, 0.029167749, 0.25295198, 0.61363846, 0.32942954, -0.17805457, -0.78002214, -0.45946267, 0.67310303, 0.28703308, -0.9399222, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.23299766, -0.31851935, 0.7464317, -0.045637965, -0.11094844, 0.69409835, 1.1177504, -0.10502858, -0.6933273, 0.14903003, -0.86115, -0.9613388, -0.022060752, -0.9073523, 0.55080104]),
    ("chorus_stereo", &[-0.39373845, 0.31034607, -0.26881573, -0.046305604, -0.06860923, 0.26358068, 0.27228802, 0.4476736, 0.2306612, 0.100932896, 0.19507238, -0.70998657, 0.018534571, 0.08242464, -0.33789283, 0.25401127, -0.7558391, 0.07994189, 0.53150624, -0.6317146, -0.54131407, -0.36188877, -0.14433417, 0.29975903, -0.016659973, 0.33832186, 0.022562502, 0.36299843, -0.37918985, -0.15468244, 0.0021428585, 0.049953274, -0.60173905, -0.02549643, 0.11443646, 0.22877191, 0.1663131, 0.2052234, 0.09712601, -0.020290717, -0.43804777, -0.0061207414, 0.449181, -0.10596069]),
    ("flanger_stereo", &[-0.49217308, 0.08582887, -0.19281091, -0.22395611, 0.120209396, 0.4490891, -0.118178636, -0.6463552, 0.729317, 0.7537991, -0.29875514, -0.50596046, 0.0121333, 0.14228436, -0.55487704, -0.66390115, -0.5847412, 0.33767122, 0.051063955, -0.469988, -0.83996564, -0.20535797, -0.18041772, 0.50798786, -0.41089073, 0.16261941, 0.002565071, 1.2971687, -0.042916477, -0.18747243, 0.07620637, -0.35380402, -0.07172787, -0.36126775, 0.8281604, 0.24535316, -0.13214105, -0.2562213, -0.25149482, 0.2223922, -0.047959358, -0.0962182, 0.3035583, 0.060751572]),
    ("chorus_wide6_mix_automated", &[-0.98434615, 0.569776, -0.4147743, -0.08863183, -0.1715231, 0.0, 0.3576153, -0.24017735, 0.79767877, -0.34036595, -0.028823888, -0.18847479, -0.086743504, 0.51203793, 0.27028126, 0.27571014, -0.49290684, -0.16525094, 0.19096778, 0.25260285, 0.3601989, -0.5193692, 0.13320696, 0.47259042, 0.2961929, 0.5298734, 0.96812403, 0.0, -0.27664945, 0.40533936, 0.23784307, 0.56116974, 0.07210098, 0.07297234, 0.05167193, -0.53493285, -0.70215285, -0.0432871, -0.7593955, -0.028164081, -0.11239159, -0.060397044, 0.6426554, -0.67812043]),
    ("phaser_mono", &[-0.61818624, 0.30589634, -0.48952824, 0.12262848, -0.15952098, 0.081432045, 0.24488337, 0.20314533, 0.98322654, -0.02834326, 0.22402485, -0.14154926, 0.08667372, 0.36843282, 0.30412287, -0.32843468, -0.6013888, -0.25968057, 0.3256858, -1.0864637, -0.40468764, -1.1512823]),
    ("phaser_stereo", &[-0.76109064, 0.43745714, -0.9632483, 0.27126628, 0.36589944, 0.29650813, 0.21199954, 0.0058871806, 1.3456728, 0.6407036, 0.018512934, -0.38021326, -0.1007421, 0.15819561, -0.33253416, -0.3435398, -0.5848913, 0.16298911, 0.40266013, -0.86086065, -0.13940072, -0.43349463, -0.27899584, 0.91335315, 0.3392676, 0.826813, 0.2194686, 0.83684075, -0.55366737, -0.663572, 0.2172176, 0.46469882, -0.66760904, -0.016349465, 0.09150413, 0.6225576, 0.6066122, 0.20744781, 0.6855906, -0.41378555, 0.13909373, 0.14242539, 0.40095696, -0.07701981]),
    ("phaser_wide6_staggered", &[-0.63910645, 0.33355564, -0.7448835, 0.007336259, -0.22043723, 0.41424227, 0.24871683, 0.11593133, 0.8849528, -0.14399332, 0.22518803, -0.28316516, 0.3560504, 0.8217744, -0.3033846, -0.050831795, -0.7999824, 0.22121215, 0.46104833, -0.8714086, -0.67066497, -0.6860471, 0.08648729, 0.8102561, 0.4569773, 0.9264583, 0.86194694, 0.61968386, -0.31306946, 0.6397997, 0.414909, 0.13238168, 0.16489184, 0.14946483, -0.39636484, -0.43039966, 0.3766073, 0.5759995, -0.8675233, -0.237162, 0.21539676, -0.49720228, -0.29201812, -1.1978667]),
];
