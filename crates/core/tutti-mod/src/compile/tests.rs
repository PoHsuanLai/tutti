//! `compile`'s properties (doc 014 §12, PR 3), against analytic figures.
//! Each test carries the mutations it was seen to fail under.

use super::*;
use crate::shape::shape;
use crate::LfoShape;
use audio_automation::{AutomationEnvelope, AutomationPoint};
use tutti_types::{Frame, Hz, UnitParam};

const SR: SampleRate = SampleRate(48_000.0);

fn now(epoch: u32, play: u64) -> PlayReading {
    PlayReading {
        epoch,
        play: Frame(play),
    }
}

fn target(node: u64) -> TargetKey {
    TargetKey {
        node,
        param: ParamAddr::Unit(UnitParam::Cutoff),
    }
}

fn plugin(node: u64, id: u32) -> TargetKey {
    TargetKey {
        node,
        param: ParamAddr::Id(id),
    }
}

fn ranged(min: f32, max: f32) -> TargetDecl {
    TargetDecl {
        min,
        max,
        automation: None,
    }
}

fn lfo(shape: LfoShape, clock: Clock) -> Source {
    Source::Lfo {
        wave: Wave::Shape { shape, seed: 0 },
        clock,
    }
}

fn beats(per_cycle: f64) -> Clock {
    Clock::Beats {
        per_cycle: BeatDuration(per_cycle),
        phase: PhaseIncrement(0.0),
    }
}

fn seconds(rate: f32) -> Clock {
    Clock::Seconds {
        rate: Hz(rate),
        phase: PhaseIncrement(0.0),
        anchor: None,
    }
}

fn route(source: u64, to: RouteTarget, depth: f32) -> RouteDecl {
    RouteDecl {
        source: SourceKey(source),
        to,
        depth: Depth(depth),
        polarity: Polarity::Bipolar,
        curve: CurveType::Linear,
    }
}

fn to(node: u64) -> RouteTarget {
    RouteTarget::Param(target(node))
}

fn follower() -> Source {
    Source::Follower {
        attack: Seconds(0.01),
        release: Seconds(0.2),
    }
}

fn adsr() -> Adsr {
    Adsr {
        attack: Seconds(0.01),
        decay: Seconds(0.1),
        sustain: Amplitude(0.6),
        release: Seconds(0.3),
    }
}

fn run(
    sources: &[(SourceKey, Source)],
    routes: &[(RouteKey, RouteDecl)],
    targets: &[(TargetKey, TargetDecl)],
) -> Result<Compiled, CompileError> {
    compile(
        &Declarations {
            sources,
            routes,
            targets,
        },
        None,
        now(1, 0),
        SR,
    )
}

fn time(beat: f64, play: f64, epoch: u32) -> ProgramTime {
    ProgramTime {
        beat: Beat(beat),
        play: SamplePosition(play),
        play_epoch: epoch,
        rate: SR,
    }
}

/// A frozen envelope: what a host hands as automation.
fn envelope() -> Arc<dyn Curve> {
    let mut e = AutomationEnvelope::new(());
    e.add_point(AutomationPoint::new(0.0, 100.0));
    e.add_point(AutomationPoint::new(4.0, 900.0));
    Arc::new(e)
}

/// A rig of every source kind: two LFOs (one scaled by a macro), a macro,
/// a follower and a note envelope, into three native targets and a plugin
/// parameter, one with automation.
struct Rig {
    sources: Vec<(SourceKey, Source)>,
    routes: Vec<(RouteKey, RouteDecl)>,
    targets: Vec<(TargetKey, TargetDecl)>,
}

impl Rig {
    fn new() -> Self {
        let sources = vec![
            (SourceKey(10), lfo(LfoShape::Sine, seconds(2.0))),
            (SourceKey(11), lfo(LfoShape::Triangle, beats(4.0))),
            (SourceKey(12), Source::Macro { value: 0.5 }),
            (SourceKey(13), follower()),
            (SourceKey(14), Source::NoteEnvelope { adsr: adsr() }),
            (SourceKey(15), Source::Macro { value: -0.25 }),
        ];
        let routes = vec![
            (RouteKey(100), route(10, to(1), 0.3)),
            (RouteKey(101), route(11, to(1), 0.1)),
            (RouteKey(102), route(12, to(2), 0.2)),
            (RouteKey(103), route(13, to(2), -0.6)),
            (
                RouteKey(104),
                route(14, RouteTarget::Param(plugin(4, 7)), 0.5),
            ),
            (
                RouteKey(105),
                route(12, RouteTarget::Depth(RouteKey(101)), 1.0),
            ),
            (RouteKey(106), route(15, to(3), 0.4)),
            (RouteKey(107), route(13, to(1), 0.25)),
        ];
        let targets = vec![
            (target(1), ranged(0.0, 1000.0)),
            (target(2), ranged(-1.0, 1.0)),
            (
                target(3),
                TargetDecl {
                    automation: Some(envelope()),
                    ..ranged(20.0, 20_000.0)
                },
            ),
            (plugin(4, 7), ranged(0.0, 1.0)),
        ];
        Self {
            sources,
            routes,
            targets,
        }
    }

    fn compile(&self, prev: Option<&Compiled>, at: PlayReading) -> Compiled {
        compile(
            &Declarations {
                sources: &self.sources,
                routes: &self.routes,
                targets: &self.targets,
            },
            prev,
            at,
            SR,
        )
        .expect("compiles")
    }

    fn route_mut(&mut self, key: u64) -> &mut RouteDecl {
        &mut self
            .routes
            .iter_mut()
            .find(|(k, _)| k.0 == key)
            .expect("a route")
            .1
    }

    fn source_mut(&mut self, key: u64) -> &mut Source {
        &mut self
            .sources
            .iter_mut()
            .find(|(k, _)| k.0 == key)
            .expect("a source")
            .1
    }
}

