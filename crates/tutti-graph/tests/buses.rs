//! Buses on a node's `Shape` (doc 013, "Buses and coercion between declared
//! layouts", B1): a grouping over the ports that nothing else reads yet.
//!
//! From the spec: what an unassigned shape builds for a width, how declared buses
//! cover the ports, and that declaring them changes nothing the compiler
//! produces.

mod common;

use std::collections::BTreeMap;

use common::{Kind, prepare};
use tutti_graph::{
    Bus, BusRole, Buses, Edge, GraphSpec, InPort, MAX_BUSES, NodeKey, NodeSpec, OutPort, PortKind,
    Shape, Source, Topology, compile,
};
use tutti_types::{BusTopology, ChannelLayout, ChannelTopology, Speaker};

fn width(n: u16) -> ChannelLayout {
    ChannelLayout::from(n)
}

fn bus(topology: BusTopology, role: BusRole) -> Bus {
    Bus::new(topology, role)
}

/// An unassigned shape gives each non-empty side one `Main` bus of `Unassigned`
/// channels of its width (no placement is claimed from a count), and the
/// buses cover exactly the ports. A side with no channels has none, and one
/// wider than a bus holds keeps its channels as ports with no bus.
#[test]
fn a_unassigned_shape_is_one_unassigned_main_bus_of_the_width() {
    for n in [1u16, 2, 4, 6, 8, 16] {
        let through = Shape::through_unassigned(width(n));
        let source = Shape::source_unassigned(width(n));
        for buses in [through.buses_in(), through.buses_out(), source.buses_out()] {
            assert_eq!(buses.len(), 1);
            let (index, main) = buses.main_bus().expect("a main bus");
            assert_eq!(index, 0);
            assert_eq!(main.role, BusRole::Main);
            assert_eq!(
                ChannelTopology::from(main.topology),
                ChannelTopology::unassigned(width(n))
            );
            assert_eq!(buses.width(), usize::from(n), "the buses cover the ports");
        }
        assert!(source.buses_in().is_empty());
        assert_eq!(source.audio_in(), ChannelLayout::EMPTY);
    }
    let empty = Shape::EMPTY;
    assert!(empty.buses_in().is_empty() && empty.buses_out().is_empty());
    let wide = Shape::through_unassigned(width(17));
    assert!(wide.buses_in().is_empty() && wide.buses_out().is_empty());
    assert_eq!(wide.audio_out(), width(17), "the ports are kept");
    assert_eq!(wide.pass().expect("one width passes").inputs(), 17);
}

/// Declared buses sit in port order: each starts where the earlier ones
/// end, a sidechain is its own bus, the list holds `MAX_BUSES` and refuses
/// a fifth, and a declaration wider than the ports is refused by `Shape`.
#[test]
fn declared_buses_sit_in_port_order() {
    use Speaker::*;
    let five_one = BusTopology::new([
        FrontLeft,
        FrontRight,
        FrontCenter,
        LowFrequency,
        SideLeft,
        SideRight,
    ])
    .unwrap();
    let key = BusTopology::unassigned(ChannelLayout::MONO).unwrap();
    let stereo = BusTopology::new([FrontLeft, FrontRight]).unwrap();
    let buses = Buses::new(&[
        bus(five_one, BusRole::Main),
        bus(key, BusRole::Sidechain),
        bus(stereo, BusRole::Aux),
    ])
    .unwrap();
    assert_eq!(buses.width(), 9);
    assert_eq!(
        (0..4).map(|i| buses.first_port(i)).collect::<Vec<_>>(),
        [Some(0), Some(6), Some(7), None]
    );
    assert_eq!(buses.get(1).unwrap().role, BusRole::Sidechain);
    assert_eq!(buses.get(0).unwrap().topology.positions()[3], LowFrequency);

    let shape = Shape::buses(buses, Buses::unassigned(width(2)));
    assert_eq!(*shape.buses_in(), buses);
    assert_eq!(shape.audio_in(), width(9), "the widths are the buses'");
    assert_eq!(shape.audio_out(), width(2));

    let one = bus(key, BusRole::Aux);
    assert!(Buses::new(&[one; MAX_BUSES]).is_some());
    assert!(Buses::new(&[one; MAX_BUSES + 1]).is_none());

    // More channels than the buses hold keeps them as ports.
    let prefix = Shape::buses(buses, Buses::unassigned(width(2))).with_ports(width(12), width(2));
    assert_eq!(prefix.audio_in(), width(12));
    assert_eq!(prefix.buses_in().width(), 9);
}

