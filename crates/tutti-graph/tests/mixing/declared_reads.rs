//! A node reads a driven param through its `declare_params!` declaration
//! (`declared_param`, `declared_param_at`), not by a port index: the read is
//! the port of the row that names the kind, wherever the row sits.
//!
//! The node below declares its rows in an order no sorted guess would give
//! (a row whose cell is absent first, `Q` before `Cutoff`, a group last), so
//! the port of each is not the position a reader would assume. A source
//! steps `Cutoff` and the second band's `Threshold`; the node's outputs show
//! which param each read took.

use tutti_graph::testing as common;

use common::prepare;
use tutti_graph::{
    Cx, Editor, Event, EventOut, Executor, IntoNode, Io, Law, Node, NodeKey, NodeParts, OutPort,
    ParamIn, ParamInput, ParamNode, ParamSegment, ParamSet, Prepare, Shape, Source, Status,
    Transport, declare_params, param_parts,
};
use tutti_types::{Amplitude, ChannelLayout, Db, Hz, Param, Q, Samples, UnitAddr, UnitParam};

#[derive(Clone)]
struct Declared {
    // Absent: its row is no port, so every port after it is one earlier than
    // its position among the rows.
    gone: Option<Param<Amplitude>>,
    q: Param<Q>,
    cutoff: Param<Hz>,
    bands: [Param<Db>; 2],
}

declare_params!([] Declared, n {
    Volume => Amplitude = n.gone.as_ref(),
    Q => Q = &n.q,
    Cutoff => Hz = &n.cutoff,
    [b in 0..2] {
        Threshold => Db = &n.bands[b],
    },
});

fn show(out: &mut [f32], input: ParamInput<'_>, base: f32) {
    match input {
        ParamInput::Base => out.fill(base),
        ParamInput::Frames(v) => out.copy_from_slice(v),
    }
}

impl Node for Declared {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::from_count(4))
            .with_param_addrs(self.declared_ports().as_slice())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let cutoff = self.declared_param::<{ UnitParam::Cutoff as u16 }>(&io);
        let q = self.declared_param::<{ UnitParam::Q as u16 }>(&io);
        let band0 = self.declared_param_at::<{ UnitParam::Threshold as u16 }>(&io, 0);
        let band1 = self.declared_param_at::<{ UnitParam::Threshold as u16 }>(&io, 1);
        show(io.output(0), cutoff, self.cutoff.load().0);
        show(io.output(1), q, self.q.load().0);
        show(io.output(2), band1, self.bands[1].load().0);
        show(io.output(3), band0, self.bands[0].load().0);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        self.declared_base(port)
    }
}

impl ParamNode for Declared {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut f = self.clone();
        f.q.detach();
        f.cutoff.detach();
        for b in &mut f.bands {
            b.detach();
        }
        f
    }
}

impl IntoNode for Declared {
    type Controls = ParamSet;
    fn into_parts(self) -> NodeParts<ParamSet> {
        param_parts(self)
    }
}

/// At the first frame of every block, steps each address in `to`.
#[derive(Clone)]
struct Steps {
    to: Vec<(UnitAddr, f32)>,
}

impl Node for Steps {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1).with_event_capacity(8)
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

const NODE: NodeKey = NodeKey(1);
const SRC: NodeKey = NodeKey(2);

fn render(to: &[(UnitAddr, f32)]) -> [Vec<f32>; 4] {
    let (mut ed, mut exec): (Editor, Executor) = Editor::new(prepare(64));
    let node = Declared {
        gone: None,
        q: Param::new(Q(0.5)),
        cutoff: Param::new(Hz(1_000.0)),
        bands: [Param::new(Db(-10.0)), Param::new(Db(-20.0))],
    };
    ed.insert(NODE, "declared", node);
    ed.insert(
        SRC,
        "steps",
        tutti_graph::ForkByClone(Steps { to: to.to_vec() }),
    );
    ed.spec_mut().topology.set_outputs(
        (0..4)
            .map(|port| Source::Node(OutPort { node: NODE, port }))
            .collect::<Vec<_>>(),
    );
    for &(addr, _) in to {
        ed.spec_mut().connect_param(
            ParamIn {
                node: NODE,
                param: addr,
            },
            EventOut { node: SRC, port: 0 },
        );
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    let mut outs = [(); 4].map(|()| vec![0.0; 64]);
    {
        let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(|o| &mut o[..]).collect();
        exec.process(64, &Transport::default(), &[], &mut refs);
    }
    ed.collect();
    outs
}

/// A driven `Cutoff` reaches the read of `Cutoff` and no other, whatever
/// port its row is, and the params nothing drives read their bases.
#[test]
fn a_read_follows_its_row_not_its_position() {
    let [cutoff, q, band1, band0] = render(&[(UnitAddr::from(UnitParam::Cutoff), 7_000.0)]);
    assert!(
        cutoff.iter().all(|&x| x == 7_000.0),
        "cutoff follows its lane"
    );
    assert!(q.iter().all(|&x| x == 0.5), "q keeps its base");
    assert!(band1.iter().all(|&x| x == -20.0));
    assert!(band0.iter().all(|&x| x == -10.0));
}

/// A group row is read at its own index: stepping band 1 leaves band 0 and
/// the rows before the group alone.
#[test]
fn a_group_read_is_at_its_index() {
    let [cutoff, q, band1, band0] = render(&[(UnitAddr::new(UnitParam::Threshold, 1), -3.0)]);
    assert!(band1.iter().all(|&x| x == -3.0), "band 1 follows its lane");
    assert!(band0.iter().all(|&x| x == -10.0), "band 0 keeps its base");
    assert!(cutoff.iter().all(|&x| x == 1_000.0));
    assert!(q.iter().all(|&x| x == 0.5));
}
