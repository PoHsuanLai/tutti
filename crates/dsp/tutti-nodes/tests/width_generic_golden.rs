//! The width-generic SVF, ladder, delay, modulated delay and phaser: pinned
//! renders.
//!
//! These values were captured from the merged nodes at the commit (`ee9e4d10`)
//! where `src/legacy/equivalence.rs` rendered them side by side with the mono/stereo
//! twins they replaced (design doc 013, rewrite-order item 3) and found them
//! bit-identical on every held configuration the twins supported — so for the
//! held cases here the pins are the old nodes' output. The `*_swept`,
//! `*_automated`, ring-routed and phaser cases pin the new behaviour where it
//! deliberately differs or is new (coefficient interpolation every 16 samples;
//! control changes ramped across a block; the cross-feedback matrix). That
//! commit's tests documented the old-vs-new tolerance for those.
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
use std::sync::Arc;

use tutti_core::{AtomicF32, AudioUnit, BufferVec, ChannelLayout, PhaseIncrement, SampleRate};
use tutti_nodes::{
    DelayLineNode, InterpolationMode, LadderFilterNode, LadderType, ModDelayNode, PhaserNode,
    SvfFilterNode, SvfType,
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
    node: Box<dyn AudioUnit>,
    inputs: Vec<Vec<f32>>,
    /// Per param of the node's feed, its values over the render when fed —
    /// what the graph's modulation hands a `Legacy` unit. Empty: nothing fed.
    /// (These were extra input channels, `with_param_inputs`' ports, when the
    /// goldens were captured; the values and the DSP that reads them are
    /// unchanged, so the pins are too.)
    params: Vec<Option<Vec<f32>>>,
    automation: Option<Automation>,
}

fn case(name: &'static str, node: impl AudioUnit + 'static, inputs: Vec<Vec<f32>>) -> Case {
    Case {
        name,
        node: Box::new(node),
        inputs,
        params: Vec::new(),
        automation: None,
    }
}

/// Automate `cell` block by block with `f(block)`.
fn automate(cell: Arc<AtomicF32>, f: impl Fn(usize) -> f32 + 'static) -> Option<Automation> {
    Some(Box::new(move |k| cell.store(f(k), Ordering::Release)))
}

