//! The compensation figures a session publishes (the pre-roll table and the
//! total latency): what a commit sends is what is published, a clean update
//! publishes nothing, a re-prepare republishes once it resumes, and the
//! compiler compensates without splicing anything into the wiring.
//!
//! These are the session halves of the tests in `bevy-tutti`'s
//! `graph::latency` (`GraphLatency` and `ChannelCompensation` there were
//! mirrors of the session's `latency()` and `compensation()`; the mirroring
//! stayed with the adapter), moved here when the adapter was parked. The
//! total and the table of a latent branch are also pinned, to the sample, by
//! `graph.rs`'s `a_latent_branch_is_compensated_on_the_other_to_the_sample`,
//! and a restart's republication by `device.rs`'s
//! `a_rerate_keeps_the_published_compensation_in_step_with_the_plan`.

use tutti_nodes::LimiterNode;
use tutti_nodes::testing::Const;
use tutti_session::{InPort, OutPort, Root, Session};
use tutti_types::{ChannelLayout, Db, SampleRate, Samples};

const RATE: SampleRate = SampleRate(44_100.0);

fn detached() -> Session {
    Session::builder()
        .outputs_floor(2)
        .build_detached(RATE)
        .expect("builds")
}

/// A limiter's lookahead is the latency; the one the engine ships.
fn limiter() -> LimiterNode {
    LimiterNode::with_channels(ChannelLayout::MONO, Db(-1.0), Db(-0.3))
}

/// ch0 through a limiter, ch1 dry: ch1's source must pre-roll to match.
/// Returns the limiter's node latency.
fn skewed(s: &mut Session) -> (tutti_graph::NodeKey, Samples) {
    let (a, _) = s.graph().insert(Const::mono(1.0));
    let (eff, _) = s.graph().insert(limiter());
    let (b, _) = s.graph().insert(Const::mono(1.0));
    s.graph()
        .set_sources(InPort::new(eff, 0), [OutPort::new(a, 0)])
        .unwrap();
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(eff, 0)])
        .unwrap();
    s.graph()
        .set_sources(Root::output(1), [OutPort::new(b, 0)])
        .unwrap();
    let lat = s.graph_ref().node_latency(eff);
    // Every test built on this graph compares against `lat`; if the limiter
    // ever reported no latency they would all agree on zero and pass while
    // testing nothing.
    assert!(lat.get() > 0, "LimiterNode must report its lookahead");
    (eff.erase(), lat)
}

/// **A graph where nothing reports latency has no figure to display**: the
/// total is zero and the table has a zero per output.
#[test]
fn a_graph_with_no_latency_reports_none() {
    let mut s = detached();
    let (a, _) = s.graph().insert(Const::mono(1.0));
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(a, 0)])
        .unwrap();
    s.graph()
        .set_sources(Root::output(1), [OutPort::new(a, 0)])
        .unwrap();
    let _ = s.update();

    assert_eq!(s.latency(), Samples(0));
    assert!(
        s.compensation().to_vec().iter().all(|c| *c == Samples(0)),
        "no channel pre-rolls"
    );
}

/// **The published figures are the plan's**: what a commit publishes is
/// exactly the `Plan::compensation` / `total_latency` it sent the executor,
/// so a pre-roll a source outside the graph reads cannot drift off the delay
/// the graph applies. The channel that carries the limiter pre-rolls by zero
/// (the dry one by its lookahead), and the total is the lookahead: the figure
/// a DAW displays is exactly the one the table does not contain. Nothing is
/// spliced in to get there: both outputs still read the nodes they were
/// wired to.
#[test]
fn the_published_figures_are_the_plans_and_nothing_is_spliced_in() {
    let mut s = detached();
    let (_, lat) = skewed(&mut s);
    let authored = [
        s.graph_ref().sources(Root::output(0)),
        s.graph_ref().sources(Root::output(1)),
    ];
    let _ = s.update();
    assert!(!s.graph_ref().is_edited(), "committed");

    let sent = s
        .graph_ref()
        .sent_compensation()
        .expect("the commit sent a plan");
    assert_eq!(
        s.compensation().to_vec(),
        sent.channels(),
        "per-channel pre-roll"
    );
    assert_eq!(s.latency(), sent.total(), "total latency");
    assert_eq!(sent.total(), lat, "and it is the limiter's lookahead");
    assert_eq!(
        s.compensation().to_vec().first().copied(),
        Some(Samples(0)),
        "the channel that defines the latency pre-rolls by zero"
    );
    assert_eq!(
        s.compensation().to_vec().get(1).copied(),
        Some(lat),
        "the dry channel pre-rolls by the lookahead"
    );
    assert_eq!(
        [
            s.graph_ref().sources(Root::output(0)),
            s.graph_ref().sources(Root::output(1))
        ],
        authored,
        "nothing is spliced into the wiring"
    );
}