/// Equal declarations compile equal, in any order: reversed, rotated and
/// interleaved, every list; and the rig compiles to what it says (a program
/// per routed or automated target, a node per signal source, an edge per
/// signal route).
///
/// Mutation (run): new `SourceId`s assigned in declaration order
/// (`compile` iterating `decl.sources`, reversed, rather than the sorted
/// index) → reordered declarations swap the follower's and the note
/// envelope's ids → fails.
#[test]
fn equal_declarations_in_any_order_compile_equal() {
    let rig = Rig::new();
    let base = rig.compile(None, now(1, 0));
    assert_eq!(
        base.programs.keys().copied().collect::<Vec<_>>(),
        [target(1), target(2), target(3), plugin(4, 7)]
    );
    assert_eq!(base.nodes.len(), 2, "the follower and the note envelope");
    assert_eq!(
        base.edges.iter().copied().collect::<Vec<_>>(),
        [
            (SourceKey(13), target(1)),
            (SourceKey(13), target(2)),
            (SourceKey(14), plugin(4, 7)),
        ]
    );
    let reorders: [fn(&mut Vec<_>); 3] = [
        |v: &mut Vec<_>| v.reverse(),
        |v: &mut Vec<_>| v.rotate_left(1),
        |v: &mut Vec<_>| {
            let n = v.len();
            let (a, b) = v.split_at(n / 2);
            *v = b.iter().cloned().chain(a.iter().cloned()).collect();
        },
    ];
    for f in reorders {
        let mut r = Rig::new();
        f(&mut r.sources);
        let mut rt = r.routes.clone();
        rt.reverse();
        r.routes = rt;
        // A `TargetDecl` is `Clone` (its curve an `Arc`, kept by identity).
        let mut ts = rig.targets.clone();
        ts.reverse();
        r.targets = ts;
        assert_eq!(r.compile(None, now(1, 0)), base);
    }
}

/// M1 closed: for every source kind (LFO, macro, follower, note envelope,
/// and an LFO scaled by a macro through `depth_by`), each polarity and
/// curve, the offset at raw ±1 is `shape(±1, depth, polarity, curve) ×
/// span`, and at raw ±0.5 likewise (where the curve bends: every curve
/// maps 1 to 1), on a native target and a plugin parameter alike: the compiled
/// program's `value_at` (the lane's evaluation) on knob 0 reads exactly
/// that.
///
/// Mutations (run), each fails: `RouteDecl::amount` dropping the curve
/// (`curve: CurveType::Linear`) → the exponential rows read the linear
/// figure; dropping the polarity → the unipolar rows read the bipolar one;
/// the depth route's amount used for the scaled layer → the scaled LFO
/// reads the full depth.
#[test]
fn the_offset_at_raw_unit_is_shape_times_span_for_every_source_kind() {
    let curves = [
        CurveType::Linear,
        CurveType::Exponential,
        CurveType::Logarithmic,
        CurveType::SCurve,
    ];
    let span = 800.0f64;
    for polarity in [Polarity::Bipolar, Polarity::Unipolar] {
        for curve in curves {
            for raw in [1.0f32, -1.0, 0.5, -0.5] {
                let depth = Depth(-0.35);
                let want =
                    (0.0f64 + (-0.0 + f64::from(shape(raw, depth, polarity, curve)) * span)) as f32;
                let r = |source, t| RouteDecl {
                    source: SourceKey(source),
                    to: RouteTarget::Param(t),
                    depth,
                    polarity,
                    curve,
                };
                // A triangle on 4 beats is +1 at beat 1, −1 at beat 3, +0.5
                // at beat 0.5 and −0.5 at beat 2.5, exactly.
                let beat = match raw {
                    1.0 => 1.0,
                    -1.0 => 3.0,
                    0.5 => 0.5,
                    _ => 2.5,
                };
                let at = time(beat, 0.0, 1);
                let sources = [
                    (SourceKey(1), lfo(LfoShape::Triangle, beats(4.0))),
                    (SourceKey(2), Source::Macro { value: raw }),
                    (SourceKey(3), follower()),
                    (SourceKey(4), Source::NoteEnvelope { adsr: adsr() }),
                    (SourceKey(5), Source::Macro { value: 1.0 }),
                ];
                let routes = [
                    (RouteKey(1), r(1, target(1))),
                    (RouteKey(2), r(2, target(2))),
                    (RouteKey(3), r(3, target(3))),
                    (RouteKey(4), r(4, plugin(4, 1))),
                    (RouteKey(5), r(1, plugin(5, 2))),
                    // Scales route 5 by a macro at +1 through the unit
                    // amount: a scale of exactly 1.
                    (
                        RouteKey(6),
                        RouteDecl {
                            source: SourceKey(5),
                            to: RouteTarget::Depth(RouteKey(5)),
                            depth: Depth::FULL,
                            polarity: Polarity::Unipolar,
                            curve: CurveType::Linear,
                        },
                    ),
                ];
                let targets = [
                    (target(1), ranged(0.0, 800.0)),
                    (target(2), ranged(0.0, 800.0)),
                    (target(3), ranged(0.0, 800.0)),
                    (plugin(4, 1), ranged(0.0, 800.0)),
                    (plugin(5, 2), ranged(0.0, 800.0)),
                ];
                let c = run(&sources, &routes, &targets).expect("compiles");
                let at_unit = |t: TargetKey, inputs: &[Option<f32>]| {
                    c.programs[&t].value_at(Some(0.0), &at, span, inputs)
                };
                let case = format!("{polarity:?} {curve:?} raw {raw}");
                assert_eq!(at_unit(target(1), &[]), Some(want), "LFO, {case}");
                assert_eq!(at_unit(target(2), &[]), Some(want), "macro, {case}");
                assert_eq!(
                    at_unit(target(3), &[Some(raw)]),
                    Some(want),
                    "follower, {case}"
                );
                assert_eq!(
                    at_unit(plugin(4, 1), &[Some(raw)]),
                    Some(want),
                    "note envelope, {case}"
                );
                assert_eq!(
                    at_unit(plugin(5, 2), &[]),
                    Some(want),
                    "depth_by-scaled LFO, {case}"
                );
            }
        }
    }
}

