//! The Linkwitz-Riley crossover as a graph node: [`CrossoverNode`] is a
//! [`DesignNode`] over [`CrossoverDesign`]. The algorithm is `tutti-dsp`'s; this
//! file holds the cells and how a block reads them.

use std::sync::Arc;

use tutti_dsp::{
    CrossoverControls, CrossoverDesign, CrossoverError, CrossoverTree, MAX_SPLITS, fit_to_rate,
};
use tutti_types::{AtomicF32, ChannelLayout, Hz, Param, SampleRate, UnitAddr, UnitParam};

use super::NodeKernel;
use crate::params::ControlSet;
use crate::ramp::LastGood;
use crate::reverb::DesignNode;

/// The crossover frequency cells of a node, and the last finite value each
/// held. Used by the multiband compressor.
#[derive(Clone, Debug)]
pub(crate) struct Splits {
    cells: Vec<Param<Hz>>,
    good: Vec<LastGood>,
}

impl Splits {
    pub(crate) fn new(frequencies: &[Hz]) -> Self {
        Self {
            cells: frequencies.iter().map(|&f| Param::new(f)).collect(),
            good: frequencies
                .iter()
                .map(|f| LastGood::new(f.get(), 0.0))
                .collect(),
        }
    }

    /// The address of crossover `i`: a cutoff, indexed.
    pub(crate) const fn addr(i: usize) -> UnitAddr {
        UnitAddr::new(UnitParam::Cutoff, i as u8)
    }

    /// Crossover `i`'s cell, for the node's [`ParamSet`](tutti_graph::ParamSet).
    pub(crate) fn cell(&self, i: usize) -> Arc<AtomicF32> {
        self.cells[i].as_atomic()
    }

    /// Crossover `i`'s value, or `None` past the last.
    pub(crate) fn base(&self, i: usize) -> Option<f32> {
        self.cells.get(i).map(|c| c.load().get())
    }

    /// Stops sharing every cell, keeping the values.
    pub(crate) fn detach(&mut self) {
        for c in &mut self.cells {
            c.detach();
        }
    }

    /// Every frequency once: the block's driven value where `driven(i)` has
    /// one, else the cell's. Held to the last finite value and fitted to
    /// `rate`.
    pub(crate) fn read(
        &mut self,
        rate: SampleRate,
        driven: impl Fn(usize) -> Option<f32>,
    ) -> [Hz; MAX_SPLITS] {
        let mut targets = [Hz(0.0); MAX_SPLITS];
        let n = self.cells.len();
        for (i, (t, good)) in targets.iter_mut().zip(&mut self.good).enumerate() {
            *t = Hz(good.read(driven(i).unwrap_or_else(|| self.cells[i].load().get())));
        }
        fit_to_rate(&mut targets[..n], rate);
        targets
    }
}

/// A graph node splitting its input into bands with a Linkwitz-Riley crossover per
/// channel: a [`DesignNode`] over [`CrossoverDesign`].
///
/// `channels` inputs, `bands * channels` outputs, band-major: output
/// `band * channels + channel`. That layout is fixed, and `bands * channels`
/// is at most the graph's 64 ports ([`CrossoverNode::new`] refuses more). The
/// bands are not delayed and the node adds no
/// latency; the sum of the bands has flat magnitude and the phase of the
/// crossovers' all-pass (see [`Crossover`](tutti_dsp::Crossover) for why, and for
/// what "not linear-phase" means for the bands). Its tail is
/// [`Tail::Unknown`](tutti_types::Tail::Unknown), as the other IIR filters': the
/// sections ring after the input stops, and it never parks.
///
/// # Params
///
/// Crossover `i` (between band `i` and band `i + 1`) is
/// `UnitAddr::new(UnitParam::Cutoff, i)` in [`Hz`], on the node's
/// [`ParamSet`](tutti_graph::ParamSet) and a param port the graph can modulate. The
/// crossovers are solved on a 16-frame grid counted from the last reset, at each
/// segment's first frame, and held over it: a control is in the render up to 16
/// frames after it was sampled, and the render does not depend on how audio is split
/// into blocks. A cell that moved glides, in hertz, from the last solve to its value
/// across the block, and a port's value is sampled at the solve frame (a split with a
/// port does not glide while another's cell does). The design holds each split between
/// the one below it and about 0.499 of the rate, so a set out of order is solved with
/// its splits no lower than the one before, and a non-finite write reads as unchanged.
pub type CrossoverNode = DesignNode<CrossoverDesign>;

