//! [`Io`]: one node call's buffers, reached through methods.
//!
//! # Why methods and not public fields
//!
//! The buffers are planar `f32` today (owner decision 2). Exposing them as
//! `&[&[f32]]` fields would make that layout part of every node's source; as
//! methods (`input`, `output`, `split`, `channel`) a later `input_f64` — or a
//! port format the compiler converts at a mismatched edge — is an addition,
//! not a breaking change. See [`PortKind`].
//!
//! # The length guarantee
//!
//! [`Io`] can only be built by the executor (or the reference interpreter),
//! through a constructor that asserts every slice is exactly
//! [`frames`](Io::frames) long and that `frames` is at most the
//! [`MaxBlock`] the node was prepared with. A node therefore never clamps.

use crate::event::{EventWriter, SortedEvents};
use crate::node::{ConstantMask, InPlaceMask, MaxBlock, SilenceMask};

/// What a port carries.
///
/// `Audio` is planar `f32` blocks. A sample *format* is deliberately not a
/// variant yet: when one is needed (an `f64` path for a plugin that wants it,
/// say), it would ride on `Audio`, and the compiler would insert a conversion
/// op on an edge whose two ends disagree — the same way it would coerce a
/// channel-count mismatch — so nodes on either side never see the other
/// format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PortKind {
    /// Planar `f32` audio, one channel per port.
    Audio,
    /// Sorted [`Event`](crate::Event) streams.
    Event,
}

/// One node call's buffers. See the [module docs](self).
pub struct Io<'a> {
    frames: usize,
    inputs: &'a [&'a [f32]],
    outputs: &'a mut [&'a mut [f32]],
    silent: SilenceMask,
    constant: ConstantMask,
    in_place: InPlaceMask,
    events_in: &'a [SortedEvents<'a>],
    events_out: &'a mut [EventWriter<'a>],
}

/// The audio inputs, while the outputs are borrowed too.
pub struct Inputs<'s> {
    slices: &'s [&'s [f32]],
    in_place: InPlaceMask,
}

/// The audio outputs, while the inputs are borrowed too.
pub struct Outputs<'s, 'a> {
    slices: &'s mut [&'a mut [f32]],
}

/// One channel's input and output together.
pub enum Channel<'s> {
    /// Separate buffers: read `input`, write `output`.
    Split {
        /// The input samples.
        input: &'s [f32],
        /// Where the output goes.
        output: &'s mut [f32],
    },
    /// One buffer: it holds the input now and must hold the output after.
    InPlace(&'s mut [f32]),
}

impl Channel<'_> {
    /// Apply `f` sample by sample, whichever form this is.
    pub fn map(self, mut f: impl FnMut(f32) -> f32) {
        match self {
            Channel::Split { input, output } => {
                for (o, &i) in output.iter_mut().zip(input) {
                    *o = f(i);
                }
            }
            Channel::InPlace(buf) => {
                for x in buf.iter_mut() {
                    *x = f(*x);
                }
            }
        }
    }
}

