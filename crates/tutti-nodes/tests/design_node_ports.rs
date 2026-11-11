//! `DesignNode<D>`'s ports are its design's cells and nothing else (doc 024 step
//! 2b PR9): no port for a mix, because a node is the wet signal alone and its
//! blend with the input is the graph's (`node_mix.rs`). The state-variable filter
//! and a live Freeverb start their ports at the first cell; the fixed General MIDI
//! send has none.

use tutti_dsp::{FreeverbControls, FreeverbDesign, SvfType};
use tutti_graph::contract::prepared;
use tutti_graph::{Node, ParamNode};
use tutti_nodes::{DesignNode, FreeverbNode, SvfFilterNode};
use tutti_types::{Hz, Q, SampleRate, UnitAddr, UnitParam};

const SR: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;

/// No `Wet` port in any of them: the filter's ports are its cutoff, Q and gain, in
/// that order, a live Freeverb's its room and damping, and a fixed one has none.
///
/// Mutation (run): `DesignNode::shape` putting `UnitAddr::first(UnitParam::Wet)`
/// first in the declared addresses fails the filter's and the live reverb's
/// assertions; `param_base` returning the cell at `k + 1` fails the base ones.
#[test]
fn a_design_nodes_ports_start_at_its_first_cell() {
    let filter = prepared(
        SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(1_000.0), Q(0.707)),
        SR,
        BLOCK,
    );
    assert_eq!(
        filter.shape().params.as_slice(),
        [
            UnitAddr::first(UnitParam::Cutoff),
            UnitAddr::first(UnitParam::Q),
            UnitAddr::first(UnitParam::GainDb),
        ]
    );
    assert_eq!(filter.param_base(0), Some(1_000.0));
    assert!(filter.param_set().get(UnitParam::Wet).is_none());

    let live = prepared(
        DesignNode::live(FreeverbDesign, FreeverbControls::GENERAL_MIDI),
        SR,
        BLOCK,
    );
    assert_eq!(
        live.shape().params.as_slice(),
        [
            UnitAddr::first(UnitParam::RoomSize),
            UnitAddr::first(UnitParam::Damping),
        ]
    );
    assert_eq!(
        live.param_base(0),
        Some(FreeverbControls::GENERAL_MIDI.room.get())
    );
    assert!(live.param_set().get(UnitParam::Wet).is_none());

    let fixed = prepared(FreeverbNode::default(), SR, BLOCK);
    assert!(fixed.shape().params.as_slice().is_empty());
    assert_eq!(fixed.param_base(0), None);
}
