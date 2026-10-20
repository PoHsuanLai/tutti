//! Modulation in the session (design 015 §4.4, §4.6, §5.1): the session's
//! own declarations ([`Session::mods`]) and a host's ([`ModInput`]), applied
//! by `Phase::Modulation`, re-applied and resynced by `Phase::AfterCommit`.
//!
//! The render tests stand on analytic figures (doc 014 §5.3's bound for a
//! sine on the control grid, a follower's closed form), never on a
//! reference render. The probe nodes are `tests/common/probe.rs`'s.

mod common;

use std::cell::Cell;
use std::time::{Duration, Instant};

use tutti_core::{AudioNode, ChannelLayout, InterleavedMut, MotionEvent, SampleRate, Samples};
use tutti_graph::ForkByClone;
use tutti_mod::{
    Declarations, Follower, Lfo, RouteDecl, RouteKey, RouteTarget, Source, SourceKey, TargetDecl,
    TargetKey,
};
use tutti_nodes::{Feed, ReconcileError};
use tutti_session::{
    GraphSource, ModInput, ModKeys, ModRefusal, ParamRef, PhaseCx, Session, SessionEvent, PHASES,
};
use tutti_types::{BeatDuration, Depth, Hz, NodeKey, ParamAddr, UnitParam};

use common::probe::{ProbeUnit, Pulse};

const RATE: f64 = 48_000.0;
/// A lane's control step (`tutti_nodes::automation::CONTROL_STEP`).
const STEP: usize = 32;
/// A port's connect crossfade (`PARAM_DECLICK`).
const DECLICK: usize = 256;