impl<'a> Io<'a> {
    /// Build a call's buffers, enforcing the length guarantee.
    ///
    /// # Panics
    ///
    /// Always, if `frames` is zero or exceeds `max` — the bound a node's
    /// scratch was sized from. In debug builds also if an output is not
    /// exactly `frames` long, or an input is not (an aliased input is empty):
    /// both executors build every slice from `frames` themselves, so that
    /// check is a guard on this crate's code, not on the caller's, and is not
    /// worth a per-channel loop on every node call in release.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        max: MaxBlock,
        frames: usize,
        inputs: &'a [&'a [f32]],
        outputs: &'a mut [&'a mut [f32]],
        silent: SilenceMask,
        constant: ConstantMask,
        in_place: InPlaceMask,
        events_in: &'a [SortedEvents<'a>],
        events_out: &'a mut [EventWriter<'a>],
    ) -> Self {
        assert!(
            frames > 0 && frames <= max.get(),
            "a block of {frames} frames against a prepared maximum of {}",
            max.get()
        );
        debug_assert!(outputs.iter().all(|o| o.len() == frames));
        debug_assert!(inputs
            .iter()
            .enumerate()
            .all(|(c, i)| i.len() == frames || (in_place.get(c) && i.is_empty())));
        Self {
            frames,
            inputs,
            outputs,
            silent,
            constant,
            in_place,
            events_in,
            events_out,
        }
    }

    /// The block length in frames — at most the prepared
    /// [`MaxBlock`]. A slice length, hence `usize`.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Number of audio input channels.
    pub fn input_count(&self) -> usize {
        self.inputs.len()
    }

    /// Number of audio output channels.
    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }

    /// Input channel `c`, wherever it is: its own buffer, or — when the
    /// executor aliased it in place — the output buffer that holds it.
    pub fn input(&self, c: usize) -> &[f32] {
        if self.in_place.get(c) {
            &self.outputs[c][..]
        } else {
            self.inputs[c]
        }
    }

    /// Output channel `c`.
    pub fn output(&mut self, c: usize) -> &mut [f32] {
        &mut self.outputs[c][..]
    }

    /// Input and output channel `c` together.
    pub fn channel(&mut self, c: usize) -> Channel<'_> {
        if self.in_place.get(c) {
            Channel::InPlace(&mut self.outputs[c][..])
        } else {
            Channel::Split {
                input: self.inputs[c],
                output: &mut self.outputs[c][..],
            }
        }
    }

    /// Every input and every output at once, for a node that mixes across
    /// channels. An aliased channel is reachable only through the outputs.
    pub fn split(&mut self) -> (Inputs<'_>, Outputs<'_, 'a>) {
        (
            Inputs {
                slices: self.inputs,
                in_place: self.in_place,
            },
            Outputs {
                slices: &mut *self.outputs,
            },
        )
    }

    /// Input channels that are exact silence this block.
    pub fn silent(&self) -> SilenceMask {
        self.silent
    }

    /// Input channels that hold one value throughout this block.
    pub fn constant(&self) -> ConstantMask {
        self.constant
    }

    /// Channels aliased in place: for each set `c`, output `c` **already
    /// holds input `c`**. Never set unless the node's
    /// [`Shape::in_place`](crate::Shape::in_place) is `true`.
    pub fn in_place(&self) -> InPlaceMask {
        self.in_place
    }

    /// Number of event input ports.
    pub fn event_input_count(&self) -> usize {
        self.events_in.len()
    }

    /// Event input port `p`, sorted by offset, every offset inside the block.
    pub fn events(&self, p: usize) -> SortedEvents<'a> {
        self.events_in[p]
    }

    /// Number of event output ports.
    pub fn event_output_count(&self) -> usize {
        self.events_out.len()
    }

    /// The writer for event output port `p`.
    pub fn event_out(&mut self, p: usize) -> &mut EventWriter<'a> {
        &mut self.events_out[p]
    }
}

impl Inputs<'_> {
    /// Number of channels.
    pub fn len(&self) -> usize {
        self.slices.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.slices.is_empty()
    }

    /// Input channel `c`.
    ///
    /// # Panics
    ///
    /// If `c` is aliased in place — read it through the outputs.
    pub fn get(&self, c: usize) -> &[f32] {
        assert!(
            !self.in_place.get(c),
            "input {c} is in place; it is in the output buffer"
        );
        self.slices[c]
    }
}

impl<'s, 'a> Outputs<'s, 'a> {
    /// Number of channels.
    pub fn len(&self) -> usize {
        self.slices.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.slices.is_empty()
    }

    /// Output channel `c`.
    pub fn get(&mut self, c: usize) -> &mut [f32] {
        &mut self.slices[c][..]
    }

    /// Every output channel.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut [f32]> + use<'_, 's, 'a> {
        self.slices.iter_mut().map(|s| &mut **s)
    }
}