// One push per case, each beside the setup it needs, reads as the table it is;
// a `vec![]` literal would force the setup out of line.
#[allow(clippy::vec_init_then_push)]
fn cases() -> Vec<Case> {
    let mut v = Vec::new();

    // ── SVF ──
    v.push(case(
        "svf_mono_lowpass",
        SvfFilterNode::<f64>::new(SvfType::LowPass, 1_200.0, 0.9),
        noise_channels(1),
    ));
    v.push(case(
        "svf_mono_bell",
        SvfFilterNode::<f64>::new(SvfType::Bell, 900.0, 1.4).with_gain_db(6.0),
        noise_channels(1),
    ));
    v.push(case(
        "svf_stereo_f32_bandpass",
        SvfFilterNode::<f32>::with_channels(ChannelLayout::STEREO, SvfType::BandPass, 300.0, 2.0),
        noise_channels(2),
    ));
    v.push(case(
        "svf_wide6_highshelf",
        SvfFilterNode::<f64>::with_channels(6usize, SvfType::HighShelf, 2_000.0, 0.7)
            .with_gain_db(-4.0),
        noise_channels(6),
    ));
    v.push(Case {
        params: vec![Some(sweep(200.0, 8_000.0)), None],
        ..case(
            "svf_stereo_swept",
            SvfFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                SvfType::LowPass,
                1_000.0,
                0.707,
            ),
            noise_channels(2),
        )
    });
    let node =
        SvfFilterNode::<f64>::with_channels(ChannelLayout::STEREO, SvfType::LowPass, 500.0, 0.8);
    let automation = automate(node.frequency(), |k| 300.0 + 700.0 * (k % 5) as f32);
    v.push(Case {
        automation,
        ..case("svf_stereo_automated", node, noise_channels(2))
    });

    // ── Ladder ──
    let node = LadderFilterNode::<f64>::new(LadderType::LP24, 1_100.0, 0.7);
    node.set_drive(2.5);
    v.push(case("ladder_mono_lp24_driven", node, noise_channels(1)));
    v.push(case(
        "ladder_stereo_f32_hp12",
        LadderFilterNode::<f32>::with_channels(ChannelLayout::STEREO, LadderType::HP12, 700.0, 0.5),
        noise_channels(2),
    ));
    v.push(case(
        "ladder_wide6_lp12",
        LadderFilterNode::<f64>::with_channels(6usize, LadderType::LP12, 1_500.0, 0.3),
        noise_channels(6),
    ));
    v.push(Case {
        params: vec![Some(sweep(200.0, 8_000.0)), None, None],
        ..case(
            "ladder_stereo_swept",
            LadderFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                LadderType::LP24,
                1_000.0,
                0.6,
            ),
            noise_channels(2),
        )
    });
    let node =
        LadderFilterNode::<f64>::with_channels(ChannelLayout::STEREO, LadderType::LP24, 800.0, 0.5);
    let automation = automate(node.drive(), |k| 1.0 + (k % 4) as f32);
    v.push(Case {
        automation,
        ..case("ladder_stereo_drive_automated", node, noise_channels(2))
    });

    // ── Delay ──
    let node =
        DelayLineNode::new(0.5, 0.012_34, 0.6).with_interpolation(InterpolationMode::CubicHermite);
    node.set_mix(0.4);
    v.push(case("delay_mono_cubic", node, noise_channels(1)));
    let node = DelayLineNode::stereo(0.5, 0.010, 0.017, 0.5);
    node.set_cross_feedback(0.3);
    node.set_mix(0.7);
    v.push(case("delay_stereo_cross_fed", node, noise_channels(2)));
    let node = DelayLineNode::with_channels(6usize, 0.5, 0.011, 0.45);
    node.set_mix(0.6);
    v.push(case("delay_wide6", node, noise_channels(6)));
    let feedback = (0..LEN).map(|i| 0.9 * (i as f32 / LEN as f32)).collect();
    let time = (0..LEN)
        .map(|i| 0.002 + 0.01 * (i as f32 / LEN as f32))
        .collect();
    let node = DelayLineNode::with_channels(ChannelLayout::STEREO, 0.5, 0.01, 0.4);
    node.set_cross_feedback(0.2);
    v.push(Case {
        params: vec![Some(feedback), Some(time)],
        ..case("delay_stereo_ports", node, noise_channels(2))
    });
    let n = 6usize;
    let mut ring = vec![0.0f32; n * n];
    for c in 0..n {
        ring[c * n + (c + n - 1) % n] = 1.0;
    }
    let node = DelayLineNode::with_channels(n, 0.1, 0.004, 0.3).with_cross_feedback_matrix(&ring);
    node.set_cross_feedback(0.5);
    v.push(case("delay_wide6_ring", node, noise_channels(6)));
    let node = DelayLineNode::stereo(0.5, 0.01, 0.013, 0.3);
    let automation = automate(node.feedback(), |k| 0.1 * (k % 8) as f32);
    let times = node.delay_time();
    let automation = {
        let fb = automation.expect("feedback automation");
        Some(Box::new(move |k: usize| {
            fb(k);
            times.store(0.005 + 0.001 * (k % 6) as f32, Ordering::Release);
        }) as Automation)
    };
    v.push(Case {
        automation,
        ..case("delay_stereo_automated", node, noise_channels(2))
    });

    // ── Chorus / flanger ──
    let node = ModDelayNode::chorus(ChannelLayout::STEREO);
    node.set_rate(1.7);
    node.set_depth(0.007);
    node.set_feedback(0.45);
    node.set_mix(0.6);
    v.push(case("chorus_stereo", node, noise_channels(2)));
    v.push(case(
        "flanger_stereo",
        ModDelayNode::flanger(ChannelLayout::STEREO),
        noise_channels(2),
    ));
    let node = ModDelayNode::chorus(6usize);
    let automation = automate(node.mix(), |k| (k % 3) as f32 * 0.5);
    v.push(Case {
        automation,
        ..case("chorus_wide6_mix_automated", node, noise_channels(6))
    });

    // ── Phaser ──
    let node = PhaserNode::new(6);
    node.set_rate(0.3);
    node.set_depth(0.8);
    node.set_feedback(0.6);
    v.push(case("phaser_mono", node, noise_channels(1)));
    let node = PhaserNode::with_channels(ChannelLayout::STEREO, 4);
    node.set_rate(2.0);
    v.push(case("phaser_stereo", node, noise_channels(2)));
    let node = PhaserNode::with_channels(6usize, 8).with_phase_offsets(&[
        PhaseIncrement(0.0),
        PhaseIncrement(0.1),
        PhaseIncrement(0.2),
        PhaseIncrement(0.3),
        PhaseIncrement(0.4),
        PhaseIncrement(0.5),
    ]);
    node.set_rate(3.0);
    v.push(case("phaser_wide6_staggered", node, noise_channels(6)));

    v
}