/// A depth edit changes only its target's program, and keeps every id: the
/// edited route's layer differs in its amount alone; every other target's
/// program, the nodes and the edges are equal.
///
/// Mutation (run): the layer id mixing in the depth (`layer_id` hashing
/// `route.depth.to_bits()` too) → the edited layer's id changes → fails.
#[test]
fn a_depth_edit_changes_only_its_targets_program_and_keeps_its_ids() {
    let mut rig = Rig::new();
    let before = rig.compile(None, now(1, 0));
    rig.route_mut(100).depth = Depth(0.7);
    let after = rig.compile(Some(&before), now(1, 5_000));
    for (t, p) in &after.programs {
        if *t == target(1) {
            assert_ne!(
                p, &before.programs[t],
                "the edited target's program changed"
            );
            let ids = |p: &ControlProgram| p.layers().iter().map(|l| l.id).collect::<Vec<_>>();
            assert_eq!(ids(p), ids(&before.programs[t]), "ids kept");
            for (a, b) in p.layers().iter().zip(before.programs[t].layers()) {
                let same = Layer {
                    amount: b.amount,
                    ..a.clone()
                };
                assert_eq!(&same, b, "only the amount moved");
            }
        } else {
            assert_eq!(p, &before.programs[t], "{t:?} untouched");
        }
    }
    assert_eq!(after.nodes, before.nodes);
    assert_eq!(after.edges, before.edges);
}

/// A macro move changes exactly the programs it reaches: macro 12 goes to
/// target 2 directly and scales route 101 (to target 1); targets 3 and the
/// plugin parameter are equal; and the moved macro reads at its new value.
///
/// Mutation (run): the depth-by wave taken from the scaled route's own
/// source (`time_function(&sources[&route.source])`) → target 1 does not
/// change with the macro → fails.
#[test]
fn a_macro_move_changes_exactly_the_programs_it_reaches() {
    let mut rig = Rig::new();
    let before = rig.compile(None, now(1, 0));
    *rig.source_mut(12) = Source::Macro { value: -0.75 };
    let after = rig.compile(Some(&before), now(1, 0));
    let changed: Vec<TargetKey> = after
        .programs
        .iter()
        .filter(|(t, p)| before.programs[*t] != **p)
        .map(|(t, _)| *t)
        .collect();
    assert_eq!(changed, [target(1), target(2)]);
    let at = time(0.0, 0.0, 1);
    assert_eq!(
        after.programs[&target(2)].value_at(Some(0.0), &at, 2.0, &[]),
        Some(
            (-0.0
                + f64::from(shape(
                    -0.75,
                    Depth(0.2),
                    Polarity::Bipolar,
                    CurveType::Linear
                )) * 2.0) as f32
        ),
        "depth 0.2 of a span of 2, at the macro's −0.75"
    );
}

/// A rate edit anchors continuously from the previous program: a 2 Hz sine
/// edited to 5 Hz at play 10 000 (epoch 7) takes effect
/// `RATE_ANCHOR_MARGIN` frames on, where the new program's phase is the
/// old one's exactly; before it the new program is the old one (within
/// 1e-9 cycles), from it the phase moves at 5 Hz; and the value has no
/// step at the anchor larger than one frame's slope at the new rate.
///
/// Mutations (run), each fails: the anchor's phase from the new rate
/// (`c.cycles` on the edited clock) → the phases differ at the anchor; no
/// anchor (`anchored` returning the clock as declared) → the value jumps
/// at the edit; the anchor's `prev_rate` the new rate → before the anchor
/// the phases part.
#[test]
fn a_rate_edit_anchors_continuously() {
    let mut rig = Rig::new();
    let before = rig.compile(None, now(7, 0));
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(5.0));
    let edit = now(7, 10_000);
    let after = rig.compile(Some(&before), edit);
    let at = edit.after(RATE_ANCHOR_MARGIN).play;
    let id = layer_id(RouteKey(100), None);
    let clock = |c: &Compiled| {
        c.programs[&target(1)]
            .layers()
            .iter()
            .find(|l| l.id == id)
            .expect("the layer")
            .clock
    };
    let Clock::Seconds {
        anchor: Some(a), ..
    } = clock(&after)
    else {
        panic!("anchored: {:?}", clock(&after));
    };
    assert_eq!((a.epoch, a.at, a.prev_rate), (7, at, Hz(2.0)));
    let cyc = |c: &Compiled, play: f64| clock(c).cycles(&time(0.0, play, 7)).expect("a phase");
    let t = at.get() as f64;
    assert_eq!(
        cyc(&after, t),
        cyc(&before, t),
        "phase-continuous at the anchor"
    );
    for p in [10_000.0, 15_000.0, t - 1.0] {
        assert!((cyc(&after, p) - cyc(&before, p)).abs() < 1e-9, "at {p}");
    }
    let slope = |p: f64| (cyc(&after, p + 480.0) - cyc(&after, p)) / 480.0 * SR.get();
    assert!((slope(t) - 5.0).abs() < 1e-6, "5 Hz from the anchor");
    assert!((slope(t - 960.0) - 2.0).abs() < 1e-6, "2 Hz before it");
    // The value through the lane's evaluation: no step at the anchor.
    let v = |play: f64| {
        let p = &after.programs[&target(1)];
        f64::from(
            p.value_at(Some(0.0), &time(0.0, play, 7), 1000.0, &[None])
                .expect("a value"),
        )
    };
    // 0.3 of a span of 1000 at 5 Hz: at most 300 · 2π · 5 / 48 000 a frame.
    let frame_slope = 300.0 * std::f64::consts::TAU * 5.0 / SR.get();
    assert!((v(t) - v(t - 1.0)).abs() <= frame_slope * 1.01);
}

