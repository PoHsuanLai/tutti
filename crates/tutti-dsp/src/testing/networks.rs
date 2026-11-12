use crate::reverb::network::{Absorption, Network, Stage};

/// Applies `f` to every [`Absorption`] of the network's recirculating stages.
pub(crate) fn map_absorption(n: &mut Network, f: impl Fn(Absorption) -> Absorption) {
    for stage in n.stages.as_mut_slice() {
        if let Stage::Fdn(fdn) = stage {
            for line in fdn.lines.as_mut_slice() {
                line.absorption = f(line.absorption);
            }
        }
    }
}
