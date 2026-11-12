//! Typed params on keys (doc 030, T2, T3, N2, N3): `lp.cutoff()` is a
//! reference whose value is in the unit the node declares, `Graph::set_param`
//! takes it with a value of that unit and says why it refused, and a host's
//! own `ParamNode` is inserted as `Params(node)` with no hand-written
//! `SessionNode`.

use tutti_graph::{Cx, Io, Node, ParamNode, ParamSet, Params, Prepare, Shape, Status};
use tutti_nodes::{CompressorNode, SvfFilterNode, SvfType};
use tutti_session::{ParamError, Session};
use tutti_types::{ChannelLayout, Db, Hz, Param, Q, SampleRate, Seconds, UnitAddr, UnitParam};

fn detached() -> Session {
    Session::builder()
        .outputs_floor(1)
        .build_detached(SampleRate(48_000.0))
        .expect("builds")
}

/// **A key's param method names the param it is called for, at the address
/// the node declares it at**, whatever the node: the kind is the method's,
/// the index 0, and the node is the key's. (An index other than 0 is a
/// node's `_at` method, held in `modulation/indexed_params.rs`.)
#[test]
fn a_keys_param_methods_name_their_kind_and_index() {
    let mut s = detached();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::new(
        SvfType::LowPass,
        Hz(1_000.0),
        Q(0.7),
    ));
    let cutoff = lp.cutoff();
    assert_eq!(cutoff.node(), lp.erase());
    assert_eq!(cutoff.addr(), UnitAddr::first(UnitParam::Cutoff));
    assert_eq!(lp.q().addr(), UnitAddr::first(UnitParam::Q));
    assert_eq!(lp.gain_db().addr(), UnitAddr::first(UnitParam::GainDb));
}

/// **A write through a key's param reaches the node's cell, in its unit**:
/// the filter's own control reads what `set_param` wrote.
#[test]
fn set_param_writes_the_cell_the_key_names() {
    let mut s = detached();
    let (lp, controls) = s.graph().add(SvfFilterNode::<f32>::new(
        SvfType::LowPass,
        Hz(1_000.0),
        Q(0.7),
    ));
    s.graph().set_param(lp.cutoff(), Hz(2_500.0)).unwrap();
    s.graph().set_param(lp.q(), Q(1.5)).unwrap();
    assert_eq!(controls.get(UnitParam::Cutoff), Some(2_500.0));
    assert_eq!(controls.get(UnitParam::Q), Some(1.5));
    assert_eq!(controls.get(UnitParam::GainDb), Some(0.0), "the rest stays");

    // A `Seconds` param is an `f64` cell: the write is exact for a value an
    // `f32` holds.
    let (comp, controls) = s.graph().add(CompressorNode::mono(
        Db(-20.0),
        tutti_types::CompressionRatio(4.0),
        Seconds(0.01),
        Seconds(0.1),
    ));
    s.graph().set_param(comp.attack(), Seconds(0.25)).unwrap();
    assert_eq!(controls.get(UnitParam::Attack), Some(0.25));
}

/// **A write that reaches nothing is refused, saying which**: a node that is
/// not in the graph, and a node with no such param addressed (one inserted
/// with `insert`, whose params nothing addresses). Nothing is written.
#[test]
fn set_param_refuses_a_missing_node_and_a_missing_param() {
    let mut s = detached();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::new(
        SvfType::LowPass,
        Hz(1_000.0),
        Q(0.7),
    ));
    let (bare, _) = s.graph().insert(SvfFilterNode::<f32>::new(
        SvfType::LowPass,
        Hz(1_000.0),
        Q(0.7),
    ));
    assert_eq!(
        s.graph().set_param(bare.cutoff(), Hz(5.0)),
        Err(ParamError::NoSuchParam {
            node: bare.erase(),
            addr: UnitAddr::first(UnitParam::Cutoff)
        })
    );

    let cutoff = lp.cutoff();
    s.graph().remove(lp);
    assert_eq!(
        s.graph().set_param(cutoff, Hz(5.0)),
        Err(ParamError::NoSuchNode(lp.erase()))
    );
}

/// A host's node with one param, a `Hz` cutoff.
#[derive(Clone)]
struct Mine {
    cutoff: Param<Hz>,
}

impl Node for Mine {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(self.cutoff.load().get());
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl ParamNode for Mine {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.cutoff.detach();
        fork
    }
}

tutti_graph::declare_params!([] Mine, n { Cutoff => Hz = &n.cutoff });

/// **A host's `ParamNode` is inserted as `Params(node)` and nothing more**:
/// no `IntoNode` and no `SessionNode` of its own. Its params are addressed
/// (`set_param` reaches it), its key has the typed method its declaration
/// gives, and a fork of it starts from the value set.
#[test]
fn a_params_wrapped_node_is_addressed_and_forks_from_its_values() {
    let mut s = detached();
    let (mine, controls) = s.graph().add(Params(Mine {
        cutoff: Param::new(Hz(100.0)),
    }));
    s.graph()
        .set_sources_hard(tutti_session::Root::output(0), [mine.out(0)])
        .unwrap();
    let _ = s.update();

    s.graph().set_param(mine.cutoff(), Hz(440.0)).unwrap();
    assert_eq!(controls.get(UnitParam::Cutoff), Some(440.0));
    let mut live = [0.0f32];
    s.render_frame(&mut live);
    assert_eq!(live, [440.0], "the live node reads it");

    let mut fork = s.graph_ref().fork_live().expect("a Params node forks");
    s.graph().set_param(mine.cutoff(), Hz(880.0)).unwrap();
    let mut forked = [0.0f32];
    fork.tick(&[], &mut forked);
    assert_eq!(forked, [440.0], "the fork has the value set at the fork");
}
