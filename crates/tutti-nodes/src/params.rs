//! One reader for the cells of every [`DesignNode`](crate::DesignNode) family
//! (doc 024 section 2.6).
//!
//! A family writes a [`ControlSet`]: its addresses, and the conversion between its
//! `Controls` and the `f32`s the ports carry. [`Params`] is the one reader over it,
//! with one rule for every control: the lane's value at
//! the solve frame when a lane drives it, else a glide from the controls last
//! designed to the cell's value, each read through [`LastGood`]. Sanitising
//! (floors, clamps, whole-set validation) is the design's, never the reader's.

use std::sync::Arc;
use std::sync::atomic::Ordering::{Acquire, Relaxed};

use tutti_dsp::Ramp;
use tutti_graph::{Io, ParamSetBuilder};
use tutti_types::{AtomicF32, SampleRate, UnitAddr};

use crate::ramp::LastGood;

/// The most controls a family has.
const MAX_CONTROLS: usize = 12;

/// A family's controls as a list of `f32` cells.
pub trait ControlSet: Clone + core::fmt::Debug + Send + 'static {
    /// The design's own `Controls`.
    type Controls: Copy + PartialEq + Send;

    /// The addresses of the cells, in port order after the blend's; its length is
    /// the number of controls [`flatten`](Self::flatten) writes.
    fn addrs(&self) -> &'static [UnitAddr];

    /// The controls a non-finite cell falls back to.
    fn defaults(&self) -> Self::Controls;

    /// `c` as one `f32` per address (`out.len() == addrs().len()`).
    fn flatten(&self, c: &Self::Controls, out: &mut [f32]);

    /// The controls of one `f32` per address: the inverse of
    /// [`flatten`](Self::flatten).
    fn assemble(&self, v: &[f32]) -> Self::Controls;

    /// The controls that ring longest, given each one's lowest and highest value
    /// in the block. Only a design that parks asks; the default is the highest.
    fn longest(&self, _lowest: &Self::Controls, highest: &Self::Controls) -> Self::Controls {
        *highest
    }

    /// Whether `target` is far enough from `applied`, the controls the model was last
    /// designed from, to design again: the dead-band. Exact inequality by default.
    fn moved(&self, applied: &Self::Controls, target: &Self::Controls) -> bool {
        applied != target
    }
}

/// That a [`ControlSet`] has a control of the kind `P` (a packed
/// [`UnitAddr`] at index 0), in the unit `Unit`: what
/// `control_set!` writes from the family's own `Controls` fields, and what
/// the typed key of a [`DesignNode`](crate::DesignNode) reads
/// ([`HasParam`](tutti_graph::HasParam)).
pub trait HasControl<const P: u16> {
    /// The unit the control's value is in.
    type Unit: tutti_graph::ParamUnit;
}

/// That a [`ControlSet`] has several controls of the kind `P`, numbered from 0.
pub trait HasControls<const P: u16>: HasControl<P> {}