/// An offline session at 48 kHz, stereo.
fn offline() -> Session {
    Session::builder()
        .build_offline(SampleRate(RATE), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds")
}

/// Every phase at `now`, with `mods` the host's declarations (`None`: the
/// session's own): one update, at a time the test picks.
fn update_at(s: &mut Session, now: Instant, mods: Option<ModInput<'_>>) -> Vec<SessionEvent> {
    let mut cx = PhaseCx::at(now);
    if let Some(mods) = mods {
        cx = cx.with_mods(mods);
    }
    let mut out = Vec::new();
    for &phase in PHASES {
        s.run_phase(phase, cx, &mut out);
    }
    out
}

/// Updates until nothing is left to resolve: nodes committed, then lanes.
fn settle(s: &mut Session) {
    for _ in 0..4 {
        s.update();
    }
    assert_eq!(s.mod_state().refusal(), None, "the declarations apply");
    assert_eq!(
        s.mod_state().unresolved_targets(),
        &[],
        "every target resolved"
    );
}

/// `500 + 1000 · depth · sin(2π · rate · t)` at frame `f`: a bipolar linear
/// sine route on a `0..=1000` target with knob 500.
fn lfo_value(depth: f64, rate: f64, f: usize) -> f64 {
    500.0 + 1000.0 * depth * (std::f64::consts::TAU * rate * f as f64 / RATE).sin()
}

/// Doc 014 §5.3's bound for a linear sine at `rate` on the 32-frame grid,
/// times the swing, plus `f32` rounding of values near 1000.
fn lfo_tolerance(depth: f64, rate: f64) -> f64 {
    let h = std::f64::consts::TAU * rate * STEP as f64 / RATE;
    h * h / 8.0 * 1000.0 * depth + 1e-3
}

/// A follower's analytic level (attack `ta`, release `tr`) of a 0.8 pulse
/// on frames `4800..7200`, one control step late and linear between grid
/// points (doc 014 §6), ducking a `0..=1` param with knob 0.8 at depth −0.6.
fn ducked(f: usize, ta: f64, tr: f64) -> f64 {
    let pole = |tau: f64| (-1.0 / (tau * RATE)).exp();
    let (ca, cr) = (pole(ta), pole(tr));
    let env = |g: usize| -> f64 {
        if g <= 4_800 {
            0.0
        } else if g <= 7_200 {
            0.8 * (1.0 - ca.powi(g as i32 - 4_800))
        } else {
            0.8 * (1.0 - ca.powi(2_400)) * cr.powi(g as i32 - 7_200)
        }
    };
    let g = f / STEP * STEP;
    let x = if g < STEP {
        0.0
    } else {
        let (a, b) = (env(g - STEP), env(g));
        a + (b - a) * (f - g) as f64 / STEP as f64
    };
    0.8 - 0.6 * x
}

/// **Doc 014 §8.1, headless (design 015 §5.1), renders `bevy-tutti`'s
/// `the_8_1_example_renders_its_analytic_values` figures on an offline
/// session.** A sine LFO of one cycle per half beat on a cutoff (knob 500
/// in `0..=1000`, depth 0.3) renders the analytic sine, bar-locked at 120
/// BPM (4 Hz), within §5.3's bound past the connect's declick; and the pad
/// (knob 0.8 in `0..=1`) dips with the kick through a follower (5 ms /
/// 100 ms) at depth −0.6, analytically.
///
/// Mutations (run), each fails:
/// - `OwnKeys::feed` answering `None` → the follower is unresolved, the
///   pad never dips (`frame 4832: 0.8 vs 0.79…`);
/// - `ModSession::modulation_phase` not applying the session's own
///   declarations (`host.is_some()` alone as the declarer) → no lane, the
///   cutoff reads 500 flat;
/// - `ParamRef::key` keying the target by `!bits` → its node never
///   resolves (`settle`: "every target resolved").
#[test]
fn the_8_1_example_renders_its_analytic_values() {
    let mut s = offline();
    let (filter, _) = s.graph().add(ProbeUnit::new(500.0));
    let (pad, _) = s.graph().add(ProbeUnit::of(UnitParam::Volume, 0.8));
    let (kick_bus, _) = s.graph().add(ForkByClone(Pulse {
        from: 4_800,
        to: 7_200,
        level: 0.8,
    }));
    s.graph().set_output_source(0, GraphSource::Node(filter, 0));
    s.graph().set_output_source(1, GraphSource::Node(pad, 0));

    let cutoff = ParamRef::unit(filter, UnitParam::Cutoff);
    let gain = ParamRef::unit(pad, UnitParam::Volume);
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    m.range(gain, 0.0, 1.0);
    let wobble = m.lfo(Lfo::sine().rate(BeatDuration(0.5)));
    m.route(wobble, cutoff, Depth(0.3));
    let kick = m.follower(Follower::default(), kick_bus);
    m.route(kick, gain, Depth(-0.6)); // sidechain
    settle(&mut s);

    // Rolling from beat 0 at frame 0 (120 BPM, the transport's default).
    s.transport()
        .motion
        .try_send(MotionEvent::Play)
        .expect("room");
    let (total, block) = (24_000, 128);
    let mut all = [Vec::new(), Vec::new()];
    let mut buf = vec![0.0f32; block * 2];
    for _ in 0..total / block {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        for frame in buf.chunks_exact(2) {
            all[0].push(frame[0]);
            all[1].push(frame[1]);
        }
    }
    let tol = lfo_tolerance(0.3, 4.0);
    for (f, &v) in all[0].iter().enumerate().skip(DECLICK) {
        let want = lfo_value(0.3, 4.0, f);
        assert!(
            (f64::from(v) - want).abs() < tol,
            "cutoff, frame {f}: {v} vs {want}"
        );
    }
    for (f, &v) in all[1].iter().enumerate() {
        let want = ducked(f, 0.005, 0.1);
        assert!(
            (f64::from(v) - want).abs() < 1e-4,
            "pad, frame {f}: {v} vs {want}"
        );
    }
    let low = all[1].iter().copied().fold(f32::INFINITY, f32::min);
    assert!(low < 0.4, "the pad dips with the kick: {low}");
}

/// A host's keys: target key 1 names `node` once it is set; source 1 has
/// no feed (an LFO).
struct HostKeys {
    node: Cell<Option<AudioNode>>,
}

impl ModKeys for HostKeys {
    fn node(&self, target: u64) -> Option<AudioNode> {
        (target == 1).then(|| self.node.get()).flatten()
    }

    fn plugin_value(&self, _: u64, _: u32) -> Option<Option<f32>> {
        Some(None)
    }

    fn feed(&self, _: SourceKey) -> Option<Feed> {
        None
    }
}

/// The host's §8.1 cutoff wobble, keyed by its own ids: source 1, route 2,
/// target `(1, Cutoff)` over `0..=1000`.
struct HostDecl {
    sources: Vec<(SourceKey, Source)>,
    routes: Vec<(RouteKey, RouteDecl)>,
    targets: Vec<(TargetKey, TargetDecl)>,
}

const HOST_TARGET: TargetKey = TargetKey {
    node: 1,
    param: ParamAddr::Unit(UnitParam::Cutoff),
};

impl HostDecl {
    fn new(range: bool) -> Self {
        Self {
            sources: vec![(SourceKey(1), Lfo::sine().hz(Hz(3.0)).into())],
            routes: vec![(
                RouteKey(2),
                RouteDecl {
                    source: SourceKey(1),
                    to: RouteTarget::Param(HOST_TARGET),
                    depth: Depth(0.3),
                    polarity: tutti_mod::Polarity::Bipolar,
                    curve: tutti_mod::CurveType::Linear,
                },
            )],
            targets: if range {
                vec![(
                    HOST_TARGET,
                    TargetDecl {
                        min: 0.0,
                        max: 1_000.0,
                        automation: None,
                    },
                )]
            } else {
                Vec::new()
            },
        }
    }

    fn input<'a>(&'a self, keys: &'a HostKeys) -> ModInput<'a> {
        ModInput {
            declarations: Declarations {
                sources: &self.sources,
                routes: &self.routes,
                targets: &self.targets,
            },
            keys,
        }
    }
}

/// **A route to a target placed one update later resolves on that
/// update** (the re-apply loop, doc 014 §8.2). On the first update the
/// host's target key names no node. On the second it names a node inserted
/// in that update: the apply finds it uncommitted, the commit places it,
/// and `AfterCommit` re-applies and commits the lane, so after that one
/// update nothing is unresolved and the lane exists.
///
/// Mutation (run): `ModSession::reapply` returning `false` → the target is
/// still unresolved after the second update (it resolves on the third) →
/// fails.
#[test]
fn a_target_placed_one_update_later_resolves_on_that_update() {
    let mut s = offline();
    let keys = HostKeys {
        node: Cell::new(None),
    };
    let decl = HostDecl::new(true);
    let now = Instant::now();

    update_at(&mut s, now, Some(decl.input(&keys)));
    assert_eq!(s.mod_state().unresolved_targets(), &[HOST_TARGET]);
    assert!(s.mod_state().lane(HOST_TARGET).is_none());

    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    keys.node.set(Some(probe));
    update_at(&mut s, now, Some(decl.input(&keys)));
    assert_eq!(
        s.mod_state().unresolved_targets(),
        &[],
        "resolved on the update its node was placed"
    );
    assert!(s.mod_state().lane(HOST_TARGET).is_some(), "with its lane");
    assert!(!s.graph_ref().is_edited(), "and the lane committed");
}

/// **The unresolved warning fires after 4 s of `now`, not after N
/// updates.** A target whose key names no node stays unresolved: a
/// thousand updates at one instant report nothing, nor does one at 3.9 s;
/// the one at 4 s reports it, once, and later ones do not repeat it.
///
/// Mutations (run), each fails:
/// - the warning not measured on `now` (`waited` set to the threshold, an
///   update-count rule with a count of one) → the first update reports
///   it;
/// - `>=` as `>` on the threshold → nothing at exactly 4 s;
/// - the `warned` flag dropped → every later update repeats it.
#[test]
fn the_unresolved_warning_fires_after_four_seconds_not_n_updates() {
    let mut s = offline();
    let keys = HostKeys {
        node: Cell::new(None),
    };
    let decl = HostDecl::new(true);
    let t0 = Instant::now();
    let warnings = |events: Vec<SessionEvent>| -> Vec<(TargetKey, Duration)> {
        events
            .into_iter()
            .filter_map(|e| match e {
                SessionEvent::ModulationUnresolved { target, waited } => Some((target, waited)),
                _ => None,
            })
            .collect()
    };

    for _ in 0..1_000 {
        assert_eq!(
            warnings(update_at(&mut s, t0, Some(decl.input(&keys)))),
            vec![]
        );
    }
    let early = t0 + Duration::from_millis(3_900);
    assert_eq!(
        warnings(update_at(&mut s, early, Some(decl.input(&keys)))),
        vec![]
    );
    let due = t0 + Duration::from_secs(4);
    assert_eq!(
        warnings(update_at(&mut s, due, Some(decl.input(&keys)))),
        vec![(HOST_TARGET, Duration::from_secs(4))],
        "reported once it has waited 4 s"
    );
    for later in 1..10 {
        let at = due + Duration::from_secs(later);
        assert_eq!(
            warnings(update_at(&mut s, at, Some(decl.input(&keys)))),
            vec![],
            "and once only"
        );
    }
}

/// **A refusal names the host's own keys** (design 015 §4.7): a route to
/// a target with no range is refused by the compile, and the refusal names
/// the `RouteKey` `s.mods()` returned. Reported once, held while it lasts,
/// and cleared when the range arrives.
///
/// Mutation (run): `ModRefusal::route` answering `None` for
/// `UndeclaredTarget` → fails.
#[test]
fn a_refusal_names_the_route_key_the_session_minted() {
    let mut s = offline();
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods();
    let lfo = m.lfo(Lfo::sine().hz(Hz(2.0)));
    let route = m.route(lfo, cutoff, Depth(0.5));

    let refused: Vec<ModRefusal> = s
        .update()
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::ModulationRefused(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(refused.len(), 1, "reported once: {refused:?}");
    assert_eq!(refused[0].route(), Some(route), "{}", refused[0]);
    assert!(matches!(
        s.mod_state().refusal(),
        Some(ModRefusal::Apply(ReconcileError::Compile(_)))
    ));
    assert!(
        !s.update()
            .iter()
            .any(|e| matches!(e, SessionEvent::ModulationRefused(_))),
        "not repeated while it lasts"
    );

    s.mods().range(cutoff, 0.0, 1_000.0);
    s.update();
    assert_eq!(s.mod_state().refusal(), None, "cleared once it applies");
}

/// **A session has one declarer.** While `s.mods()` holds declarations, a
/// host's `ModInput` is refused (`ModRefusal::TwoDeclarers`) and applies
/// nothing.
///
/// Mutation (run): `ModSession::declarer` not refusing (applying the
/// host's) → no refusal, and the host's lane is built → fails.
#[test]
fn a_host_and_the_session_cannot_both_declare() {
    let mut s = offline();
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    s.mods().macro_(0.5);
    let keys = HostKeys {
        node: Cell::new(Some(probe)),
    };
    let decl = HostDecl::new(true);
    let events = update_at(&mut s, Instant::now(), Some(decl.input(&keys)));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::ModulationRefused(ModRefusal::TwoDeclarers))),
        "{events:?}"
    );
    assert_eq!(s.mod_state().refusal(), Some(&ModRefusal::TwoDeclarers));
    assert!(s.mod_state().lane(HOST_TARGET).is_none());
}

/// **A removed node takes the session's declarations on it**: its range
/// and the route into it go with it, so nothing is left to refuse or to
/// warn about.
///
/// Mutation (run): `Session::remove` not calling `forget_node` → the
/// target stays declared and unresolved → fails.
#[test]
fn removing_a_node_forgets_its_declarations() {
    let mut s = offline();
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods();
    m.range(cutoff, 0.0, 1_000.0);
    let lfo = m.lfo(Lfo::sine().hz(Hz(2.0)));
    m.route(lfo, cutoff, Depth(0.5));
    settle(&mut s);
    assert!(s.mod_state().lane(cutoff.key()).is_some());

    assert!(s.remove(probe));
    settle(&mut s);
    assert!(s.mod_state().lane(cutoff.key()).is_none(), "its lane went");
}

/// The key a session `ParamRef` names is its node's key bits.
#[test]
fn a_param_ref_round_trips_through_its_key() {
    let node = AudioNode(NodeKey(42));
    let p = ParamRef::unit(node, UnitParam::Cutoff);
    assert_eq!(p.key().node, 42);
    assert_eq!(ParamRef::from_key(p.key()), p);
}