fn render(case: &mut Case) -> Vec<Vec<f32>> {
    let node = case.node.as_mut();
    node.set_sample_rate(SR);
    let (nin, nout) = (node.inputs(), node.outputs());
    assert_eq!(
        case.inputs.len(),
        nin,
        "{}: one input signal per port",
        case.name
    );
    let mut out = vec![vec![0.0f32; LEN]; nout];
    let mut ib = BufferVec::new(nin);
    let mut ob = BufferVec::new(nout);
    let (mut pos, mut k) = (0, 0);
    while pos < LEN {
        let n = PATTERN[k % PATTERN.len()].min(LEN - pos);
        if let Some(a) = &case.automation {
            a(k);
        }
        k += 1;
        for (c, sig) in case.inputs.iter().enumerate() {
            for i in 0..n {
                ib.set_f32(c, i, sig[pos + i]);
            }
        }
        if !case.params.is_empty() {
            let feed = node.param_feed().expect("a fed case has a feed");
            for (k, p) in case.params.iter().enumerate() {
                match p {
                    Some(v) => feed.feed(k, &v[pos..pos + n]),
                    None => feed.clear(k),
                }
            }
        }
        node.process(n, &ib.buffer_ref(), &mut ob.buffer_mut());
        for (c, o) in out.iter_mut().enumerate() {
            for i in 0..n {
                o[pos + i] = ob.at_f32(c, i);
            }
        }
        pos += n;
    }
    out
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
    ("svf_mono_bell", &[-1.0238142, 0.8077545, -0.592133, -0.015188786, -0.08614763, 0.67294055, 0.37375695, -0.34716427, 0.86114544, 1.15707, -0.06441335, -0.46211818, -0.24799393, 0.9144485, -0.9543525, -0.48883492, -0.783686, -0.177649, 0.41365302, -0.7419837, -0.5919085, -0.6681274]),
    ("svf_stereo_f32_bandpass", &[-0.019134818, 0.11532706, -0.11734694, 0.022560598, 0.104758315, 0.10882972, 0.02482237, 0.035443697, 0.025173923, 0.018440552, 0.19615355, 0.04424132, -0.05887313, 0.18730839, -0.11333053, -0.06098309, 0.019959308, -0.028842572, -0.05436393, 0.053399302, -0.021355785, 0.07886086, -0.007014321, 0.05028124, -0.051508192, -0.17275739, -0.15014313, 0.029949065, -0.011406783, -0.09176129, 0.027455578, -0.21556571, -0.086526096, -0.018930927, 0.12067772, -0.24278289, 0.21773621, -0.17539528, -0.0015360313, -0.109832, 0.004725826, -0.031341523, 0.1320488, -0.010230849]),
    ("svf_wide6_highshelf", &[-0.6513949, 0.5586584, -0.45681906, -0.026568512, -0.033419024, 0.45760763, 0.30119157, -0.14847864, 0.56263584, 0.7176178, 0.033694997, -0.24496846, -0.15097843, 0.6513446, -0.6330636, -0.34529218, -0.442524, -0.13477619, 0.2327337, -0.45635703, -0.32387456, -0.38652718, 0.088150226, 0.4449529, 0.28877237, 0.47008443, 0.6333616, -0.020463062, -0.2417335, 0.39350966, 0.14561006, -0.12488872, 0.49694398, 0.14363728, 0.16608673, -0.34068128, -0.24786252, 0.48509142, -0.59841317, 0.059210118, 0.13287242, -0.3152484, -0.024898635, -0.5936369]),
    ("svf_stereo_swept", &[-0.00016558991, 0.023960033, 0.06044471, -0.027316876, 0.10596977, -0.060756944, 0.006294443, 0.10998846, 0.07003359, 0.039997462, 0.12065747, 0.0034926268, 0.016005954, 0.24720554, -0.08594605, 0.07197945, -0.020992162, -0.038949598, 0.041584186, 0.09592846, -0.26017344, -0.5385236, -6.070091e-5, 0.053344782, 0.05109099, -0.01923143, -0.02824956, -0.023936601, -0.038923264, 0.10413141, 0.11167535, -0.26394466, -0.26374793, 0.027093336, 0.09999224, -0.18360175, 0.18672946, -0.18948632, 0.059886638, -0.35002634, 0.10843297, -0.5648451, 0.261462, 0.42373884]),
    ("svf_stereo_automated", &[-0.00037035995, 0.0911028, -0.25063244, -0.050343204, 0.27845114, -0.044346925, -0.1075007, 0.12316064, -0.00712395, -0.0017954546, 0.17950042, -0.09403474, 0.06962074, 0.26968265, -0.08499637, -0.13967653, 0.22054991, 0.14684682, 0.003048445, -0.07093374, -0.19257967, -0.06299128, -0.00013576422, 0.08509551, 0.21002236, -0.12950963, -0.3236585, -0.0075932355, 0.05151771, 0.097268455, -0.083217934, -0.3494967, -0.15192893, 0.0102081215, 0.13135603, -0.16700464, 0.1703083, -0.08431983, -0.1121287, -0.3565611, 0.05779628, -0.24092309, 0.25348464, 0.28589934]),
    ("ladder_mono_lp24_driven", &[-2.0179412e-5, 0.100083895, -0.09854974, -0.1159132, -0.049213927, 0.018638661, -0.009060835, 0.15012085, 0.05486078, -0.067531504, 0.08922538, 0.1094787, 0.012209649, 0.15876815, -0.058802735, -0.048392367, 0.17473558, -0.004299534, -0.06940406, 0.011137059, 0.081427634, 0.14184368]),
    ("ladder_stereo_f32_hp12", &[-0.7534904, 0.5465749, -0.5501151, -0.04403211, -0.27508676, 0.5911377, 0.29368624, -0.2890894, 0.6889444, 0.7591676, -0.2677787, -0.5228418, -0.15326098, 0.5013421, -0.7156228, -0.36287814, -0.8118862, -0.22035398, 0.43871272, -0.553428, -0.45482442, -0.59527683, -0.34528488, 0.48333716, -0.0815811, 0.84718823, 0.25487277, 0.7811093, -0.7709285, -0.46133226, -0.03895869, 0.5998117, -0.4159487, -0.29920468, 0.5543004, 0.6470004, 0.32071656, 0.52949685, -0.4743364, 0.029548153, 0.29985923, -0.2931441, -0.097719, 0.18537498]),
    ("ladder_wide6_lp12", &[-0.00606899, 0.05634594, 0.02745128, 0.04599959, 0.0994053, -0.040716633, -0.1526114, -0.09967061, 0.015973408, 0.14774323, 0.053282265, -0.036404334, 0.015576623, 0.14496636, -0.04112079, 0.022157159, 0.09159956, 0.098896235, 0.032722678, -0.052787688, -0.16119626, 0.073609464, 0.0010645648, 0.09411904, -0.07064848, 0.06390043, 0.18078318, 0.027059404, 0.04648643, 0.06987895, -0.000731159, 0.026027085, -0.1126548, -0.04284995, 0.05787807, 0.11421903, 0.018580182, -0.064215325, 0.11317414, 0.01909905, 0.18314043, -0.040072434, 0.030136323, 0.09538782]),
    ("ladder_stereo_swept", &[-2.104619e-8, -0.0028658581, 0.03956217, -0.024875514, 0.04216435, -0.05727426, 0.03189565, 0.04258808, 0.00043853518, -0.0594786, 0.028716603, 0.04152566, -0.029164655, 0.09537602, -0.081357256, -0.013287926, -0.0011298605, 0.07453745, -0.14706695, -0.17087352, 0.013439641, -0.21105199, -9.644359e-9, 0.00806897, 0.028084831, 0.017786173, 0.023653818, 0.0026750707, -0.037747875, 0.0197056, 0.0024159576, -0.09583444, -0.0887658, -0.021648914, 0.07071607, -0.022700565, 0.05108808, -0.124508165, 0.07172625, -0.12027041, 0.06857551, -0.1412065, 0.25298578, -0.023781395]),
    ("ladder_stereo_drive_automated", &[-4.6425857e-6, 0.044633128, 0.037091978, -0.07142935, 0.112267524, -0.023863778, 0.08899685, 0.053637344, -0.043542735, 0.013700816, 0.085002236, 0.06920269, -0.09545461, 0.0810235, -0.06565108, -0.16395174, 0.103837356, -0.013801612, -0.09498635, -0.065308884, 0.06612822, 0.0060226913, -2.127452e-6, 0.09347689, 0.036578022, -0.07523734, -0.051928505, -0.025762537, 0.016814288, 0.11894158, 0.020687332, -0.23224871, -0.077927746, 0.012324329, -0.07857084, -0.0812631, -0.061535943, -0.041143842, 0.09781838, -0.041788694, -0.013032822, 0.013263029, 0.088543095, -0.0023585989]),
    ("delay_mono_cubic", &[-0.5906077, 0.46551913, -0.40322366, -0.06945842, -0.102913864, 0.39537108, 0.24493125, -0.38860905, 0.51814145, 0.28469166, -0.121706754, 0.07076211, -0.2888013, 0.53743225, -0.678148, -0.4864468, -0.21962026, -0.13651243, 0.22279195, -0.005669415, -0.5924424, -0.6400535]),
    ("delay_stereo_cross_fed", &[-0.29530385, 0.23275957, -0.20161183, -0.03472921, -0.051456932, -0.054749966, -0.1317821, 0.20453398, -0.24021778, 0.18635175, -0.5241971, -0.046079382, 0.03347417, -0.33164853, -0.21952854, -0.7818478, -0.30711642, -0.07471803, 0.9419783, 0.050305843, 0.4469054, -0.06096995, -0.10825063, 0.2248193, -0.012494982, 0.2537414, 0.016921878, 0.27224883, -0.28439242, -0.116011836, 0.0016071439, -0.12755562, 0.045469046, -0.7234341, 0.74952376, -0.52449846, -0.71518046, 0.24835399, 0.70498145, 0.38631904, 0.7192677, 0.4470889, 0.6435833, -0.7979806]),
    ("delay_wide6", &[-0.39373845, 0.31034607, -0.26881573, -0.046305604, -0.06860923, 0.26358068, 0.4626307, 0.25507838, 0.090592116, 0.5930089, 0.14661828, -0.6798482, -0.11818276, -0.08466637, -0.3071038, -0.772961, 0.5223354, -0.50025976, -0.21975197, -0.07008763, 0.37599307, -0.44076127, 0.053282782, 0.25741094, 0.19196299, 0.2768318, 0.3872496, -0.039330576, 0.22841968, 0.60865396, -0.39452612, 0.19165376, 0.13541426, 0.060054373, -0.14876334, 0.041655213, 0.14157876, 0.6579304, -0.09727675, -0.86815995, -0.32513162, 0.26421654, 0.2521767, -0.110800505]),
    ("delay_stereo_ports", &[0.0, 0.0, -0.057941765, -0.18363556, 0.6873832, 0.2665431, 0.75175357, 0.07506809, 0.078468055, 0.13862559, -0.36369023, 0.65552455, -0.64108557, 0.26990336, -0.6770028, 0.49753568, -0.13420725, 0.79287636, -0.68734205, 0.15477514, 0.7163779, -0.10325426, 0.0, 0.0, -0.7975454, 0.3070774, 0.12343031, -0.23180485, 0.21826693, -0.5281636, 0.10300796, 0.015124828, -0.11815944, 0.45168746, -0.56550276, 0.02702251, -0.8583219, -0.19538368, 0.15064603, -0.12742636, -0.0026099645, -0.67666054, 0.02289322, -0.65968674]),
    ("delay_wide6_ring", &[0.0, 0.0, 0.67123896, 0.4526207, 0.93167233, -0.16448085, -0.7925237, 0.5325172, 0.8804439, 0.35766163, 1.5407981, -0.2997759, -0.7334603, -0.9944951, 0.929167, 1.3316729, 0.6390133, -0.043078505, 0.7337201, -0.8159842, -0.93990606, -0.11810403, 0.0, 0.0, -0.034278207, 0.67135245, -0.9025614, -0.35830835, -0.97840494, 1.3685048, 0.5557031, 0.6384473, 1.1626849, 0.03449375, -0.18275714, 0.009866723, 1.1915628, 0.052337658, -0.1392111, -0.17076084, 1.1717721, -0.18251248, -0.09306131, -2.092069]),
    ("delay_stereo_automated", &[0.0, 0.0, 0.0, 0.0, 0.5499736, -0.36062217, -0.9604371, -0.04295793, -0.95751333, -0.64748836, -0.4890436, -0.6326039, 0.029167749, 0.25295198, 0.61363846, 0.32942954, -0.17805457, -0.78002214, -0.45946267, 0.67310303, 0.28703308, -0.9399222, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.23299766, -0.31851935, 0.7464317, -0.045637965, -0.11094844, 0.69409835, 1.1177504, -0.10502858, -0.6933273, 0.14903003, -0.86115, -0.9613388, -0.022060752, -0.9073523, 0.55080104]),
    ("chorus_stereo", &[-0.39373845, 0.31034607, -0.26881573, -0.046305604, -0.06860923, 0.26358068, 0.27229378, 0.44766828, 0.23069896, 0.100815594, 0.19508824, -0.70988923, 0.01852844, 0.08251992, -0.33789212, 0.2539832, -0.7558361, 0.079921775, 0.53149056, -0.63170946, -0.5412444, -0.36192304, -0.14433417, 0.29975903, -0.016659973, 0.33832186, 0.022562502, 0.36299843, -0.37918985, -0.15468244, 0.0021428585, 0.050087348, -0.60144615, -0.02521687, 0.11446299, 0.22881004, 0.16639178, 0.20526984, 0.09712601, -0.020174272, -0.4381411, -0.005929284, 0.44926876, -0.10583801]),
    ("flanger_stereo", &[-0.49217308, 0.08582887, -0.19281091, -0.22395611, 0.120209396, 0.44908923, -0.118178725, -0.6463552, 0.729317, 0.75379884, -0.29875508, -0.5059612, 0.012133375, 0.1422835, -0.55487704, -0.663901, -0.58474076, 0.33767053, 0.051063955, -0.46998754, -0.83996654, -0.20535763, -0.18041772, 0.5079839, -0.41089422, 0.1626128, 0.0025583953, 1.2971752, -0.042922676, -0.18746278, 0.076207, -0.3538036, -0.07173279, -0.36126456, 0.82816017, 0.24534215, -0.13214871, -0.2562071, -0.25149482, 0.22239637, -0.047972813, -0.09622146, 0.3035599, 0.060752578]),
    ("chorus_wide6_mix_automated", &[-0.98434615, 0.569776, -0.4147743, -0.08863183, -0.1715231, 0.0, 0.3576153, -0.2401699, 0.797671, -0.3403402, -0.028785849, -0.18847479, -0.08674508, 0.5120504, 0.27028126, 0.27571, -0.49290684, -0.16525094, 0.19096693, 0.25265056, 0.36011243, -0.519384, 0.13320696, 0.47259042, 0.2961929, 0.5298734, 0.96812403, 0.0, -0.27664945, 0.40533936, 0.23787776, 0.56100875, 0.07193989, 0.07296692, 0.051662233, -0.5349426, -0.7020167, -0.043298878, -0.7593933, -0.028163336, -0.11241901, -0.060436875, 0.6426224, -0.67808264]),
    ("phaser_mono", &[-0.61818624, 0.30589634, -0.48952824, 0.12262848, -0.15952098, 0.081432045, 0.24488337, 0.20314533, 0.98322654, -0.02834326, 0.22402485, -0.14154926, 0.08667372, 0.36843282, 0.30412287, -0.32843468, -0.6013888, -0.25968057, 0.3256858, -1.0864637, -0.40468764, -1.1512823]),
    ("phaser_stereo", &[-0.76109064, 0.43745714, -0.9632483, 0.27126628, 0.36589944, 0.29650813, 0.21199954, 0.0058871806, 1.3456728, 0.6407036, 0.018512934, -0.38021326, -0.1007421, 0.15819561, -0.33253416, -0.3435398, -0.5848913, 0.16298911, 0.40266013, -0.86086065, -0.13940072, -0.43349463, -0.27899584, 0.91335315, 0.3392676, 0.826813, 0.2194686, 0.83684075, -0.55366737, -0.663572, 0.2172176, 0.46469882, -0.66760904, -0.016349465, 0.09150413, 0.6225576, 0.6066122, 0.20744781, 0.6855906, -0.41378555, 0.13909373, 0.14242539, 0.40095696, -0.07701981]),
    ("phaser_wide6_staggered", &[-0.63910645, 0.33355564, -0.7448835, 0.007336259, -0.22043723, 0.41424227, 0.24871683, 0.11593133, 0.8849528, -0.14399332, 0.22518803, -0.28316516, 0.3560504, 0.8217744, -0.3033846, -0.050831795, -0.7999824, 0.22121215, 0.46104833, -0.8714086, -0.67066497, -0.6860471, 0.08648729, 0.8102561, 0.4569773, 0.9264583, 0.86194694, 0.61968386, -0.31306946, 0.6397997, 0.414909, 0.13238168, 0.16489184, 0.14946483, -0.39636484, -0.43039966, 0.3766073, 0.5759995, -0.8675233, -0.237162, 0.21539676, -0.49720228, -0.29201812, -1.1978667]),
];