impl CrossoverNode {
    /// A crossover over `layout` with one split at each of `frequencies`.
    ///
    /// # Errors
    ///
    /// [`CrossoverError`] if the frequencies are invalid at 48 kHz (see
    /// [`CrossoverControls::new`]), or `layout` has no channels or `bands * channels`
    /// exceeds the graph's 64 ports.
    pub fn new(layout: ChannelLayout, frequencies: &[Hz]) -> Result<Self, CrossoverError> {
        let controls = CrossoverControls::new(frequencies, SampleRate::DEFAULT)?;
        let design = CrossoverDesign::new(layout, frequencies.len() + 1)?;
        let channels = usize::from(layout.count());
        if channels * design.bands() > tutti_graph::MAX_PORTS {
            return Err(CrossoverError::Channels { channels });
        }
        Ok(Self::live(design, controls))
    }
}

/// The controls of a [`CrossoverNode`]: one cutoff per split.
#[derive(Clone, Copy, Debug)]
pub struct CrossoverSet {
    splits: usize,
}

impl ControlSet for CrossoverSet {
    type Controls = CrossoverControls;

    fn addrs(&self) -> &'static [UnitAddr] {
        static ADDRS: [UnitAddr; MAX_SPLITS] = [
            Splits::addr(0),
            Splits::addr(1),
            Splits::addr(2),
            Splits::addr(3),
            Splits::addr(4),
        ];
        &ADDRS[..self.splits]
    }

    fn defaults(&self) -> CrossoverControls {
        CrossoverControls {
            frequencies: [Hz(0.0); MAX_SPLITS],
        }
    }

    fn flatten(&self, c: &CrossoverControls, out: &mut [f32]) {
        for (o, f) in out.iter_mut().zip(&c.frequencies) {
            *o = f.get();
        }
    }

    fn assemble(&self, v: &[f32]) -> CrossoverControls {
        let mut frequencies = [Hz(0.0); MAX_SPLITS];
        for (f, &v) in frequencies.iter_mut().zip(v) {
            *f = Hz(v);
        }
        CrossoverControls { frequencies }
    }
}

impl NodeKernel for CrossoverTree {
    type Set = CrossoverSet;

