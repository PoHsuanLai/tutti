//! Indexed param addresses (doc 013, "Param addresses"): a node with two
//! params of one kind, `Threshold` at index 0 and at index 1, each its own
//! cell, its own param port and its own modulation target.
//!
//! What is pinned here, each with the mutation it was seen to fail under:
//!
//! - `ParamSet::{set, get, cell}` address each index independently, and a
//!   bare `UnitParam` is index 0;
//! - the builder refuses a duplicate full address and accepts the same kind
//!   at another index; `ParamPorts` does the same;
//! - an event source addressed to index 1 drives port 1 and leaves port 0 on
//!   its base, whichever ports it is connected to;
//! - a fork starts from each index's own value;
//! - the sample-accuracy contract's param row drives index 1.
//!
//! The kind-and-index encoding itself is pinned in `tutti-types`
//! (`unit_param.rs`); the session and Bevy sides in their own suites.

mod common;

use common::prepare;
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, Event, EventOut, ForkMode, ForkTarget, IntoNode, Io, Law, MAX_PARAM_PORTS, Node,
    NodeParts, ParamIn, ParamInput, ParamNode, ParamPorts, ParamPortsError, ParamSegment, ParamSet,
    Prepare, Shape, Status, Transport, param_parts,
};
use tutti_graph::{OutPort, Source};
use tutti_types::{ChannelLayout, Db, Param, Samples, UnitAddr, UnitParam};

/// `Threshold` of band 0 and of band 1: the same kind, two indices.
const BAND0: UnitAddr = UnitAddr::new(UnitParam::Threshold, 0);
const BAND1: UnitAddr = UnitAddr::new(UnitParam::Threshold, 1);

/// Two thresholds. Writes port 0's value to output 0 and port 1's to output
/// 1, each at every frame (the base as it stands, for a port nothing drives).
/// A [`ParamNode`]: its cells are addressed `Threshold@0` and `Threshold@1`.
#[derive(Clone)]
struct Bands {
    thr: [Param<Db>; 2],
}

impl Bands {
    fn new(t0: f32, t1: f32) -> Self {
        Self {
            thr: [Param::new(Db(t0)), Param::new(Db(t1))],
        }
    }
}

impl Node for Bands {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::from_count(2))
            .with_param_addrs(&[BAND0, BAND1])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for k in 0..2 {
            match io.param(k) {
                ParamInput::Base => {
                    let base = self.thr[k].load().0;
                    io.output(k).fill(base);
                }
                ParamInput::Frames(v) => io.output(k).copy_from_slice(v),
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        self.thr.get(port).map(|p| p.load().0)
    }
}

impl ParamNode for Bands {
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(BAND0, self.thr[0].as_atomic())
            .param(BAND1, self.thr[1].as_atomic())
            .build()
    }
    fn fork_fresh(&self) -> Self {
        let mut f = self.clone();
        for p in &mut f.thr {
            p.detach();
        }
        f
    }
}

impl IntoNode for Bands {
    type Controls = ParamSet;
    fn into_parts(self) -> NodeParts<ParamSet> {
        param_parts(self)
    }
}

/// An event source that, at the first frame of every block, steps the
/// threshold of each address in `to` to its value.
#[derive(Clone)]
struct Steps {
    to: Vec<(UnitAddr, f32)>,
}

