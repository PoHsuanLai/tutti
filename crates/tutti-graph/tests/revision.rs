//! `Editor::revision`: it moves on every call that can change what a reader
//! of the editor sees (the spec, shapes, generations, the plan sent last,
//! param ranges and their cells, the `Prepare`), and stays put across
//! reads, a `collect` with nothing to resume and a commit with nothing to
//! send. The modulation reconcile skips its work on an unchanged revision,
//! so a call that changes the graph without moving it would leave the
//! reconcile behind (a lane on a removed node, a range cell never fetched).

use tutti_graph::NodeKey;
use tutti_graph::{
    CrossfadeCurve, Cx, Editor, Fade, Io, Node, ParamIn, ParamRange, Prepare, Shape, Status,
    Unforkable,
};
use tutti_graph::{OutPort, Source};
use tutti_types::{ChannelLayout, Latency, SampleRate, Samples, UnitParam};

struct Out;

impl Node for Out {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn prep(max: usize) -> Prepare {
    Prepare::new(SampleRate(48_000.0), Samples(max))
}

/// Runs `f` and asserts it moved the revision.
fn moves(ed: &mut Editor, what: &str, f: impl FnOnce(&mut Editor)) {
    let before = ed.revision();
    f(ed);
    assert_ne!(ed.revision(), before, "{what} must move the revision");
}

/// Runs `f` and asserts it left the revision where it was.
fn stays(ed: &mut Editor, what: &str, f: impl FnOnce(&mut Editor)) {
    let before = ed.revision();
    f(ed);
    assert_eq!(ed.revision(), before, "{what} must leave the revision");
}

/// Every editing call moves the revision; reads and no-op housekeeping do
/// not.
///
/// Mutations (run), each fails here: drop the `touch` from `place` (the
/// insert and the replace), `set_latency`, `set_param_range`, `remove`,
/// `reprepare`, `send` (the commit, and the resume's send in `collect`) or
/// `spec_mut`; add one to `collect` (fails "collect with nothing to
/// resume"). Dropping `resume`'s own `touch` alone passes: the resume
/// sends, and `send` moves it; that `touch` covers a resume that poisons
/// the editor instead, which this test does not reach.
#[test]
fn every_edit_moves_the_revision_and_nothing_else_does() {
    let (mut ed, mut exec) = Editor::new(prep(512));
    let one = NodeKey(1);
    moves(&mut ed, "insert", |ed| {
        ed.insert(one, "out", Unforkable(Out));
    });
    moves(&mut ed, "spec_mut", |ed| {
        ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: one, port: 0 })];
    });
    moves(&mut ed, "a commit that sends", |ed| {
        ed.commit().expect("commits");
    });
    exec.apply_pending();
    stays(&mut ed, "a commit with nothing to send", |ed| {
        ed.commit().expect("commits nothing");
    });
    stays(&mut ed, "collect with nothing to resume", |ed| {
        ed.collect();
    });
    stays(&mut ed, "reads", |ed| {
        let _ = (ed.spec(), ed.shapes(), ed.base(), ed.prepare());
        let _ = ed.play_reading();
    });
    moves(&mut ed, "set_param_range", |ed| {
        ed.set_param_range(
            ParamIn {
                node: one,
                param: UnitParam::Cutoff.into(),
            },
            ParamRange::new(0.0, 1.0),
        )
        .expect("a node");
    });
    moves(&mut ed, "set_latency", |ed| {
        ed.set_latency(one, Latency::new(Samples(4)))
            .expect("a node");
    });
    moves(&mut ed, "replace", |ed| {
        ed.replace(
            one,
            Unforkable(Out),
            Fade::new(Samples(64), CrossfadeCurve::EqualPower),
        )
        .expect("replaces");
    });
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    moves(&mut ed, "reprepare", |ed| {
        ed.reprepare(prep(1024)).expect("re-prepares");
    });
    // The executor hands the units back; the next collect resumes.
    exec.apply_pending();
    moves(&mut ed, "the collect that resumes a re-prepare", |ed| {
        ed.collect();
    });
    assert!(!ed.is_repreparing());
    moves(&mut ed, "remove", |ed| ed.remove(one));
}