#[test]
#[should_panic(expected = "ports narrower than the buses")]
fn ports_narrower_than_the_buses_are_refused() {
    let _ = Shape::through(BusTopology::STEREO).with_ports(width(1), width(2));
}

/// Declaring buses changes nothing the compiler makes: the same topology,
/// compiled once with every node's width-only buses and once with each node
/// declaring 5.1 on its main buses, gives the same plan (each unit keeps its
/// own shape, buses included; everything the compiler derived is equal).
#[test]
fn declaring_buses_changes_no_plan() {
    use Speaker::*;
    let five_one = BusTopology::new([
        FrontLeft,
        FrontRight,
        FrontCenter,
        LowFrequency,
        SideLeft,
        SideRight,
    ])
    .unwrap();
    let kinds: BTreeMap<NodeKey, Kind> = BTreeMap::from([
        (
            NodeKey(1),
            Kind::Const {
                value: 0.5,
                width: 6,
            },
        ),
        (NodeKey(2), Kind::Thru { width: 6 }),
    ]);
    let plain = common::shapes_of(&kinds);
    let mut placed = plain.clone();
    for shape in placed.values_mut() {
        let buses = Buses::main(five_one);
        let inputs = if shape.audio_in().count() == 6 {
            buses
        } else {
            Buses::NONE
        };
        *shape = common::rebused(*shape, inputs, buses);
    }
    assert_ne!(plain, placed, "the shapes differ in their buses");

    let mut t = Topology::default();
    for (k, kind) in &kinds {
        let s = plain[k];
        let _ = kind;
        t.nodes
            .insert(*k, NodeSpec::new("test", s.audio_in(), s.audio_out()));
    }
    for p in 0..6u16 {
        t.connect(
            InPort {
                node: NodeKey(2),
                port: p,
            },
            Edge::Direct(Source::Node(OutPort {
                node: NodeKey(1),
                port: p,
            })),
        );
    }
    t.outputs = vec![
        (0..6u16)
            .map(|p| {
                Source::Node(OutPort {
                    node: NodeKey(2),
                    port: p,
                })
            })
            .collect(),
    ];
    let graph = GraphSpec::new(t).validate().expect("valid");
    let a = compile(&graph, &plain, &prepare(64), None)
        .expect("compiles")
        .0;
    let b = compile(&graph, &placed, &prepare(64), None)
        .expect("compiles")
        .0;
    // The plan carries each unit's shape, buses and all; everything else of
    // it is what the compiler made, and is the same.
    assert_eq!(a.ops(), b.ops());
    assert_eq!(a.audio_list(), b.audio_list());
    assert_eq!((a.tasks(), a.task_ops()), (b.tasks(), b.task_ops()));
    assert_eq!(a.work(), b.work());
    assert_eq!(a.delays(), b.delays());
    assert_eq!(a.order(), b.order());
    assert_eq!(a.compensation(), b.compensation());
    assert_eq!(a.values(PortKind::Audio), b.values(PortKind::Audio));
    assert_eq!(a.units().len(), b.units().len());
    let bare = |s: Shape| common::rebused(s, Buses::NONE, Buses::NONE);
    for (x, y) in a.units().iter().zip(b.units()) {
        assert_eq!((x.key, x.arrival), (y.key, y.arrival));
        assert_eq!(bare(x.shape), bare(y.shape));
    }
    assert_ne!(a.units()[1].shape.buses_in(), b.units()[1].shape.buses_in());
}