/// What follows a rate edit keeps it: a depth edit republished while the
/// anchor is in force carries it unchanged (else the phase would jump back
/// to `rate · play`); a second rate edit while the anchor is still
/// `ANCHOR_REUSE_MIN` or more ahead keeps the anchor's frame (the near case
/// is `a_second_rate_edit_near_its_anchor_reanchors`); a play start (a new
/// epoch) drops it; and a phase edit moves a carried anchor's phase by the
/// edit.
///
/// Mutations (run), each fails: the same-rate path dropping the live
/// anchor (`anchor: None`) → the depth edit's program has no anchor; the
/// pending case re-anchoring at `now + margin` → the second edit's anchor
/// moves, and so does the reuse bound raised to the full margin (the
/// 10 288 frames left then re-anchor); carrying an anchor across epochs
/// (`live = old_anchor`) → the new epoch keeps it; the phase jump not
/// applied to a carried anchor → its phase is unmoved.
#[test]
fn an_anchor_is_carried_until_a_play_start() {
    let id = layer_id(RouteKey(100), None);
    let anchor = |c: &Compiled| match c.programs[&target(1)]
        .layers()
        .iter()
        .find(|l| l.id == id)
        .expect("the layer")
        .clock
    {
        Clock::Seconds { anchor, .. } => anchor,
        Clock::Beats { .. } => None,
    };
    let mut rig = Rig::new();
    let c0 = rig.compile(None, now(7, 0));
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(5.0));
    let c1 = rig.compile(Some(&c0), now(7, 10_000));
    let a1 = anchor(&c1).expect("anchored");

    rig.route_mut(100).depth = Depth(0.9);
    let c2 = rig.compile(Some(&c1), now(7, 30_000));
    assert_eq!(anchor(&c2), Some(a1), "a depth edit carries it");

    let mut pending = Rig::new();
    *pending.source_mut(10) = lfo(LfoShape::Sine, seconds(3.0));
    let c3 = pending.compile(Some(&c1), now(7, 12_000));
    let a3 = anchor(&c3).expect("anchored");
    assert_eq!(
        (a3.at, a3.phase, a3.prev_rate),
        (a1.at, a1.phase, a1.prev_rate),
        "a second edit before the anchor lands on its frame"
    );

    let c4 = rig.compile(Some(&c2), now(8, 0));
    assert_eq!(anchor(&c4), None, "a play start drops it");

    *rig.source_mut(10) = Source::Lfo {
        wave: Wave::Shape {
            shape: LfoShape::Sine,
            seed: 0,
        },
        clock: Clock::Seconds {
            rate: Hz(5.0),
            phase: PhaseIncrement(0.25),
            anchor: None,
        },
    };
    let c5 = rig.compile(Some(&c2), now(7, 40_000));
    let a5 = anchor(&c5).expect("carried");
    assert_eq!((a5.at, a5.prev_rate), (a1.at, a1.prev_rate));
    assert_eq!(a5.phase, a1.phase + 0.25, "the phase edit moves it");
    // Equal inputs, equal outputs: compiling again changes nothing.
    assert_eq!(rig.compile(Some(&c2), now(7, 40_000)), c5);
}

/// A signal source keeps its `SourceId` across compiles: adding a follower
/// with a lower key does not renumber the existing ones (the new one takes
/// the next id never given). Removing one frees nothing: no later compile
/// gives its id again, so no lane can read a removed source's last segment
/// as a new source's (doc 014 §7).
///
/// Changed in doc 014 PR 3b: a removed source's id was reusable from the
/// compile after the one that removed it (the last assertion read
/// `SourceId(0)`, "freed last time"); a lane that skipped the program
/// between could then read the stale segment. Ids are now never reused.
///
/// Mutations (run), each fails: `prev` ignored (`kept` always `None`) →
/// the new follower takes id 0 and renumbers the others; the next id not
/// carried (`next` starting at 0: the lowest id not held) → follower 2
/// takes 13's id 0 in the compile that removes 13.
#[test]
fn a_signal_source_keeps_its_id() {
    let rig = Rig::new();
    let c0 = rig.compile(None, now(1, 0));
    assert_eq!(c0.nodes[&SourceKey(13)].id, SourceId(0));
    assert_eq!(c0.nodes[&SourceKey(14)].id, SourceId(1));
    let mut more = Rig::new();
    more.sources.push((SourceKey(1), follower()));
    let c1 = more.compile(Some(&c0), now(1, 0));
    assert_eq!(c1.nodes[&SourceKey(13)].id, SourceId(0));
    assert_eq!(c1.nodes[&SourceKey(14)].id, SourceId(1));
    assert_eq!(c1.nodes[&SourceKey(1)].id, SourceId(2));
    assert_eq!(
        c1.nodes[&SourceKey(14)].kind,
        SignalKind::NoteEnvelope { adsr: adsr() }
    );

    // Remove follower 13 (and its routes) and add follower 2 in one compile.
    let mut swap = more;
    swap.sources.retain(|(k, _)| k.0 != 13);
    swap.routes.retain(|(_, r)| r.source != SourceKey(13));
    swap.sources.push((SourceKey(2), follower()));
    let c2 = swap.compile(Some(&c1), now(1, 0));
    assert!(!c2.nodes.contains_key(&SourceKey(13)));
    assert_eq!(c2.nodes[&SourceKey(2)].id, SourceId(3), "not 13's id");
    // Nor in any later compile.
    swap.sources.push((SourceKey(3), follower()));
    let c3 = swap.compile(Some(&c2), now(1, 0));
    assert_eq!(c3.nodes[&SourceKey(2)].id, SourceId(3));
    assert_eq!(c3.nodes[&SourceKey(3)].id, SourceId(4), "never 13's id");
    assert_eq!(c3.next_source, SourceId(5));
}

