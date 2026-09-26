//! [`ParamModShaping`]: how one audio-rate modulation edge turns a raw
//! `[-1, 1]` modulator into an offset on a param.
//!
//! The edge itself is the native graph's (design doc 013 item 6): a
//! `GraphSpec::connect_param` from the modulator's output to the node's
//! declared param, which the compiler fuses with the node's own control (the
//! base) and every other source into one step of the node's op —
//! `clamp(base + Σ shaped offsets)` — handed to the node per frame. That
//! replaced a sub-graph of three node types built per modulated param:
//!
//! ```text
//! AtomicSourceNode ─► ParamSumNode ◄─ ParamShaperNode ◄─ source   (× N)
//!   (the base)        (base + Σ,        (depth·polarity·
//!                      one clamp)         curve, LUT)
//! ```
//!
//! with the node born with an extra input channel for the param
//! (`with_param_inputs`), because an unconnected input read 0 and a param
//! nothing fed would have dropped to it. The graph resolves an unconnected
//! param to its base instead, so none of that is needed; what is left here
//! is the shaping, which a host authors per edge.

use tutti_mod::{shape, CurveType, Polarity};

/// How one edge turns a raw `[-1, 1]` signal into an offset.
///
/// A tuple would do, but three positional near-interchangeable values (a float
/// newtype and two enums) is exactly the shape that gets mis-ordered.
///
/// **Deliberately carries no source.** Shaping is a property of the edge;
/// *what drives it* is a wiring question the graph spec answers
/// (`ParamFrom`), and the two have different owners.
///
/// `PartialEq`, so a host can tell that a route's shaping moved (a depth
/// slider changes no node count) by comparing the declaration against what it
/// last committed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamModShaping {
    /// How far the modulator swings the target, as a [`Depth`](tutti_types::Depth)
    /// fraction of the parameter's range. `0.0` is inert.
    pub depth: tutti_types::Depth,
    /// Whether the modulator's `[-1, 1]` output is applied bipolar (both
    /// directions from the base) or folded to unipolar (one direction only).
    pub polarity: Polarity,
    /// The response curve mapping the modulator's output onto the target —
    /// linear, exponential, and so on.
    pub curve: CurveType,
}

impl ParamModShaping {
    /// This edge's shaping as the graph's fused param step reads it:
    /// [`tutti_mod::shape`] — the same function the control-rate path applies
    /// — baked into a [`ShapeLut`](tutti_graph::ShapeLut) over the
    /// modulator's `[-1, 1]`, so the two tiers agree about a route's value.
    /// The bake and the lookup are the old `ParamShaperNode`'s, so a route
    /// sounds as it did (`tests/param_mod_oracle.rs` pins that bit for bit).
    ///
    /// Allocates the table: build it on the control thread.
    pub fn shaping(&self) -> tutti_graph::ParamShaping {
        let Self {
            depth,
            polarity,
            curve,
        } = *self;
        tutti_graph::ParamShaping::Lut(tutti_graph::ShapeLut::from_fn(move |x| {
            shape(x, depth, polarity, curve)
        }))
    }
}