/// **An update with no edit publishes nothing**: the figures go out with a
/// plan, and a clean session sends none. The graph's own inserts edit it, so
/// the first update commits; a sentinel published into the table after that
/// must survive an update with no edit.
#[test]
fn an_update_with_no_edit_publishes_nothing() {
    let mut s = detached();
    skewed(&mut s);
    let _ = s.update();
    assert!(!s.graph_ref().is_edited());
    let sentinel = vec![Samples(99)];
    s.compensation().set(&sentinel);

    let _ = s.update();

    assert_eq!(s.compensation().to_vec(), sentinel, "no table published");
}

/// **The spec's latency fold and the compiled plan agree**, on a latent graph
/// and on one with no latency: the fold (`latency_plan`, what a readout
/// reads) is what the compiled plan compensates by, zeros and all (the fold
/// reports no channels when nothing is latent; the plan a zero per output).
#[test]
fn the_latency_fold_and_the_compiled_plan_agree() {
    fn figures(s: &Session) -> (Vec<Samples>, Samples) {
        let folded = s.graph_ref().latency_plan();
        let planned = s
            .graph_ref()
            .planned_compensation()
            .expect("a plan is compiled");
        let width = folded.channels().len().max(planned.channels().len());
        let of = |c: &tutti_graph::Compensation| -> (Vec<Samples>, Samples) {
            ((0..width).map(|i| c.for_channel(i)).collect(), c.total())
        };
        let (f, p) = (of(&folded), of(&planned));
        assert_eq!(
            f, p,
            "the spec's latency fold and the compiled plan disagree"
        );
        f
    }

    let mut s = detached();
    let (_, lat) = skewed(&mut s);
    let _ = s.update();
    assert_eq!(figures(&s).1, lat);

    let mut s = detached();
    let (a, _) = s.graph().insert(Const::mono(1.0));
    s.graph().connect(a, Root).unwrap();
    let _ = s.update();
    assert_eq!(figures(&s).1, Samples(0));
}

/// **A re-prepare to a new rate republishes the figures once it resumes**,
/// with nothing else marking the graph edited: the limiter's lookahead is a
/// time, so its latency in samples moves with the rate, and the resumed plan
/// compensates by the new figure: the update whose collect resumes the plan
/// publishes its figures.
#[test]
fn a_re_prepare_republishes_the_figures_once_it_resumes() {
    let mut s = detached();
    let (a, _) = s.graph().insert(Const::mono(1.0));
    let (eff, _) = s.graph().insert(limiter());
    s.graph()
        .set_sources(InPort::new(eff, 0), [OutPort::new(a, 0)])
        .unwrap();
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(eff, 0)])
        .unwrap();
    s.graph()
        .set_sources(Root::output(1), [OutPort::new(a, 0)])
        .unwrap();
    let at_44k = s.graph_ref().node_latency(eff);
    let mut side = s.graph().take_audio_side();
    let _ = s.update();
    assert_eq!(s.latency(), at_44k);
    let mut out = [0.0f32; 2];
    side.tick(&[], &mut out);

    s.graph().set_sample_rate(SampleRate(48_000.0));
    for _ in 0..3 {
        side.tick(&[], &mut out);
        let _ = s.update();
    }
    let at_48k = s.graph_ref().node_latency(eff);
    assert_ne!(at_48k, at_44k, "the lookahead moved in samples");
    assert_eq!(s.latency(), at_48k);
    assert_eq!(
        s.compensation().to_vec().get(1),
        Some(&at_48k),
        "the dry channel pre-rolls by the new figure"
    );
}
