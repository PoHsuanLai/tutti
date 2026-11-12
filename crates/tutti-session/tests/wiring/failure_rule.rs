//! The one failure rule (doc 013, "The session graph's surface"): a call that
//! names a key or port that does not exist fails at the call with a named
//! error, never later at the commit and never as silence.

use tutti_graph::ForkByClone;
use tutti_graph::{CrossfadeCurve, GraphInvalid, NodeKey, ParamRange};
use tutti_nodes::testing::{Const, Through};
use tutti_session::{
    ConnectError, Crossfade, CrossfadeRefused, EventInPort, EventOutPort, InPort, OutPort, Root,
    Session,
};
use tutti_types::{ChannelLayout, SampleRate, Seconds, UnitParam};

const RATE: SampleRate = SampleRate(48_000.0);
const GHOST: NodeKey = NodeKey(u64::MAX);

fn detached() -> Session {
    Session::builder()
        .outputs_floor(1)
        .build_detached(RATE)
        .expect("builds")
}

/// **Every query names the node that is not there.** A key no node has is an
/// error carrying that key, where a count of 0, a latency of 0, `Active` or
/// `Mix::WET` would have read as an answer.
#[test]
fn a_query_of_a_node_that_is_not_there_names_it() {
    let s = detached();
    let g = s.graph_ref();
    let named = |e: tutti_session::NoSuchNode| e.node();
    assert_eq!(named(g.node_inputs(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.node_outputs(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.node_event_inputs(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.node_latency(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.node_tail(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.buses(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.mode(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.mix(GHOST).unwrap_err()), GHOST);
    assert_eq!(
        named(g.declares_param(GHOST, UnitParam::Cutoff).unwrap_err()),
        GHOST
    );
    assert_eq!(
        named(g.param_mod(GHOST, UnitParam::Cutoff).unwrap_err()),
        GHOST
    );
}

/// **A node that is there answers.** The same queries on a real node are `Ok`,
/// so the refusal above is the missing key and nothing else.
#[test]
fn a_query_of_a_node_that_is_there_answers() {
    let mut s = detached();
    let (through, _) = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    let g = s.graph_ref();
    assert_eq!(g.node_inputs(through).unwrap(), 2);
    assert_eq!(g.node_outputs(through).unwrap(), 2);
    assert_eq!(g.node_event_inputs(through).unwrap(), 0);
    assert!(g.buses(through).is_ok());
    assert!(g.mode(through).is_ok());
    assert!(g.mix(through).is_ok());
    assert_eq!(g.param_mod(through, UnitParam::Cutoff).unwrap(), None);
}

/// **A wire taken out names a node that is not there, and says when an edge
/// is merely absent.** `Ok(false)` is "no such edge", and an error is "no such
/// node or port": a typo in a key is not a disconnect that found nothing.
#[test]
fn disconnect_and_sources_refuse_a_node_that_is_not_there() {
    let mut s = detached();
    let (a, _) = s.graph().add(ForkByClone(Const::frame(&[0.5])));
    let g = s.graph();

    let ghost_out = OutPort::new(GHOST, 0);
    let ghost_in = InPort::new(GHOST, 0);
    assert_eq!(
        g.disconnect(ghost_out, Root::output(0)),
        Err(ConnectError::NoSuchNode(GHOST))
    );
    assert_eq!(
        g.disconnect_hard(a.out(0), ghost_in),
        Err(ConnectError::NoSuchNode(GHOST))
    );
    assert_eq!(
        g.disconnect_all(ghost_in),
        Err(ConnectError::NoSuchNode(GHOST))
    );
    assert_eq!(g.sources(ghost_in), Err(ConnectError::NoSuchNode(GHOST)));
    assert_eq!(
        g.disconnect(a.events_out(), EventInPort::new(GHOST, 0)),
        Err(ConnectError::NoSuchEventOut(a.events_out())),
        "the source side is checked first: a constant has no event output"
    );
    assert_eq!(
        g.sources(EventInPort::new(GHOST, 0)),
        Err(ConnectError::NoSuchNode(GHOST))
    );
    assert_eq!(
        g.disconnect(EventOutPort::new(GHOST, 0), EventInPort::new(a.erase(), 0)),
        Err(ConnectError::NoSuchNode(GHOST))
    );
    assert_eq!(
        g.bus_sources(tutti_session::BusRef {
            node: GHOST,
            bus: 0,
            band: 0
        }),
        Err(ConnectError::NoSuchNode(GHOST))
    );

    // Real ends with no edge between them: absent, not an error.
    assert_eq!(g.disconnect(a.out(0), Root::output(0)), Ok(false));
    assert_eq!(g.disconnect_all(Root::output(0)), Ok(false));
    assert_eq!(g.sources(Root::output(0)), Ok(Vec::new()));
    // A port a real node does not have is a named error too.
    assert_eq!(
        g.sources(InPort::new(a.erase(), 9)),
        Err(ConnectError::NoSuchInPort(InPort::new(a.erase(), 9)))
    );
}

/// **A modulation naming a node that is not there is refused at the call**
/// (`GraphInvalid::UnknownParamNode`, what the commit would have said), and the
/// graph is left alone: not marked edited, nothing stored.
#[test]
fn a_param_modulation_of_a_missing_node_is_refused_at_the_call() {
    let mut s = detached();
    let (through, _) = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    let _ = s.update();
    assert!(!s.graph_ref().is_edited());
    let range = ParamRange { min: 0.0, max: 1.0 };
    let refused = s
        .graph()
        .set_param_mod(GHOST, UnitParam::Cutoff, None, range)
        .unwrap_err();
    assert!(
        matches!(refused, GraphInvalid::UnknownParamNode { missing, .. } if missing == GHOST),
        "{refused:?}"
    );
    let refused = s
        .graph()
        .set_param_mod(through, UnitParam::Cutoff, Some(GHOST), range)
        .unwrap_err();
    assert!(
        matches!(refused, GraphInvalid::UnknownParamNode { missing, .. } if missing == GHOST),
        "{refused:?}"
    );
    assert!(!s.graph_ref().is_edited(), "a refusal changes nothing");
    assert_eq!(
        s.graph()
            .clear_param_mod(GHOST, UnitParam::Cutoff)
            .unwrap_err()
            .node(),
        GHOST
    );
}

/// **A crossfade of a node that is not there is refused at the call even while
/// the graph re-prepares.** Asked `Busy` first, it would have been parked to
/// fail on a later update; it is `NoSuchNode` now and nothing waits.
#[test]
fn a_crossfade_of_a_missing_node_is_refused_during_a_re_prepare() {
    let mut s = detached();
    let _side = s.graph().take_audio_side();
    let (knob, _) = s.graph().add(ForkByClone(Const::frame(&[1.0])));
    let _ = s.update();
    s.graph().set_sample_rate(SampleRate(44_100.0));
    assert!(s.graph_ref().testing().is_repreparing(), "mid re-prepare");

    let refused = s
        .graph()
        .crossfade(
            GHOST,
            ForkByClone(Const::frame(&[2.0])),
            Seconds(0.005),
            CrossfadeCurve::EqualAmplitude,
        )
        .expect_err("a node that is not there");
    assert_eq!(refused, CrossfadeRefused::NoSuchNode(GHOST));
    assert_eq!(s.graph_ref().testing().waiting_crossfades(), 0);

    // The node that is there waits, as before.
    let waits = s.graph().crossfade(
        knob,
        ForkByClone(Const::frame(&[2.0])),
        Seconds(0.005),
        CrossfadeCurve::EqualAmplitude,
    );
    assert!(matches!(waits, Ok(Crossfade::Waiting(_))), "{waits:?}");
}

/// **A tap or meter source whose port index no `u16` holds is named, not
/// dropped.** Dropped, `open` would have opened a tap with fewer lanes than
/// the caller asked for.
#[test]
fn a_tap_source_past_what_a_port_index_holds_is_refused() {
    use tutti_graph::{MissingSource, TapAlign};
    use tutti_types::Samples;
    let mut s = detached();
    let (a, _) = s.graph().add(ForkByClone(Const::frame(&[0.5])));
    let far = OutPort::new(a.erase(), 70_000);
    let refused = s
        .graph()
        .taps()
        .open(&[a.out(0), far], TapAlign::Raw, Samples(64))
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(refused, MissingSource::Port { node, .. } if node == a.erase()));
    let refused = s
        .graph()
        .meters()
        .open(&[far], TapAlign::Raw)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(refused, MissingSource::Port { .. }));
}
