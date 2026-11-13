//! Regression sweep (probed 2026-11, nothing to fix): every time, frequency and gain argument of the listed constructors,
//! at NaN, both infinities and a negative value: building and rendering never
//! panics and never produces a non-finite sample.

use std::panic::{AssertUnwindSafe, catch_unwind};
use tutti_types::MaxBlock;

use tutti_graph::contract::drive;
use tutti_graph::{Node, Prepare};
use tutti_nodes::*;
use tutti_types::{BusTopology, ChannelLayout, Db, Feedback, Hz, Q, SampleRate, Samples, Seconds};

const RATE: SampleRate = SampleRate::SR_48K;
// A huge finite value (1e30) is left out on purpose: it is a valid number that
// asks for an allocation no machine has (a delay line, a lookahead ring).
const BAD: [f32; 5] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, -1e30];

/// A compressor at -12 dB, 4:1, 10 ms and 100 ms, the knee and makeup at the default's.
fn compressor_at_twelve() -> CompressorControls {
    CompressorControls {
        threshold: Db(-12.0),
        ratio: tutti_types::CompressionRatio::new_clamped(4.0),
        attack: Seconds::from_f32(0.01),
        release: Seconds::from_f32(0.1),
        ..CompressorControls::DEFAULT
    }
}

fn finite(node: &mut dyn Node) -> bool {
    let width = usize::from(node.shape().audio_in().count());
    let mut ok = true;
    for k in 0..4 {
        let chans: Vec<Vec<f32>> = (0..width)
            .map(|c| {
                (0..64)
                    .map(|i| ((i + c + k * 64) as f32 * 0.3).sin() * 0.5)
                    .collect()
            })
            .collect();
        let refs: Vec<&[f32]> = chans.iter().map(|c| &c[..]).collect();
        ok &= drive(node, RATE, &refs, &[])
            .iter()
            .flatten()
            .all(|s| s.is_finite());
    }
    ok
}

fn probe(name: &str, bad: f32, make: impl FnOnce() -> Box<dyn Node>) -> Option<String> {
    let r = catch_unwind(AssertUnwindSafe(|| {
        let mut n = make();
        n.prepare(&Prepare::new(RATE, MaxBlock::of(Samples(64))));
        finite(&mut *n)
    }));
    match r {
        Err(_) => Some(format!("{name} with {bad}: PANIC")),
        Ok(false) => Some(format!("{name} with {bad}: non-finite output")),
        Ok(true) => None,
    }
}