/// Writes a family's [`ControlSet`] from its `Controls` fields, once: the
/// addresses, the conversion to the `f32`s the ports carry and back, and the
/// unit of each control ([`HasControl`]). The unit a row states is checked
/// against the field's own type (`flatten` reads it as that unit), so a
/// mismatch does not compile.
///
/// ```text
/// control_set! {
///     SvfSet(s): SvfControls = SvfControls { ty: s.0, ..SvfControls::DEFAULT };
///     live: (all);
///     Cutoff => Hz = freq,
///     Q => Q = q,
///     GainDb => Db = gain;
///     // the rest of the `ControlSet` impl:
///     fn defaults(&self) -> SvfControls { SvfControls::DEFAULT }
/// }
/// ```
///
/// The first line names the set, its binding (`s`, for the next expressions)
/// and its `Controls`, with the controls [`assemble`](ControlSet::assemble)
/// starts from (the parts the ports do not carry). `live` is how many of the
/// rows the set has (`all`, or an expression: a design that cannot freeze
/// leaves off the freeze). A row is `Kind => Unit = field`, a single control;
/// `Kind[0] => Unit = field[0]` the first of several of a kind (`HasControls`),
/// and `Kind[i]` with `i` above 0 the others.
macro_rules! control_set {
    (
        $set:ident($s:ident): $controls:ty = $base:expr;
        live: ($($live:tt)+);
        $($kind:ident $([$ix:tt])? => $unit:ty = $f:ident $([$pi:literal])?),+ ;
        $($items:tt)*
    ) => {
        impl $crate::params::ControlSet for $set {
            type Controls = $controls;

            #[allow(unused_variables)]
            fn addrs(&self) -> &'static [tutti_types::UnitAddr] {
                const ALL: &[tutti_types::UnitAddr] = &[$(
                    control_set!(@addr $kind $([$ix])?)
                ),+];
                let $s = self;
                &ALL[..control_set!(@live ALL, $($live)+)]
            }

            fn flatten(&self, c: &$controls, out: &mut [f32]) {
                let all = [$(
                    <$unit as tutti_graph::ParamUnit>::to_cell(c.$f $([$pi])?)
                ),+];
                out.copy_from_slice(&all[..out.len()]);
            }

            #[allow(unused_variables)]
            fn assemble(&self, v: &[f32]) -> $controls {
                let $s = self;
                let mut c: $controls = $base;
                let mut cells = v.iter().copied();
                $(
                    if let Some(x) = cells.next() {
                        c.$f $([$pi])? = <$unit as tutti_graph::ParamUnit>::from_cell(x);
                    }
                )+
                c
            }

            $($items)*
        }
        $( control_set!(@has $set; $kind $([$ix])? => $unit); )+
    };
    (@addr $kind:ident) => { tutti_types::UnitAddr::first(tutti_types::UnitParam::$kind) };
    (@addr $kind:ident [$ix:tt]) => {
        tutti_types::UnitAddr::new(tutti_types::UnitParam::$kind, $ix)
    };
    (@live $all:ident, all) => { $all.len() };
    (@live $all:ident, $($live:tt)+) => { $($live)+ };
    (@has $set:ident; $kind:ident => $unit:ty) => {
        impl $crate::params::HasControl<{ tutti_types::UnitParam::$kind as u16 }> for $set {
            type Unit = $unit;
        }
    };
    (@has $set:ident; $kind:ident [0] => $unit:ty) => {
        impl $crate::params::HasControl<{ tutti_types::UnitParam::$kind as u16 }> for $set {
            type Unit = $unit;
        }
        impl $crate::params::HasControls<{ tutti_types::UnitParam::$kind as u16 }> for $set {}
    };
    (@has $set:ident; $kind:ident [$ix:tt] => $unit:ty) => {};
}

/// Where in a block a node reads its controls, and what it last designed from.
#[derive(Clone, Copy, Debug)]
pub struct At<C> {
    /// The frame of the block to read a driven port at.
    pub frame: usize,
    /// The frames a cell's glide ramps over: the block, or up to its last solve
    /// when a grid solves inside it.
    pub block: usize,
    /// The controls the model was designed from at the start of the block, where
    /// a cell's glide starts; `None` before the first design (after a prepare),
    /// when there is nothing to glide from.
    pub applied: Option<C>,
    /// The rate the node runs at, for a control that is only valid against it.
    pub rate: SampleRate,
}

/// The cells of a [`ControlSet`] and how a block reads them.
#[derive(Clone, Debug)]
pub struct Params<S: ControlSet> {
    set: S,
    cells: Vec<Arc<AtomicF32>>,
    /// The last finite value each control held: a non-finite write reads as
    /// unchanged, so it never reaches the design.
    good: Vec<LastGood>,
}

impl<S: ControlSet> Params<S> {
    /// Cells of `set` holding `initial`. Control side; allocates.
    #[must_use]
    pub fn new(set: S, initial: &S::Controls) -> Self {
        let n = set.addrs().len();
        let (mut value, mut fallback) = ([0.0; MAX_CONTROLS], [0.0; MAX_CONTROLS]);
        set.flatten(initial, &mut value[..n]);
        set.flatten(&set.defaults(), &mut fallback[..n]);
        let (mut cells, mut good) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for k in 0..n {
            let mut g = LastGood::new(value[k], fallback[k]);
            cells.push(Arc::new(AtomicF32::new(g.read(value[k]))));
            good.push(g);
        }
        Self { set, cells, good }
    }