/// Each refusal fires, by name.
///
/// Mutations (run), each fails on its row: the layer and input caps not
/// mapped (`TooManyLayers` reported as `LayerIdCollision`); the NaN range
/// check dropped; the frozen check dropped; the chain check dropped (the
/// chain then compiles); the signal check dropped; the second depth route
/// overwriting the first (`insert` without the `is_some` check).
#[test]
fn each_refusal_fires_by_name() {
    let t1 = [(target(1), ranged(0.0, 1.0))];
    let one_lfo = [(SourceKey(1), lfo(LfoShape::Sine, beats(1.0)))];

    // Past MAX_LANE_LAYERS: 17 routes from one LFO into one target.
    let routes: Vec<_> = (0..17)
        .map(|k| (RouteKey(k), route(1, to(1), 0.1)))
        .collect();
    assert_eq!(
        run(&one_lfo, &routes, &t1),
        Err(CompileError::TooManyLayers {
            target: target(1),
            count: 17
        })
    );
    // Past MAX_LANE_INPUTS: 9 follower routes.
    let f = [(SourceKey(1), follower())];
    let routes: Vec<_> = (0..9)
        .map(|k| (RouteKey(k), route(1, to(1), 0.1)))
        .collect();
    assert_eq!(
        run(&f, &routes, &t1),
        Err(CompileError::TooManyInputs {
            target: target(1),
            count: 9
        })
    );
    let r1 = [(RouteKey(1), route(1, to(1), 0.5))];
    // A NaN range bound.
    assert_eq!(
        run(&one_lfo, &r1, &[(target(1), ranged(f32::NAN, 1.0))]),
        Err(CompileError::NanRange(target(1)))
    );
    // An automation curve that is not frozen.
    struct Live;
    impl Curve for Live {
        fn value_at(&self, _: Beat) -> Option<f32> {
            Some(0.0)
        }
        fn frozen(&self) -> Option<Arc<dyn Curve>> {
            Some(Arc::new(Live))
        }
    }
    let live = [(
        target(1),
        TargetDecl {
            automation: Some(Arc::new(Live)),
            ..ranged(0.0, 1.0)
        },
    )];
    assert_eq!(
        run(&one_lfo, &r1, &live),
        Err(CompileError::LiveCurve(target(1)))
    );
    // A depth_by chain deeper than one.
    let two = [
        (SourceKey(1), lfo(LfoShape::Sine, beats(1.0))),
        (SourceKey(2), Source::Macro { value: 0.5 }),
    ];
    let chain = [
        (RouteKey(1), route(1, to(1), 0.5)),
        (RouteKey(2), route(2, RouteTarget::Depth(RouteKey(1)), 1.0)),
        (RouteKey(3), route(2, RouteTarget::Depth(RouteKey(2)), 1.0)),
    ];
    assert_eq!(
        run(&two, &chain, &t1),
        Err(CompileError::DepthByChain { route: RouteKey(3) })
    );
    // Two depth routes to one route.
    let twice = [
        (RouteKey(1), route(1, to(1), 0.5)),
        (RouteKey(2), route(2, RouteTarget::Depth(RouteKey(1)), 1.0)),
        (RouteKey(3), route(1, RouteTarget::Depth(RouteKey(1)), 1.0)),
    ];
    assert_eq!(
        run(&two, &twice, &t1),
        Err(CompileError::TwoDepthBy { route: RouteKey(1) })
    );
    // A depth route from a signal source, and to a route from one.
    let sig = [
        (SourceKey(1), lfo(LfoShape::Sine, beats(1.0))),
        (SourceKey(2), follower()),
    ];
    let from_signal = [
        (RouteKey(1), route(1, to(1), 0.5)),
        (RouteKey(2), route(2, RouteTarget::Depth(RouteKey(1)), 1.0)),
    ];
    assert_eq!(
        run(&sig, &from_signal, &t1),
        Err(CompileError::DepthBySignal { route: RouteKey(2) })
    );
    let of_signal = [
        (RouteKey(1), route(2, to(1), 0.5)),
        (RouteKey(2), route(1, RouteTarget::Depth(RouteKey(1)), 1.0)),
    ];
    assert_eq!(
        run(&sig, &of_signal, &t1),
        Err(CompileError::DepthBySignal { route: RouteKey(2) })
    );
    // Unknown references.
    assert_eq!(
        run(&[], &r1, &t1),
        Err(CompileError::UnknownSource { route: RouteKey(1) })
    );
    assert_eq!(
        run(
            &one_lfo,
            &[(RouteKey(2), route(1, RouteTarget::Depth(RouteKey(9)), 1.0))],
            &t1
        ),
        Err(CompileError::UnknownRoute { route: RouteKey(2) })
    );
    assert_eq!(
        run(&one_lfo, &r1, &[]),
        Err(CompileError::UndeclaredTarget { route: RouteKey(1) })
    );
    // Duplicate keys.
    assert_eq!(
        run(&[one_lfo[0].clone(), one_lfo[0].clone()], &r1, &t1),
        Err(CompileError::DuplicateSource(SourceKey(1)))
    );
    assert_eq!(
        run(&one_lfo, &[r1[0], r1[0]], &t1),
        Err(CompileError::DuplicateRoute(RouteKey(1)))
    );
    assert_eq!(
        run(&one_lfo, &r1, &[t1[0].clone(), t1[0].clone()]),
        Err(CompileError::DuplicateTarget(target(1)))
    );
    // Sources out of their domain, and a declared anchor.
    let bad_sources = [
        Source::Macro { value: f32::NAN },
        Source::Follower {
            attack: Seconds(-0.1),
            release: Seconds(0.1),
        },
        Source::NoteEnvelope {
            adsr: Adsr {
                sustain: Amplitude(1.5),
                ..adsr()
            },
        },
        lfo(LfoShape::Sine, seconds(f32::NAN)),
        Source::Lfo {
            wave: Wave::Const(f32::NAN),
            clock: beats(1.0),
        },
    ];
    for s in bad_sources {
        assert_eq!(
            run(&[(SourceKey(1), s.clone())], &r1, &t1),
            Err(CompileError::BadSource(SourceKey(1))),
            "{s:?}"
        );
    }
    let anchored = Source::Lfo {
        wave: Wave::Shape {
            shape: LfoShape::Sine,
            seed: 0,
        },
        clock: Clock::Seconds {
            rate: Hz(1.0),
            phase: PhaseIncrement(0.0),
            anchor: Some(RateAnchor {
                epoch: 1,
                at: Frame(0),
                phase: 0.0,
                prev_rate: Hz(1.0),
            }),
        },
    };
    assert_eq!(
        run(&[(SourceKey(1), anchored)], &r1, &t1),
        Err(CompileError::AnchorDeclared(SourceKey(1)))
    );
    // A NaN depth.
    assert_eq!(
        run(&one_lfo, &[(RouteKey(1), route(1, to(1), f32::NAN))], &t1),
        Err(CompileError::BadRoute(RouteKey(1)))
    );
    // At the caps, it compiles.
    let routes: Vec<_> = (0..16)
        .map(|k| (RouteKey(k), route(1, to(1), 0.1)))
        .collect();
    assert!(run(&one_lfo, &routes, &t1).is_ok());
}