impl Node for Steps {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(8)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let at = io.offset(0).expect("a block has a frame");
        for &(addr, v) in &self.to {
            let seg = ParamSegment::unit(addr, v, v, Samples(0), Law::Linear);
            let _ = io.event_out(0).push(Event::segment(at, seg));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn out(node: NodeKey, port: u16) -> Source {
    Source::Node(OutPort { node, port })
}

const BANDS: NodeKey = NodeKey(1);
const SRC: NodeKey = NodeKey(2);

fn param(addr: UnitAddr) -> ParamIn {
    ParamIn {
        node: BANDS,
        param: addr,
    }
}

fn src_out() -> EventOut {
    EventOut { node: SRC, port: 0 }
}

/// Renders `frames` of the graph's two outputs in one block.
fn render(ed: &mut Editor, exec: &mut tutti_graph::Executor, frames: usize) -> [Vec<f32>; 2] {
    let mut a = vec![0.0; frames];
    let mut b = vec![0.0; frames];
    exec.process(
        frames,
        &Transport::default(),
        &[],
        &mut [&mut a[..], &mut b[..]],
    );
    ed.collect();
    [a, b]
}

/// A graph of [`Bands`] (thresholds -10 and -20) and a [`Steps`] source
/// stepping `to`; the two bands are its outputs. Nothing is connected yet.
fn graph(to: &[(UnitAddr, f32)]) -> (Editor, tutti_graph::Executor, ParamSet) {
    let (mut ed, exec) = Editor::new(prepare(64));
    let controls = ed.insert(BANDS, "bands", Bands::new(-10.0, -20.0));
    ed.insert(
        SRC,
        "steps",
        tutti_graph::ForkByClone(Steps { to: to.to_vec() }),
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![out(BANDS, 0), out(BANDS, 1)]);
    (ed, exec, controls)
}

/// `set`, `get` and `cell` reach each index's own cell and no other, and a
/// bare `UnitParam` is index 0.
///
/// Mutation (run): in `ParamSet::slot`, compare `s.param.param == param.param`
/// (ignore the index) → the write to index 1 lands on index 0 → fails.
#[test]
fn set_and_get_address_each_index_independently() {
    let b = Bands::new(-10.0, -20.0);
    let set = b.param_set();
    assert_eq!(set.get(BAND0), Some(-10.0));
    assert_eq!(set.get(BAND1), Some(-20.0));

    assert!(set.set(BAND1, -33.0));
    assert_eq!(set.get(BAND1), Some(-33.0));
    assert_eq!(set.get(BAND0), Some(-10.0), "index 0 did not move");
    assert_eq!(b.thr[1].load(), Db(-33.0), "the node's own cell");
    assert_eq!(b.thr[0].load(), Db(-10.0));

    // A bare `UnitParam` is index 0.
    assert!(set.set(UnitParam::Threshold, -6.0));
    assert_eq!(set.get(BAND0), Some(-6.0));
    assert_eq!(set.get(UnitParam::Threshold), Some(-6.0));
    assert_eq!(set.get(BAND1), Some(-33.0));

    // `cell` is the very cell `set` writes.
    let cell = set.cell(BAND1).expect("index 1 has a cell");
    assert!(set.set(BAND1, -40.0));
    assert_eq!(cell.load(std::sync::atomic::Ordering::Acquire), -40.0);
    assert!(!cell.ptr_eq(&set.cell(BAND0).unwrap()));

    // Both addresses are listed, in the order added.
    assert_eq!(set.params().collect::<Vec<_>>(), [BAND0, BAND1]);
}

/// An index the node does not have is refused and writes nothing, even
/// though its kind is there.
///
/// Mutation (run): in `ParamSet::slot`, compare the kind alone and ignore the
/// index (as above) → index 2 reaches a cell → fails.
#[test]
fn an_index_the_node_lacks_is_refused() {
    let b = Bands::new(-10.0, -20.0);
    let set = b.param_set();
    let band2 = UnitAddr::new(UnitParam::Threshold, 2);
    assert!(!set.set(band2, 0.0));
    assert_eq!(set.get(band2), None);
    assert!(set.cell(band2).is_none());
    assert_eq!((b.thr[0].load(), b.thr[1].load()), (Db(-10.0), Db(-20.0)));
    // Another kind at an index the node has for `Threshold`.
    assert!(!set.set(UnitAddr::new(UnitParam::Ratio, 1), 0.0));
}

/// The same kind at another index is accepted by the builder.
///
/// Mutation (run): the builder's duplicate check on the kind alone → the
/// second `Threshold` panics → fails.
#[test]
fn the_same_kind_at_another_index_is_accepted() {
    let a = Param::new(Db(0.0));
    let b = Param::new(Db(1.0));
    let c = Param::new(Db(2.0));
    let set = ParamSet::builder()
        .param(UnitParam::Threshold, a.as_atomic())
        .param(UnitAddr::new(UnitParam::Threshold, 1), b.as_atomic())
        .param(UnitAddr::new(UnitParam::Threshold, 2), c.as_atomic())
        .build();
    assert_eq!(set.params().count(), 3);
}

/// A duplicate full address is refused at build, whether the index was spelled
/// out or came from a bare `UnitParam`.
///
/// Mutation (run): drop the duplicate check from `param` → no panic →
/// fails.
#[test]
#[should_panic(expected = "twice")]
fn a_duplicate_full_address_is_refused() {
    let a = Param::new(Db(0.0));
    let _ = ParamSet::builder()
        .param(UnitAddr::new(UnitParam::Threshold, 1), a.as_atomic())
        .param(UnitAddr::new(UnitParam::Threshold, 1), a.as_atomic());
}

/// Index 0 spelled as a bare `UnitParam` and as `UnitAddr::new(.., 0)` is
/// one address.
///
/// Mutation (run): `UnitAddr::first` (the `From<UnitParam>` widening) at
/// index 1 → the two addresses differ, no panic → fails.
#[test]
#[should_panic(expected = "twice")]
fn a_bare_param_is_index_zero_for_the_duplicate_check() {
    let a = Param::new(Db(0.0));
    let _ = ParamSet::builder()
        .param(UnitParam::Threshold, a.as_atomic())
        .param(BAND0, a.as_atomic());
}

/// A node's port list takes the same kind at several indices, refuses a
/// duplicate full address, and finds each port by full address.
///
/// Mutation (run): `try_of`'s duplicate check comparing `.param` only → the
/// two bands are refused as a duplicate → fails.
#[test]
fn param_ports_key_on_the_full_address() {
    let ports = ParamPorts::of(&[BAND0, BAND1, UnitAddr::from(UnitParam::Ratio)]);
    assert_eq!(ports.len(), 3);
    assert_eq!(ports.index_of(BAND0), Some(0));
    assert_eq!(ports.index_of(BAND1), Some(1));
    assert_eq!(
        ports.index_of(UnitParam::Threshold),
        Some(0),
        "bare is index 0"
    );
    assert_eq!(ports.index_of(UnitParam::Ratio), Some(2));
    assert_eq!(ports.index_of(UnitAddr::new(UnitParam::Threshold, 2)), None);
    assert_eq!(
        ParamPorts::try_of(&[BAND1, BAND0, BAND1]),
        Err(ParamPortsError::Duplicate(BAND1))
    );
    // The bare-kind constructor is index 0 only.
    assert_eq!(
        ParamPorts::try_new(&[UnitParam::Threshold, UnitParam::Threshold]),
        Err(ParamPortsError::Duplicate(BAND0))
    );
}

/// A source addressed to index 1 drives port 1 and leaves port 0 reading its
/// base, even with the same source connected to both ports: a port takes
/// the events for its own full address and no other's.
///
/// Mutation (run): `ParamSegment::is_for` comparing the kind alone (the low
/// byte of the packed id) → port 0 follows index 1's steps too → fails.
/// `render_ramps` wanting `UnitAddr::first(param.param)` (the index dropped)
/// → port 1 hears nothing → fails.
#[test]
fn a_lane_for_index_1_does_not_touch_index_0() {
    let (mut ed, mut exec, _) = graph(&[(BAND1, -3.0)]);
    ed.spec_mut().connect_param(param(BAND0), src_out());
    ed.spec_mut().connect_param(param(BAND1), src_out());
    ed.commit().expect("commits");
    exec.apply_pending();
    // The first block is not declicked (a fresh unit), so the source's step is
    // read from its frame.
    let [b0, b1] = render(&mut ed, &mut exec, 64);
    assert!(b1.iter().all(|&x| x == -3.0), "band 1 follows its source");
    assert!(
        b0.iter().all(|&x| x == -10.0),
        "band 0 keeps its base: {:?}",
        &b0[..4]
    );
}

/// The mirror: a source addressed to index 0 leaves port 1 on its base.
///
/// Mutation (run): as above, in either direction.
#[test]
fn a_lane_for_index_0_does_not_touch_index_1() {
    let (mut ed, mut exec, _) = graph(&[(BAND0, -3.0)]);
    ed.spec_mut().connect_param(param(BAND0), src_out());
    ed.spec_mut().connect_param(param(BAND1), src_out());
    ed.commit().expect("commits");
    exec.apply_pending();
    let [b0, b1] = render(&mut ed, &mut exec, 64);
    assert!(b0.iter().all(|&x| x == -3.0));
    assert!(b1.iter().all(|&x| x == -20.0));
}

/// Both bands driven by their own addresses from one source, each to its own
/// value, and an undriven base still follows its cell: the base is read per
/// index.
///
/// Mutation (run): `param_base` returning `thr[0]` for every port → band 1's
/// base reads -10 → fails.
#[test]
fn each_index_reads_its_own_base_and_its_own_source_value() {
    let (mut ed, mut exec, controls) = graph(&[(BAND0, -1.0), (BAND1, -2.0)]);
    ed.spec_mut().connect_param(param(BAND0), src_out());
    ed.spec_mut().connect_param(param(BAND1), src_out());
    ed.commit().expect("commits");
    exec.apply_pending();
    let [b0, b1] = render(&mut ed, &mut exec, 64);
    assert!(b0.iter().all(|&x| x == -1.0));
    assert!(b1.iter().all(|&x| x == -2.0));

    // Disconnect band 1: it reads its base again (declicked), and the base
    // is its own cell's.
    assert!(controls.set(BAND1, -44.0));
    assert!(ed.spec_mut().disconnect_param(param(BAND1), src_out()));
    ed.commit().expect("commits");
    exec.apply_pending();
    for _ in 0..8 {
        render(&mut ed, &mut exec, 64);
    }
    let [b0, b1] = render(&mut ed, &mut exec, 64);
    assert!(b0.iter().all(|&x| x == -1.0), "band 0 still driven");
    assert!(b1.iter().all(|&x| x == -44.0), "band 1 on its own base");
}

/// A fork starts from each index's own value, and its cells are its own.
///
/// Mutation (run): `Bands::fork_fresh` not detaching (a shared cell) → the
/// write after the fork reaches it → fails; `param_set` adding the same cell
/// at both indices → band 1 forks at band 0's value → fails.
#[test]
fn a_fork_starts_from_each_indexs_value() {
    let (mut ed, mut exec, controls) = graph(&[]);
    ed.commit().expect("commits");
    exec.apply_pending();
    assert!(controls.set(BAND1, -33.0));
    let (mut fork_ed, mut fork_exec) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(64))
        .expect("forks");
    assert!(controls.set(BAND1, -1.0), "a write after the fork");
    assert!(controls.set(BAND0, -2.0));
    let [f0, f1] = render(&mut fork_ed, &mut fork_exec, 64);
    assert!(f0.iter().all(|&x| x == -10.0), "band 0 as it was");
    assert!(f1.iter().all(|&x| x == -33.0), "band 1 as it was set");
    let [l0, l1] = render(&mut ed, &mut exec, 64);
    assert!(l0.iter().all(|&x| x == -2.0));
    assert!(l1.iter().all(|&x| x == -1.0));
}

/// The contract's fork check covers every index of a set: each is a cell a
/// fork starts from and does not share.
///
/// Mutation (run): `Bands::fork_fresh` not detaching its cells (so a fork
/// shares them) → the check fails.
#[test]
fn the_contract_fork_check_covers_each_index() {
    tutti_graph::contract::assert_param_fork(Bands::new(-10.0, -20.0));
}

/// A node declares up to `MAX_PARAM_PORTS` (32) params: the list takes 32
/// addresses of one kind, refuses a 33rd, and a node declaring 32 renders with
/// its last port driven (the per-block input array, the range cells and the
/// executor's held views are all sized for the cap).
///
/// Mutations (run), each fails: `MAX_PARAM_PORTS` = 31 → the 32-address list
/// is refused; `try_of`'s cap check as `>=` → likewise; `try_of` checking
/// `> MAX_PARAM_PORTS + 1` → the 33rd is accepted; `ParamState::inputs` returning its array for 31 ports (`take(31)`) →
/// port 31 reads its base.
#[test]
fn a_node_declares_up_to_the_cap_and_no_more() {
    assert_eq!(MAX_PARAM_PORTS, 32);
    let addrs: Vec<UnitAddr> = (0..33)
        .map(|i| UnitAddr::new(UnitParam::Threshold, i))
        .collect();
    assert_eq!(
        ParamPorts::try_of(&addrs[..32]).map(|p| p.len()),
        Ok(32),
        "32 addresses"
    );
    assert_eq!(
        ParamPorts::try_of(&addrs),
        Err(ParamPortsError::TooMany { count: 33 }),
        "the 33rd"
    );

    // The last of 32 ports, driven through the whole graph.
    let last = addrs[31];
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.insert(
        BANDS,
        "wide",
        tutti_graph::ForkByClone(Wide(addrs[..32].to_vec())),
    );
    ed.insert(
        SRC,
        "steps",
        tutti_graph::ForkByClone(Steps {
            to: vec![(last, -7.0)],
        }),
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![out(BANDS, 0), out(BANDS, 1)]);
    ed.spec_mut().connect_param(param(last), src_out());
    ed.commit().expect("commits");
    exec.apply_pending();
    let [first, driven] = render(&mut ed, &mut exec, 64);
    assert!(first.iter().all(|&x| x == 0.0), "port 0 reads its base");
    assert!(
        driven.iter().all(|&x| x == -7.0),
        "port 31 follows its source"
    );
}

/// 32 declared params; writes port 0's value to output 0 and port 31's to
/// output 1. Bases are 0.
#[derive(Clone)]
struct Wide(Vec<UnitAddr>);

impl Node for Wide {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::from_count(2)).with_param_addrs(&self.0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (out, k) in [(0, 0), (1, 31)] {
            match io.param(k) {
                ParamInput::Base => io.output(out).fill(0.0),
                ParamInput::Frames(v) => io.output(out).copy_from_slice(v),
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, _: usize) -> Option<f32> {
        Some(0.0)
    }
}