    fn set(&self, _: &CrossoverControls) -> CrossoverSet {
        CrossoverSet {
            splits: self.bands() - 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{amplitude_at, sine};
    use crate::test_support::{prepared, render_chunked};
    use tutti_graph::{Cost, Node, ParamNode};
    use tutti_types::Tail;

    const RATE: f64 = 48_000.0;
    /// A whole number of periods of every tone on the 5 Hz grid.
    const WINDOW: usize = 9_600;
    const SETTLE: usize = 12_000;
    /// f32 rounding of the input, each band, and the sum: at most
    /// `(bands + 2)` half-ulps of the peak, each `2^-24`, which the lock-in
    /// only averages down.
    const TOL: f64 = 1e-6;

    fn render(node: &mut dyn Node, inputs: &[Vec<f32>]) -> Vec<Vec<f32>> {
        render_chunked(node, inputs, 500)
    }

    /// The node routes band-major: output `band * channels + channel`, with
    /// each channel filtered on its own.
    ///
    /// The shape declares what the node is: zero latency, an IIR's unknown tail,
    /// and a moderate cost.
    ///
    /// Mutation (run): `Tail::None` or `Cost::Heavy` in `shape` → fails.
    ///
    /// Mutation (run): index outputs `c * bands + b` → the silent right
    /// channel's outputs carry the left's bands → fails.
    #[test]
    fn the_node_writes_bands_band_major_per_channel() {
        let mut node = prepared(CrossoverNode::new(ChannelLayout::STEREO, &[Hz(1_000.0)]).unwrap());
        let shape = node.shape();
        assert_eq!((shape.audio_in.count(), shape.audio_out.count()), (2, 4));
        assert_eq!(shape.latency, tutti_types::Latency::default());
        assert_eq!(shape.tail, Tail::Unknown);
        assert_eq!(shape.cost, Cost::Moderate);
        let left = sine(100.0, RATE, SETTLE + WINDOW);
        let out = render(&mut node, &[left, vec![0.0; SETTLE + WINDOW]]);
        let warp = |f: f64| libm::tan(core::f64::consts::PI * f / RATE);
        let x4 = (warp(100.0) / warp(1_000.0)).powi(4);
        let low = amplitude_at(&out[0][SETTLE..], 100.0, RATE);
        let high = amplitude_at(&out[2][SETTLE..], 100.0, RATE);
        assert!((low - 1.0 / (1.0 + x4)).abs() < TOL, "{low}");
        assert!((high - x4 / (1.0 + x4)).abs() < TOL, "{high}");
        assert!(out[1].iter().chain(&out[3]).all(|&s| s == 0.0));
    }

    /// A param write moves the split the node runs: crossover 0 at 2 kHz
    /// leaves a 2 kHz tone at half amplitude in the low band (each branch is
    /// -6.02 dB there), where the 500 Hz split passes almost none of it.
    ///
    /// Mutation (run): `process` never reading the cell (`|_| None` and a
    /// constant) → the band keeps its 500 Hz split → fails.
    #[test]
    fn a_param_write_moves_the_split() {
        let node = CrossoverNode::new(ChannelLayout::MONO, &[Hz(500.0), Hz(4_000.0)]).unwrap();
        let set = node.param_set();
        let mut node = prepared(node);
        assert!(set.set(Splits::addr(0), 2_000.0));
        let x = sine(2_000.0, RATE, SETTLE + WINDOW);
        let out = render(&mut node, &[x]);
        let low = amplitude_at(&out[0][SETTLE..], 2_000.0, RATE);
        assert!((low - 0.5).abs() < TOL, "{low}");
    }

    /// A fork starts from the frequencies set and shares no cell with the
    /// live node, whatever the index.
    ///
    /// Mutation (run): drop `splits.detach()` in `fork_fresh` → a write to
    /// the live node reaches the fork → fails.
    #[test]
    fn a_fork_shares_no_cell() {
        tutti_graph::contract::assert_param_fork(
            CrossoverNode::new(ChannelLayout::MONO, &[Hz(500.0), Hz(900.0), Hz(3_000.0)]).unwrap(),
        );
    }

    /// The shape declares one cutoff port per crossover, indexed, and each
    /// port's base is its cell.
    ///
    /// Mutation (run): declare the ports with `UnitAddr::first` (every
    /// index 0) → the shape refuses the duplicate → fails; `param_base`
    /// answering port 0 for every port → fails.
    #[test]
    fn the_ports_are_the_indexed_cutoffs() {
        let node = CrossoverNode::new(
            ChannelLayout::STEREO,
            &[Hz(200.0), Hz(2_000.0), Hz(9_000.0)],
        )
        .unwrap();
        let want: Vec<UnitAddr> = (0..3)
            .map(|i| UnitAddr::new(UnitParam::Cutoff, i))
            .collect();
        assert_eq!(node.shape().params.as_slice(), &want[..]);
        let set = node.param_set();
        assert_eq!(set.params().collect::<Vec<_>>(), want);
        for (i, hz) in [200.0, 2_000.0, 9_000.0].into_iter().enumerate() {
            assert_eq!(node.param_base(i), Some(hz));
            assert_eq!(set.get(want[i]), Some(hz));
        }
        assert_eq!(node.param_base(3), None);
    }

    /// A driven port moves its own crossover: port 0 at 2 kHz leaves a 2 kHz
    /// tone at half amplitude in the low band, and the cell (500 Hz) is only
    /// the base.
    ///
    /// Mutation (run): read the driven value of port 1 for crossover 0
    /// (`block_driven(&io, i + 1)`) → the split stays at 500 Hz → fails.
    #[test]
    fn a_driven_port_moves_its_crossover() {
        use tutti_graph::contract::drive;
        let mut node =
            prepared(CrossoverNode::new(ChannelLayout::MONO, &[Hz(500.0), Hz(4_000.0)]).unwrap());
        let x = sine(2_000.0, RATE, SETTLE + WINDOW);
        let lane = [2_000.0f32; 512];
        let mut low = Vec::new();
        for chunk in x.chunks(512) {
            let out = drive(
                &mut node,
                crate::test_support::RATE,
                &[chunk],
                &[Some(&lane[..chunk.len()])],
            );
            low.extend_from_slice(&out[0]);
        }
        let amp = amplitude_at(&low[SETTLE..], 2_000.0, RATE);
        assert!((amp - 0.5).abs() < TOL, "{amp}");
    }

    /// The node refuses a layout it has no ports for, and bad frequencies.
    ///
    /// Mutation (run): drop the `channels == 0 ||` test in `CrossoverNode::new`
    /// → the empty layout builds → fails; `MAX_PORTS` to `MAX_PORTS + 4` →
    /// the 66-port layout builds → fails.
    #[test]
    fn the_node_validates_its_layout_and_frequencies() {
        assert!(matches!(
            CrossoverNode::new(ChannelLayout::from_count(0), &[Hz(100.0)]),
            Err(CrossoverError::Channels { channels: 0 })
        ));
        // 11 channels x 6 bands = 66 ports.
        assert!(matches!(
            CrossoverNode::new(
                ChannelLayout::from_count(11),
                &[Hz(1.0), Hz(2.0), Hz(3.0), Hz(4.0), Hz(5.0)]
            ),
            Err(CrossoverError::Channels { channels: 11 })
        ));
        assert!(
            CrossoverNode::new(
                ChannelLayout::from_count(10),
                &[Hz(1.0), Hz(2.0), Hz(3.0), Hz(4.0), Hz(5.0)]
            )
            .is_ok()
        );
        assert!(CrossoverNode::new(ChannelLayout::MONO, &[Hz(f32::NAN)]).is_err());
        assert!(CrossoverNode::new(ChannelLayout::MONO, &[Hz(9.0), Hz(3.0)]).is_err());
    }

    /// A node at 22.05 kHz with one frequency above its Nyquist still moves the
    /// others: the out-of-range one is held under the rate, not the whole set
    /// frozen.
    ///
    /// Mutation (run): `Params::read` reading a constant 200 Hz for the cell →
    /// the 300 Hz split never lands → fails.
    #[test]
    fn a_frequency_above_the_rate_does_not_freeze_the_others() {
        let node = CrossoverNode::new(ChannelLayout::MONO, &[Hz(200.0), Hz(15_000.0)]).unwrap();
        let set = node.param_set();
        let rate = 22_050.0;
        let mut node = tutti_graph::contract::prepared(node, SampleRate(rate), 1024);
        set.set(Splits::addr(0), 300.0);
        // 9 555 frames are 130 whole periods of 300 Hz at 22.05 kHz.
        let len = 9_555 + 12_000;
        let out = render(&mut node, &[sine(300.0, rate, len)]);
        // The split moved: the low band is half amplitude at 300 Hz.
        let low = amplitude_at(&out[0][12_000..], 300.0, rate);
        assert!((low - 0.5).abs() < TOL, "{low}");
        // The top crossover is under Nyquist, so the top band exists and is finite.
        assert!(out[2].iter().all(|s| s.is_finite()));
    }

    /// `prepare` reads the cells, not the construction values: a node built
    /// at 500 Hz whose cell was set to 2 kHz splits at 2 kHz from its first
    /// block. (Later blocks read the cells again, so only the first block can
    /// tell, and the measurement is over that one block.)
    ///
    /// The first block is 480 frames, whole periods of 200 Hz. A 200 Hz sine,
    /// a decade under the split, leaves the high band at `x^4 / (1 + x^4)` with
    /// `x = 0.1`, about 1e-4 in steady state; the bound is 2e-3 to allow the
    /// start-up transient of a zero state. A split still at 500 Hz, or ramping
    /// in from it, puts the high band at 0.025 or more. The low band at
    /// 200 Hz must be near 1 (`1 / (1 + x^4)`), within 2e-2 for the same
    /// transient.
    /// The filter's solved frequency is private, so it is read through its
    /// effect on a render.
    ///
    /// Mutation (run): the `set_frequencies` in `prepare` replaced by a no-op
    /// → the first block ramps in from 500 Hz and the high band is well above
    /// 2e-3 → fails.
    #[test]
    fn prepare_reads_the_cells() {
        let node = CrossoverNode::new(ChannelLayout::MONO, &[Hz(500.0)]).unwrap();
        node.param_set().set(Splits::addr(0), 2_000.0);
        let mut node = prepared(node);
        let out = render(&mut node, &[sine(200.0, RATE, 480)]);
        let low = amplitude_at(&out[0], 200.0, RATE);
        let high = amplitude_at(&out[1], 200.0, RATE);
        assert!((low - 1.0).abs() < 2e-2, "{low}");
        assert!(high < 2e-3, "{high}");
    }
}