/// Exit (doc 014 §12): what `bevy-tutti` expresses today compiles with no
/// runtime: only the ungated floor (no `routing` feature, no driver, no
/// target), with each feature as a declaration:
///
/// - a `ModSource { shape }` at `ModClock::Free { hz }` or
///   `ModClock::Synced { beats_per_cycle }` with a `phase_offset`: an LFO
///   on a seconds or a beat clock, every shape (the random ones as hashed
///   waves);
/// - a `ModRoute { depth, polarity, curve }` to a native param or a plugin
///   parameter (`ParamAddr::Id`), whatever its `ModDelivery` (every tier
///   is now a lane); a disabled one is left out;
/// - a `ModParamRange { min, max }`: the target's range (its `base` is the
///   knob, a cell, not a declaration);
/// - a plugin parameter's automation curve, and an LFO route into a plugin
///   parameter (`LfoCurve`, `LfoOffset`): automation and a layer.
///
/// A custom `ModSourceKind` (an arbitrary `Modulator`) has no closed form;
/// PR 4 gives it `declare() -> Source` (doc 014 §8.1).
///
/// Mutation: none applies (it pins that the declarations compile, which
/// the type checker and the run assert; removing a source kind fails to
/// build). The no-runtime half is `cargo check -p tutti-mod` without
/// features, run with this PR.
#[test]
fn what_bevy_tutti_expresses_compiles_without_a_runtime() {
    let mut sources = Vec::new();
    let mut routes = Vec::new();
    for (k, &shape) in LfoShape::all().iter().enumerate() {
        let k = k as u64;
        let phase = PhaseIncrement(0.25);
        let free = Clock::Seconds {
            rate: Hz(1.5),
            phase,
            anchor: None,
        };
        let synced = Clock::Beats {
            per_cycle: BeatDuration(2.0),
            phase,
        };
        sources.push((
            SourceKey(2 * k),
            Source::Lfo {
                wave: Wave::Shape { shape, seed: 0 },
                clock: free,
            },
        ));
        sources.push((
            SourceKey(2 * k + 1),
            Source::Lfo {
                wave: Wave::Shape { shape, seed: 0 },
                clock: synced,
            },
        ));
        for (j, (polarity, curve)) in [
            (Polarity::Bipolar, CurveType::Linear),
            (Polarity::Unipolar, CurveType::Exponential),
        ]
        .into_iter()
        .enumerate()
        {
            let s = 2 * k + j as u64;
            routes.push((
                RouteKey(100 + s),
                RouteDecl {
                    source: SourceKey(s),
                    to: RouteTarget::Param(target(1 + k % 2)),
                    depth: Depth(0.3),
                    polarity,
                    curve,
                },
            ));
        }
    }
    routes.push((
        RouteKey(999),
        route(0, RouteTarget::Param(plugin(5, 3)), 0.2),
    ));
    let targets = [
        (target(1), ranged(20.0, 20_000.0)),
        (target(2), ranged(0.0, 1.0)),
        (
            plugin(5, 3),
            TargetDecl {
                automation: Some(envelope()),
                ..ranged(0.0, 1.0)
            },
        ),
    ];
    let c = run(&sources, &routes, &targets).expect("compiles");
    assert_eq!(c.programs.len(), 3);
    assert!(c.nodes.is_empty() && c.edges.is_empty(), "no signal source");
    assert_eq!(
        c.programs[&target(1)].layers().len() + c.programs[&target(2)].layers().len(),
        LfoShape::all().len() * 2
    );
}

/// The clock of `route`'s layer in `target` (`depth_by` its scaler's id).
fn layer_clock(c: &Compiled, t: TargetKey, route: u64, depth_by: Option<u64>) -> Clock {
    let id = layer_id(RouteKey(route), depth_by.map(RouteKey));
    c.programs[&t]
        .layers()
        .iter()
        .find(|l| l.id == id)
        .expect("the layer")
        .clock
}

fn anchor_of(clock: Clock) -> Option<RateAnchor> {
    match clock {
        Clock::Seconds { anchor, .. } => anchor,
        Clock::Beats { .. } => None,
    }
}

