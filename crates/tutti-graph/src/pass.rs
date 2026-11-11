//! [`Pass`]: which input channel feeds which output channel when a node does
//! not run (bypass, and the aligned input of a mix; doc 013, "Buses and
//! coercion between declared layouts", B3).
//!
//! A node declares a `Main` bus on each side. When both say where their
//! channels sit, the pass is the [`Fold`] between them (a mono centre reaches
//! both sides of a stereo pair; a 5.1 bus into a stereo bus is the ITU
//! downmix, not "the first two channels"). When neither says (every channel
//! `Discrete`, which is what [`Shape::through_discrete`] builds) and the widths agree,
//! output `c` is input `c`. **Nothing else is guessed**: main buses of
//! different widths that name no placement, or placements that do not fold,
//! have no pass ([`PassError`]), and the graph refuses whatever would hear one
//! (a bypass, a mix short of [`Mix::WET`](tutti_types::Mix::WET), a lane on
//! the mix: [`Editor::set_mode`](crate::Editor::set_mode),
//! [`Editor::set_mix`](crate::Editor::set_mix), a commit). By index, a mono
//! input under a stereo output was heard on the left alone. A node whose
//! channels are not speaker feeds says what its other ports are: a key is a
//! `Sidechain` bus after the main one, a crossover's further bands `Aux`.
//!
//! Built on the control side (a fold allocates); applied without allocating.

use tutti_types::{ChannelTopology, Fold, FoldError, Speaker};

/// Why a node has no pass: what it would need to declare to be bypassed or
/// mixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PassError {
    /// The main input and output differ in width and one of them names no
    /// placement, so which input reaches which output is a guess. Declare the buses
    /// ([`Shape::buses`]).
    Unplaced {
        /// The main input's width.
        inputs: usize,
        /// The main output's width.
        outputs: usize,
    },
    /// The main buses name their channels and do not fold into each other.
    Fold(FoldError),
}

impl std::fmt::Display for PassError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unplaced { inputs, outputs } => write!(
                f,
                "{inputs} channels in and {outputs} out name no placement, so the dry \
                 path would be a guess: declare the node's buses"
            ),
            Self::Fold(e) => write!(f, "the main buses do not fold: {e}"),
        }
    }
}

impl std::error::Error for PassError {}

use crate::node::Shape;

/// The dry path of a node: where each input channel goes when the node does
/// not run (a bypass, the dry side of a mix). [`Shape::pass`] builds it.
#[derive(Clone, Debug, PartialEq)]
pub struct Pass {
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

    /// Output `c` is input `c`, for `n` channels.
    fn by_index(n: usize) -> Self {
        Self {
            inputs: n,
            outputs: n,
            fold: None,
            reads: Vec::new(),
        }
    }

    /// The mapping of `shape`, or why it has none ([`Shape::pass`]). Control
    /// side: allocates.
    pub(crate) fn of(shape: &Shape) -> Result<Self, PassError> {
        let (Some((_, src)), Some((_, dst))) =
            (shape.buses_in().main_bus(), shape.buses_out().main_bus())
        else {
            // No main bus on a side: no audio there (nothing passes), or more
            // channels than a bus holds, reached by port.
            let (inputs, outputs) = (
                usize::from(shape.audio_in().count()),
                usize::from(shape.audio_out().count()),
            );
            return match (inputs, outputs) {
                (0, _) | (_, 0) => Ok(Self::nothing()),
                (i, o) if i == o => Ok(Self::by_index(i)),
                (inputs, outputs) => Err(PassError::Unplaced { inputs, outputs }),
            };
        };
        let discrete = |b: &crate::Bus| {
            b.topology
                .positions()
                .iter()
                .all(|s| *s == Speaker::Discrete)
        };
        // A side that names no speaker says nothing about where a channel of
        // the other goes, except by its index at one width. (`Fold::between`
        // takes a named source onto a discrete destination by index, zero
        // filling, which suits an edge that knows only a width; for a dry path
        // it is the guess: a centre on the left alone.)
        if discrete(src) || discrete(dst) {
            if src.width() != dst.width() {
                return Err(PassError::Unplaced {
                    inputs: src.width(),
                    outputs: dst.width(),
                });
            }
            if discrete(src) && discrete(dst) {
                return Ok(Self::by_index(src.width()));
            }
        }
        let fold = Fold::between(
            &ChannelTopology::from(src.topology),
            &ChannelTopology::from(dst.topology),
        )
        .map_err(PassError::Fold)?;
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

    /// [`of`](Self::of), or nothing passing for a node with no pass: one the
    /// graph never bypasses or mixes, since every door that would refuses it.
    pub(crate) fn of_or_nothing(shape: &Shape) -> Self {
        Self::of(shape).unwrap_or_else(|_| Self::nothing())
    }

    /// Input channels carried.
    #[must_use]
    pub fn inputs(&self) -> usize {
        self.inputs
    }

    /// Output channels written; any past them are silent.
    #[must_use]
    pub fn outputs(&self) -> usize {
        self.outputs
    }

    /// The input channels output `c` reads, each with its gain: input `c` at
    /// 1.0 where the path goes by index, the fold's terms where it folds, and
    /// nothing for an output past [`outputs`](Self::outputs).
    pub fn contributions(&self, c: usize) -> impl Iterator<Item = (usize, f32)> + use<> {
        let terms: Vec<(usize, f32)> = match (&self.fold, c < self.outputs) {
            (_, false) => Vec::new(),
            (Some(fold), true) => fold.contributions(c).collect(),
            (None, true) => vec![(c, 1.0)],
        };
        terms.into_iter()
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