/// Nothing is claimed beyond `non_finite_inputs.rs`'s (the delay and
/// limiter guards); this sweep widens the net over every constructor.
#[test]
fn sweep() {
    let mut bad_cases = vec![];
    let d = |v: f32| Db(v);
    let s = |v: f32| Seconds::from_f32(v);
    for b in BAD {
        let mut add = |r: Option<String>| bad_cases.extend(r);
        for pos in 0..4 {
            let v = |i: usize, ok: f32| if i == pos { b } else { ok };
            add(probe(&format!("compressor arg{pos}"), b, || {
                Box::new(CompressorNode::new(
                    BusTopology::STEREO,
                    KeySource::Sidechain,
                    CompressorControls {
                        threshold: d(v(0, -12.0)),
                        ratio: tutti_types::CompressionRatio::new_clamped(if pos == 1 {
                            b
                        } else {
                            4.0
                        }),
                        attack: s(v(2, 0.01)),
                        release: s(v(3, 0.1)),
                        ..CompressorControls::DEFAULT
                    },
                ))
            }));
            add(probe(&format!("gate arg{pos}"), b, || {
                Box::new(GateNode::new(
                    BusTopology::STEREO,
                    KeySource::Sidechain,
                    GateControls {
                        threshold: d(v(0, -40.0)),
                        attack: s(v(1, 0.01)),
                        hold: s(v(2, 0.05)),
                        release: s(v(3, 0.1)),
                        range: d(-80.0),
                    },
                ))
            }));
        }
        add(probe("compressor knee", b, || {
            Box::new(CompressorNode::new(
                BusTopology::STEREO,
                KeySource::Sidechain,
                CompressorControls {
                    knee: d(b),
                    ..compressor_at_twelve()
                },
            ))
        }));
        add(probe("compressor makeup", b, || {
            Box::new(CompressorNode::new(
                BusTopology::STEREO,
                KeySource::Sidechain,
                CompressorControls {
                    makeup: d(b),
                    ..compressor_at_twelve()
                },
            ))
        }));
        add(probe("gate range", b, || {
            Box::new(GateNode::new(
                BusTopology::STEREO,
                KeySource::Sidechain,
                GateControls {
                    threshold: d(-40.0),
                    attack: s(0.01),
                    hold: s(0.05),
                    release: s(0.1),
                    range: d(b),
                },
            ))
        }));
        add(probe("limiter thr", b, || {
            Box::new(LimiterNode::stereo(LimiterControls {
                threshold: d(b),
                ceiling: d(-1.0),
                ..LimiterControls::DEFAULT
            }))
        }));
        add(probe("limiter ceil", b, || {
            Box::new(LimiterNode::stereo(LimiterControls {
                threshold: d(-6.0),
                ceiling: d(b),
                ..LimiterControls::DEFAULT
            }))
        }));
        // The lookahead is a `Lookahead`, which refuses a non-finite value by type
        // (`a_lookahead_keeps_the_range_checked_shape` in tutti-dsp).
        add(probe("limiter release", b, || {
            Box::new(LimiterNode::stereo(LimiterControls {
                threshold: d(-6.0),
                ceiling: d(-1.0),
                release: s(b),
            }))
        }));
        add(probe("brickwall", b, || {
            Box::new(BrickwallLimiterNode::new(d(b)))
        }));
        add(probe("eq freq", b, || {
            Box::new(EqBandNode::<f64>::new(SvfType::Bell, Hz(b), Q(1.0), d(3.0)))
        }));
        add(probe("eq q", b, || {
            Box::new(EqBandNode::<f64>::new(
                SvfType::Bell,
                Hz(1000.0),
                Q(b),
                d(3.0),
            ))
        }));
        add(probe("eq gain", b, || {
            Box::new(EqBandNode::<f64>::new(
                SvfType::Bell,
                Hz(1000.0),
                Q(1.0),
                d(b),
            ))
        }));
        add(probe("svf freq", b, || {
            Box::new(SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(b), Q(1.0)))
        }));
        add(probe("svf q", b, || {
            Box::new(SvfFilterNode::<f64>::new(
                SvfType::LowPass,
                Hz(1000.0),
                Q(b),
            ))
        }));
        add(probe("svf gain", b, || {
            Box::new(SvfFilterNode::<f64>::with_controls(
                ChannelLayout::MONO,
                SvfControls {
                    ty: SvfType::Bell,
                    freq: Hz(1000.0),
                    q: Q(1.0),
                    gain: d(b),
                },
            ))
        }));
        add(probe("ladder freq", b, || {
            Box::new(LadderFilterNode::<f64>::new(
                tutti_nodes::LadderType::LP24,
                Hz(b),
                Resonance::new(0.5),
            ))
        }));
        add(probe("crossover", b, || {
            Box::new(
                CrossoverNode::new(ChannelLayout::STEREO, &[Hz(b)]).unwrap_or_else(|_| {
                    CrossoverNode::new(ChannelLayout::STEREO, &[Hz(1000.0)]).unwrap()
                }),
            )
        }));
        add(probe("crossover2", b, || {
            Box::new(
                CrossoverNode::new(ChannelLayout::STEREO, &[Hz(200.0), Hz(b)]).unwrap_or_else(
                    |_| CrossoverNode::new(ChannelLayout::STEREO, &[Hz(1000.0)]).unwrap(),
                ),
            )
        }));
        // The longest delay is a `MaxDelay`, which refuses a non-finite, negative or
        // huge value by type (`a_delay_with_an_unusable_maximum_is_refused_and_the_longest_builds`).
        add(probe("delay time", b, || {
            Box::new(DelayNode::mono(
                MaxDelay::of(s(0.5)),
                DelayControls {
                    time: s(b),
                    feedback: Feedback::new(0.3),
                    cross_feedback: Feedback::NONE,
                },
            ))
        }));
        // A channel's own time is a `ChannelTimes`, which refuses a time that is not
        // finite or is negative; the delay falls back to linked channels.
        add(probe("delay stereo", b, || {
            let family = DelayFamily::new(
                BusTopology::STEREO,
                MaxDelay::of(s(0.5)),
                InterpolationMode::Linear,
            );
            let family = match ChannelTimes::each(&[s(b)]) {
                Ok(times) => family.with_times(times).unwrap(),
                Err(_) => family,
            };
            Box::new(DelayNode::live(
                family,
                DelayControls {
                    time: s(b),
                    feedback: Feedback::new(0.3),
                    cross_feedback: Feedback::new(0.3),
                },
            ))
        }));
    }
    assert!(bad_cases.is_empty(), "{}", bad_cases.join("\n"));
}