/// One LFO is phase-coherent across its targets (doc 014 §4.3): a route
/// added to an LFO after its rate edit reads the anchor the LFO's other
/// layers read, so at any frame both targets are at one phase.
///
/// Mutation (run): anchors looked up by `(target, LayerId)` in the
/// previous programs, not by source (the PR's first `anchor` closure) →
/// the new route's layer has no anchor, so its clock differs from route
/// 100's → fails.
#[test]
fn a_route_added_after_a_rate_edit_reads_the_lfos_anchor() {
    let mut rig = Rig::new();
    let c0 = rig.compile(None, now(3, 0));
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(4.0));
    let c1 = rig.compile(Some(&c0), now(3, 480_000));
    rig.routes.push((RouteKey(200), route(10, to(2), 0.5)));
    let c2 = rig.compile(Some(&c1), now(3, 600_000));
    let one = layer_clock(&c2, target(1), 100, None);
    let two = layer_clock(&c2, target(2), 200, None);
    assert!(anchor_of(one).is_some(), "the edit's anchor is carried");
    assert_eq!(one, two, "both layers read the LFO's one clock");
    assert_eq!(c2.clocks[&SourceKey(10)], one);
    let t = time(0.0, 1e6, 3);
    assert_eq!(one.cycles(&t), two.cycles(&t));
}

/// A route moved to another LFO takes that LFO's clock: it is not a rate
/// edit of the route's layer, and does not continue the first LFO's phase
/// at the second's rate.
///
/// Mutation (run): anchors looked up by `(target, LayerId)` → route 100's
/// layer is taken for a rate edit (2 Hz to 3 Hz) and anchored at A's
/// phase, so B's two layers part → fails.
#[test]
fn a_route_moved_to_another_lfo_takes_that_lfos_clock() {
    let mut rig = Rig::new();
    rig.sources
        .push((SourceKey(20), lfo(LfoShape::Sine, seconds(3.0))));
    rig.routes.push((RouteKey(200), route(20, to(2), 0.5)));
    let c0 = rig.compile(None, now(3, 0));
    rig.route_mut(100).source = SourceKey(20);
    let c1 = rig.compile(Some(&c0), now(3, 480_000));
    let moved = layer_clock(&c1, target(1), 100, None);
    assert_eq!(moved, seconds(3.0), "B's own clock, no anchor");
    assert_eq!(moved, layer_clock(&c1, target(2), 200, None));
}

/// Adding or removing a `depth_by` (which changes the layer's id) keeps the
/// LFO's live anchor: the layer is the same LFO's, at the same phase.
///
/// Mutation (run): anchors looked up by `(target, LayerId)` → the scaled
/// layer's new id finds no old layer, so its anchor is dropped (its phase
/// jumps) → fails.
#[test]
fn a_depth_by_change_keeps_the_lfos_anchor() {
    let mut rig = Rig::new();
    let c0 = rig.compile(None, now(3, 0));
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(4.0));
    let c1 = rig.compile(Some(&c0), now(3, 480_000));
    let a1 = anchor_of(layer_clock(&c1, target(1), 100, None)).expect("anchored");
    rig.routes.push((
        RouteKey(201),
        route(15, RouteTarget::Depth(RouteKey(100)), 1.0),
    ));
    let c2 = rig.compile(Some(&c1), now(3, 600_000));
    let scaled = layer_clock(&c2, target(1), 100, Some(201));
    assert_eq!(anchor_of(scaled), Some(a1), "adding a depth_by keeps it");
    rig.routes.retain(|(k, _)| k.0 != 201);
    let c3 = rig.compile(Some(&c2), now(3, 700_000));
    let plain = layer_clock(&c3, target(1), 100, None);
    assert_eq!(anchor_of(plain), Some(a1), "removing it keeps it");
    let t = time(0.0, 1e6, 3);
    assert_eq!(
        plain.cycles(&t),
        layer_clock(&c1, target(1), 100, None).cycles(&t)
    );
}

/// A second rate edit while the first's anchor is pending but nearer than
/// `ANCHOR_REUSE_MIN` re-anchors at `now + RATE_ANCHOR_MARGIN` rather than
/// reuse a frame the publish may miss. The new clock agrees with the old
/// one from the old anchor on; before it, the step a lane switching at `s`
/// takes is exactly the documented `(R1 − R0) · (a − s)`. (The far branch,
/// reusing the anchor's frame, is `an_anchor_is_carried_until_a_play_start`.)
///
/// Mutations (run), each fails: the reuse bound dropped (`a.at >
/// now.play`, the PR's first rule) → the anchor stays at the old frame;
/// `prev_rate` the old clock's `prev_rate` (R0) → the clocks part after
/// the old anchor.
#[test]
fn a_second_rate_edit_near_its_anchor_reanchors() {
    let mut rig = Rig::new();
    let c0 = rig.compile(None, now(7, 0));
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(5.0));
    let c1 = rig.compile(Some(&c0), now(7, 10_000));
    let old = layer_clock(&c1, target(1), 100, None);
    let a1 = anchor_of(old).expect("anchored");
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(3.0));
    let edit = now(7, 20_000);
    assert!(a1.at.since(edit.play).expect("ahead") < ANCHOR_REUSE_MIN);
    let c2 = rig.compile(Some(&c1), edit);
    let new = layer_clock(&c2, target(1), 100, None);
    let a2 = anchor_of(new).expect("anchored");
    assert_eq!(
        (a2.at, a2.prev_rate),
        (edit.after(RATE_ANCHOR_MARGIN).play, Hz(5.0))
    );
    let cyc = |c: Clock, p: f64| c.cycles(&time(0.0, p, 7)).expect("a phase");
    let a = a1.at.get() as f64;
    let b = a2.at.get() as f64;
    for p in [a, a + 1000.0, b - 1.0, b] {
        assert!((cyc(new, p) - cyc(old, p)).abs() < 1e-9, "agrees at {p}");
    }
    for p in [20_000.0, 21_000.0, a - 1.0] {
        let step = (5.0 - 2.0) * (a - p) / SR.get();
        assert!(
            ((cyc(old, p) - cyc(new, p)) - step).abs() < 1e-9,
            "the documented step at {p}"
        );
    }
}

