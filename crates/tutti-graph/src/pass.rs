//! [`Pass`]: which input channel feeds which output channel when a node does
//! not run (bypass, and the aligned input of a mix; doc 013, "Buses and
//! coercion between declared layouts", B3).
//!
//! A node declares a `Main` bus on each side. When both say where their
//! channels sit, the bypass is the [`Fold`] between them (a 5.1 bus into a
//! stereo bus is the ITU downmix, not "the first two channels"). When neither
//! says (every channel `Discrete`, which is what [`Shape::audio`] builds) there
//! is no placement to fold between, and the mapping is the width rule that
//! predates buses: output `c` is input `c` below the narrower width, silence
//! past it. A pair that does name a placement and does not fold
//! ([`FoldError`]) passes nothing, and [`Editor::set_mode`](crate::Editor::set_mode)
//! refuses to bypass it ([`ModeError::Unplaced`](crate::ModeError::Unplaced)):
//! reading a bare width as a placement is the guess CLAUDE.md forbids.
//!
//! Built on the control side (a fold allocates); applied without allocating.

use tutti_types::{ChannelTopology, Fold, FoldError, Speaker};

use crate::node::Shape;

/// The channel mapping of a node that does not run.
pub(crate) struct Pass {
    /// Input channels the aligned path carries (one ring each).
    inputs: usize,
    /// Output channels it writes; the rest are silent.
    outputs: usize,
    /// The fold, when the mapping is not output `c` = input `c`.
    fold: Option<Fold>,
    /// Per output channel, the input channels it reads (bit `i`), for the
    /// silence flag. Empty when there is no fold.
    reads: Vec<u16>,
}

impl Pass {
    /// Nothing passes: no channel, no line.
    fn nothing() -> Self {
        Self {
            inputs: 0,
            outputs: 0,
            fold: None,
            reads: Vec::new(),
        }
    }

    /// The width rule: output `c` is input `c` below the narrower width.
    fn by_width(shape: &Shape) -> Self {
        let n = shape.audio_in.count().min(shape.audio_out.count()) as usize;
        Self {
            inputs: n,
            outputs: n,
            fold: None,
            reads: Vec::new(),
        }
    }

    /// The mapping of `shape`, or why its main buses do not fold. Control
    /// side: allocates.
    pub(crate) fn of(shape: &Shape) -> Result<Self, FoldError> {
        let (Some((_, src)), Some((_, dst))) =
            (shape.buses_in.main_bus(), shape.buses_out.main_bus())
        else {
            return Ok(Self::by_width(shape));
        };
        let discrete = |b: &crate::Bus| {
            b.topology
                .positions()
                .iter()
                .all(|s| *s == Speaker::Discrete)
        };
        if discrete(src) && discrete(dst) {
            return Ok(Self::by_width(shape));
        }
        let fold = Fold::between(
            &ChannelTopology::from(src.topology),
            &ChannelTopology::from(dst.topology),
        )?;
        let (inputs, outputs) = (src.width(), dst.width());
        if fold.is_identity() {
            return Ok(Self {
                inputs,
                outputs,
                fold: None,
                reads: Vec::new(),
            });
        }
        let reads = (0..outputs)
            .map(|o| {
                fold.contributions(o)
                    .fold(0u16, |mask, (i, _)| mask | 1 << i)
            })
            .collect();
        Ok(Self {
            inputs,
            outputs,
            fold: Some(fold),
            reads,
        })
    }

    /// [`of`](Self::of), or nothing passing when the buses do not fold.
    pub(crate) fn of_or_nothing(shape: &Shape) -> Self {
        Self::of(shape).unwrap_or_else(|_| Self::nothing())
    }

    /// Input channels carried.
    pub(crate) fn inputs(&self) -> usize {
        self.inputs
    }

    /// Output channels written.
    pub(crate) fn outputs(&self) -> usize {
        self.outputs
    }

    /// Whether output `c` is a combination of inputs, not a copy of input `c`.
    pub(crate) fn folds(&self) -> bool {
        self.fold.is_some()
    }

    /// Whether output `c` is silent when the inputs in `silent` (bit `i`: input
    /// `i` is) are.
    pub(crate) fn silent(&self, c: usize, silent: u64) -> bool {
        match self.reads.get(c) {
            Some(&reads) => u64::from(reads) & !silent == 0,
            None => silent & (1 << c) != 0,
        }
    }

    /// Output channel `c` of the fold over the aligned inputs `src`, into
    /// `out`. Allocation-free.
    ///
    /// # Panics
    ///
    /// If there is no fold (check [`folds`](Self::folds)).
    pub(crate) fn fold_into<'a>(
        &self,
        src: impl Fn(usize) -> &'a [f32],
        c: usize,
        out: &mut [f32],
    ) {
        self.fold
            .as_ref()
            .expect("a pass that folds")
            .planar_channel(src, c, out);
    }
}
