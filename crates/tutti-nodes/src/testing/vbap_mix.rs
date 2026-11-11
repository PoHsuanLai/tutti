//! A VBAP mix as a test graph is wired: a panner per source, summed by fan-in
//! onto the output channels, and (where the layout has an LFE) a bass-managed
//! send, because a panner never feeds the LFE.

use tutti_graph::{GraphBuilder, NodeKey};
use tutti_types::{Azimuth, ChannelLayout, ChannelTopology, Elevation, Hz, Q};

use crate::{SvfFilterNode, SvfType, VbapError, VbapPannerNode};

/// Wire a panner for `layout` per `(source, bearing)` into `g`'s outputs: the
/// source's ports 0 and 1 feed the panner (at `Elevation::LEVEL`), and each
/// panner channel `c` feeds output `c`, so the graph sums the panners by
/// fan-in.
///
/// # Errors
///
/// [`VbapError`] for a layout with no bed.
pub fn vbap_mix(
    g: &mut GraphBuilder,
    layout: ChannelLayout,
    sources: &[(NodeKey, Azimuth)],
) -> Result<(), VbapError> {
    for &(source, azimuth) in sources {
        let panner = VbapPannerNode::for_topology(&ChannelTopology::for_width(layout))?;
        panner.controls().set_position(azimuth, Elevation::LEVEL);
        let (pan, _) = g.add_with_controls(panner);
        g.connect(source, 0, pan, 0).connect(source, 1, pan, 1);
        for c in 0..layout.count() as usize {
            g.connect_output(pan, c, c);
        }
    }
    Ok(())
}

/// A bass-managed send for the LFE of `layout`: each source's port 0, summed by
/// fan-in onto a 120 Hz low-pass, into the output the layout's SMPTE topology
/// names as its LFE. Nothing is added for a layout without one.
pub fn bass_send(g: &mut GraphBuilder, layout: ChannelLayout, sources: &[NodeKey]) {
    let Some(lfe) = ChannelTopology::smpte(layout).and_then(|t| t.lfe_index()) else {
        return;
    };
    let (low_pass, _) = g.add_with_controls(SvfFilterNode::<f32>::new(
        SvfType::LowPass,
        Hz(120.0),
        Q(0.707),
    ));
    for &source in sources {
        g.connect(source, 0, low_pass, 0);
    }
    g.connect_output(low_pass, 0, lfe);
}