/// A value that is not finite is refused where it would reach the lane: an
/// infinite route depth (`inf · 0` is a NaN in the value rule), an
/// infinite constant wave, and a clock's infinite rate, period or phase,
/// on an LFO no route reads (so the source check refuses it, not a
/// program). An infinite phase after a rate edit would otherwise carry the
/// live anchor with a jump of `inf − inf`, a NaN in `Compiled::clocks`
/// that breaks `Compiled: Eq`.
///
/// Mutations (run), each fails: the depth check back to `is_nan` → the
/// infinite depth compiles; the constant check back to `is_nan` → the
/// infinite constant compiles; the seconds clock's check back to `is_nan`
/// → the infinite rate and phase compile (and the anchored LFO's clock
/// holds a NaN phase); the beat clock's check back to `is_nan` → the
/// infinite period and phase compile.
#[test]
fn a_value_that_is_not_finite_is_refused() {
    let t1 = [(target(1), ranged(0.0, 1.0))];
    let one_lfo = [(SourceKey(1), lfo(LfoShape::Sine, beats(1.0)))];
    for depth in [f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
        assert_eq!(
            run(&one_lfo, &[(RouteKey(1), route(1, to(1), depth))], &t1),
            Err(CompileError::BadRoute(RouteKey(1))),
            "depth {depth}"
        );
    }
    for v in [f32::INFINITY, f32::NAN] {
        let constant = [(
            SourceKey(1),
            Source::Lfo {
                wave: Wave::Const(v),
                clock: beats(1.0),
            },
        )];
        assert_eq!(
            run(&constant, &[(RouteKey(1), route(1, to(1), 0.5))], &t1),
            Err(CompileError::BadSource(SourceKey(1))),
            "constant {v}"
        );
    }
    let clock = |rate: f32, phase: f32| Clock::Seconds {
        rate: Hz(rate),
        phase: PhaseIncrement(phase),
        anchor: None,
    };
    let beat = |per_cycle: f64, phase: f32| Clock::Beats {
        per_cycle: BeatDuration(per_cycle),
        phase: PhaseIncrement(phase),
    };
    for c in [
        clock(f32::INFINITY, 0.0),
        clock(1.0, f32::INFINITY),
        clock(1.0, f32::NEG_INFINITY),
        beat(f64::INFINITY, 0.0),
        beat(1.0, f32::INFINITY),
    ] {
        let unrouted = [(SourceKey(1), lfo(LfoShape::Sine, c))];
        assert_eq!(
            run(&unrouted, &[], &t1),
            Err(CompileError::BadSource(SourceKey(1))),
            "{c:?}"
        );
    }
    // The reviewer's sequence: a rate edit (a live anchor), then an
    // infinite phase on the unrouted LFO.
    let at = |rate, phase, prev: Option<&Compiled>, play| {
        compile(
            &Declarations {
                sources: &[(SourceKey(10), lfo(LfoShape::Sine, clock(rate, phase)))],
                routes: &[],
                targets: &t1,
            },
            prev,
            now(3, play),
            SR,
        )
    };
    let c0 = at(1.0, 0.0, None, 0).expect("compiles");
    let c1 = at(2.0, 0.0, Some(&c0), 1000).expect("compiles");
    assert!(anchor_of(c1.clocks[&SourceKey(10)]).is_some());
    assert_eq!(
        at(2.0, f32::INFINITY, Some(&c1), 2000),
        Err(CompileError::BadSource(SourceKey(10)))
    );
}

/// A depth route's scaler reads its own LFO's dated clock, not the scaled
/// LFO's: with a seconds-LFO scaler at another rate than the scaled one,
/// the layer's `DepthBy` clock is `clocks[scaler]`, before and after a
/// rate edit of the scaler (which anchors it).
///
/// Mutation (run): the scaler's clock taken from the scaled route's source
/// (`dated(route.source).0`, the wave still the scaler's) → the `DepthBy`
/// clock is the 2 Hz LFO's → fails.
#[test]
fn a_depth_by_scaler_reads_its_own_lfos_clock() {
    let mut rig = Rig::new();
    rig.sources
        .push((SourceKey(20), lfo(LfoShape::Sine, seconds(3.0))));
    rig.routes.push((
        RouteKey(201),
        route(20, RouteTarget::Depth(RouteKey(100)), 1.0),
    ));
    let depth_clock = |c: &Compiled| {
        let id = layer_id(RouteKey(100), Some(RouteKey(201)));
        c.programs[&target(1)]
            .layers()
            .iter()
            .find(|l| l.id == id)
            .expect("the layer")
            .depth_by
            .as_ref()
            .expect("scaled")
            .clock
    };
    let c0 = rig.compile(None, now(7, 0));
    assert_eq!(depth_clock(&c0), c0.clocks[&SourceKey(20)]);
    assert_eq!(depth_clock(&c0), seconds(3.0));
    *rig.source_mut(20) = lfo(LfoShape::Sine, seconds(7.0));
    let c1 = rig.compile(Some(&c0), now(7, 10_000));
    assert!(anchor_of(c1.clocks[&SourceKey(20)]).is_some(), "anchored");
    assert_eq!(depth_clock(&c1), c1.clocks[&SourceKey(20)]);
    assert_eq!(
        layer_clock(&c1, target(1), 100, Some(201)),
        c1.clocks[&SourceKey(10)]
    );
}