    /// Each control's lowest and highest value in the block (the cell's where no
    /// lane drives it), or `None` when no lane does.
    fn extremes(&self, io: &Io<'_>) -> Option<[[f32; MAX_CONTROLS]; 2]> {
        let mut out = [[0.0; MAX_CONTROLS]; 2];
        let mut driven = false;
        for (k, cell) in self.cells.iter().enumerate() {
            let lane = io.param(k).frames();
            driven |= lane.is_some();
            let base = cell.load(Acquire);
            let fold = |pick: fn(f32, f32) -> f32| {
                lane.into_iter()
                    .flatten()
                    .copied()
                    .filter(|v| v.is_finite())
                    .fold(base, pick)
            };
            out[0][k] = fold(f32::min);
            out[1][k] = fold(f32::max);
        }
        driven.then_some(out)
    }

    /// The addresses of the cells, in port order.
    pub(crate) fn addrs(&self) -> &'static [UnitAddr] {
        self.set.addrs()
    }

    /// Adds each cell to `set` under its address.
    pub(crate) fn bind(&self, set: ParamSetBuilder) -> ParamSetBuilder {
        let cells = self.cells.iter().cloned();
        self.addrs()
            .iter()
            .zip(cells)
            .fold(set, |set, (&addr, cell)| set.param(addr, cell))
    }

    /// The base of the `k`th address ([`Node::param_base`](tutti_graph::Node::param_base)): its cell's value.
    pub(crate) fn base(&self, k: usize) -> Option<f32> {
        self.cells.get(k).map(|c| c.load(Acquire))
    }

    /// The controls at `at.frame`: each cell, or the value its port carries when the
    /// graph modulates it, read through [`LastGood`] so a non-finite write reads as
    /// unchanged.
    pub(crate) fn read(&mut self, io: &Io<'_>, at: At<S::Controls>) -> S::Controls {
        let n = self.cells.len();
        let mut from = [0.0; MAX_CONTROLS];
        if let Some(applied) = &at.applied {
            self.set.flatten(applied, &mut from[..n]);
        }
        let mut value = [0.0; MAX_CONTROLS];
        for (k, v) in value[..n].iter_mut().enumerate() {
            *v = match io.param(k).frames() {
                Some(lane) => self.good[k].read(lane[at.frame]),
                None => {
                    let to = self.good[k].read(self.cells[k].load(Relaxed));
                    match at.applied {
                        Some(_) => Ramp::new(from[k], to, at.block).at(at.frame),
                        None => to,
                    }
                }
            };
        }
        self.set.assemble(&value[..n])
    }

    /// Whether a port drives a control, so the controls can differ between the frames
    /// of the block.
    pub(crate) fn driven(&self, io: &Io<'_>) -> bool {
        (0..self.cells.len()).any(|k| io.param(k).frames().is_some())
    }

    /// The controls that ring longest among the values a modulated port takes anywhere
    /// in the block, or `None` when nothing is modulated.
    pub(crate) fn longest(&self, io: &Io<'_>) -> Option<S::Controls> {
        let [lowest, highest] = self.extremes(io)?;
        let n = self.cells.len();
        Some(self.set.longest(
            &self.set.assemble(&lowest[..n]),
            &self.set.assemble(&highest[..n]),
        ))
    }

    /// Stops sharing the cells, keeping their values: a fork's own.
    pub(crate) fn detach(&mut self) {
        for cell in &mut self.cells {
            *cell = Arc::new(AtomicF32::new(cell.load(Acquire)));
        }
    }

    /// Whether `target` is far enough from `applied` to design again ([`ControlSet::moved`]).
    pub(crate) fn moved(&self, applied: &S::Controls, target: &S::Controls) -> bool {
        self.set.moved(applied, target)
    }
}
