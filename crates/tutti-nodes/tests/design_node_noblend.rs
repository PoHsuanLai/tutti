//! `DesignNode<D>` over a design whose blend is [`NoBlend`] (doc 024 step 1b): a
//! node that is the wet signal alone, with no port and no cell for a mix, next to
//! the General MIDI reverb, whose [`DryWet`] blend has both. The state-variable
//! filter is the production [`NoBlend`] node; its output is the filter's alone
//! (`filter_digests.rs` pins its bits).

use tutti_dsp::SvfType;
use tutti_graph::contract::prepared;
use tutti_graph::{Node, ParamNode};
use tutti_nodes::{FreeverbNode, SvfFilterNode};
use tutti_types::{Hz, Q, SampleRate, UnitAddr, UnitParam};

const SR: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;

/// A node over [`NoBlend`](tutti_nodes::NoBlend) has no mix: no `Wet` port in its
/// shape (its ports are the filter's cutoff, Q and gain, in that order), no cell in
/// its `ParamSet`; a node over `DryWet` has the mix first.
///
/// Mutation (run): `NoBlend::ADDRS` written `&[UnitAddr::first(UnitParam::Wet)]`
/// puts a `Wet` port first in the filter's shape and fails the first assertion.
#[test]
fn a_noblend_node_has_no_wet_port() {
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

    let blended = prepared(FreeverbNode::default(), SR, BLOCK);
    assert_eq!(
        blended.shape().params.as_slice(),
        [UnitAddr::first(UnitParam::Wet)]
    );
    assert_eq!(blended.param_base(0), Some(1.0));
    assert!(blended.param_set().get(UnitParam::Wet).is_some());
}
