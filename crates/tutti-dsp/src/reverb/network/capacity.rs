//! [`NetworkCapacity`]: the reverb's worst-case sizes, what [`Grows::reach`](crate::Grows::reach)
//! gives for [`FreeverbFamily`](crate::FreeverbFamily) and [`FdnFamily`](crate::FdnFamily).

use tutti_types::{Db, Samples};

use super::{Network, Stage};
use crate::{Capacity, Ring, System};

/// The size of a [`Network`], or the worst case over a family's controls: every
/// figure a runner allocates from.
///
/// [`Capacity::need`] measures one network; [`Capacity::dominates`] says one capacity holds
/// another.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NetworkCapacity {
    /// Delay lines in all.
    pub lines: usize,
    /// Stages.
    pub stages: usize,
    /// Taps in all.
    pub taps: usize,
    /// Output channels of the widest recirculating stage.
    pub outputs: usize,
    /// The longest single line, in frames, with its nested all-pass and its
    /// modulation excursion.
    pub line: f32,
    /// The total delay of every line, all-pass and tap, in frames, with the
    /// modulation excursions.
    pub delay: f32,
    /// The longest FIR.
    pub ir: Samples,
    /// The longest pre-delay.
    pub pre_delay: Samples,
    /// The longest [`tail`](System::tail) at the reach's floor.
    pub tail: Ring,
}

impl Capacity for NetworkCapacity {
    type Source = Network;

    fn need(network: &Network, floor: Db) -> Self {
        let mut b = NetworkCapacity {
            lines: 0,
            stages: network.stages.len(),
            taps: 0,
            outputs: 0,
            line: 0.0,
            delay: 0.0,
            ir: Samples::ZERO,
            pre_delay: network.pre_delay,
            tail: network.tail(floor),
        };
        for stage in network.stages.iter() {
            match stage {
                Stage::Taps(t) => {
                    b.taps += t.taps.len();
                    b.delay += t.taps.iter().map(|t| t.delay).fold(0.0, f32::max);
                }
                Stage::AllPass(a) => {
                    b.delay += a
                        .iter()
                        .map(|a| a.delay + a.modulation.depth.abs())
                        .sum::<f32>();
                }
                Stage::Fdn(f) => {
                    b.lines += f.lines.len();
                    b.outputs = b.outputs.max(f.output.rows.len());
                    for l in f.lines.iter() {
                        let len = l.effective_length() + l.modulation.depth.abs();
                        b.line = b.line.max(len);
                        b.delay += len;
                    }
                }
                Stage::Fir(s) => b.ir = b.ir.max(s.length),
            }
        }
        b
    }

    fn tail(&self) -> Ring {
        self.tail
    }

    fn dominates(&self, other: &NetworkCapacity) -> bool {
        self.lines >= other.lines
            && self.stages >= other.stages
            && self.taps >= other.taps
            && self.outputs >= other.outputs
            && self.line >= other.line
            && self.delay >= other.delay
            && self.ir >= other.ir
            && self.pre_delay >= other.pre_delay
            && self.tail.covers(other.tail)
    }
}
