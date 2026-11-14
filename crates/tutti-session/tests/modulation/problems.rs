//! One place to ask why nothing modulates (doc 030, T5): `ModState::problems`
//! lists a `Problem` for each refusal, unresolved target and unfed follower,
//! and its `Display` names the node and the param.

use std::time::{Duration, Instant};

use tutti_graph::NodeKey;
use tutti_modulation::{Follower, Lfo};
use tutti_session::{
    ModTarget, PHASES, ParamRef, PhaseCx, Problem, RouteSpec, Session, SessionEvent, SourceName,
    TargetName,
};
use tutti_types::{Depth, Hz, MaxBlock, Samples, UnitParam};

fn session() -> Session {
    Session::builder()
        .outputs(tutti_types::ChannelLayout::STEREO)
        .max_block(MaxBlock::of(Samples(256)))
        .build()
        .expect("builds")
}

/// Every phase at `now`: one update, at a time the test picks.
fn update_at(s: &mut Session, now: Instant) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    for &phase in PHASES {
        s.run_phase(phase, PhaseCx::at(now), &mut out);
    }
    out
}

/// **A target no node answers is a problem that says which param of which
/// node**, and a follower of a node that is not there is another that names
/// the node. They list in the order the session found them (targets first),
/// and the event the warning raises is the same `Problem`.
#[test]
fn an_unresolved_target_and_an_unfed_follower_are_named() {
    let mut s = session();
    let ghost = ParamRef::unit(NodeKey(999), UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(ghost, 0.0, 1_000.0).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz(2.0)));
    m.route(RouteSpec::new(lfo, ghost, Depth::of(0.5))).unwrap();
    let follower = m.follower(Follower::default(), NodeKey(404));
    m.route(RouteSpec::new(follower, ghost, Depth::of(0.5)))
        .unwrap();

    let t0 = Instant::now();
    let _ = update_at(&mut s, t0);
    let problems: Vec<Problem> = s.mod_state().problems().cloned().collect();
    assert_eq!(
        problems,
        [
            Problem::Unresolved {
                target: TargetName::Own(ModTarget::Param(ghost)),
                waited: Duration::ZERO,
            },
            Problem::Unfed {
                source: SourceName::Own {
                    id: follower,
                    fed_by: Some(NodeKey(404)),
                },
            },
        ]
    );
    let texts: Vec<String> = problems.iter().map(ToString::to_string).collect();
    assert!(
        texts[0].contains("Cutoff of node 999"),
        "names the param and its node: {}",
        texts[0]
    );
    assert!(
        texts[1].contains("node 404"),
        "names the node the follower waits on: {}",
        texts[1]
    );

    // Past the warning time the same problem is the event.
    let events = update_at(&mut s, t0 + Duration::from_secs(5));
    let reported: Vec<&Problem> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Modulation(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(
        reported,
        [&Problem::Unresolved {
            target: TargetName::Own(ModTarget::Param(ghost)),
            waited: Duration::from_secs(5),
        }],
        "reported once, as the problem the list holds"
    );
}

/// **Nothing is a problem while everything applies**: no declarations, and
/// declarations on a node that is there once committed.
#[test]
fn no_problems_while_the_declarations_apply() {
    let mut s = session();
    assert_eq!(s.mod_state().problems().count(), 0);
    let (lp, _) = s.graph().add(tutti_nodes::SvfFilterNode::<f32>::new(
        tutti_nodes::SvfType::LowPass,
        Hz(1_000.0),
        tutti_types::Q(0.7),
    ));
    let mut m = s.mods_mut();
    m.range(lp.cutoff(), Hz(100.0)..Hz(4_000.0)).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz(2.0)));
    m.route(RouteSpec::new(lfo, lp.cutoff(), Depth::of(0.5)))
        .unwrap();
    for _ in 0..4 {
        let _ = s.update();
    }
    assert_eq!(s.mod_state().problems().next(), None);
    assert!(
        s.mod_state()
            .lane(ParamRef::from(lp.cutoff()).key())
            .is_some()
    );
}
