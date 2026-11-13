//! The neutral model module: the traits every family of processor
//! answers to, and the engineering that sizes and schedules what runs it.
//!
//! A [`Family`] is a parameter map, a [`System`] (and its class, [`Lti`]) is the
//! mathematical object it gives (a member of the family), and a [`Recurrence`], run by
//! [`Realized`], is one structure that computes it. [`Grows`], [`Capacity`], [`Reach`],
//! [`Domain`], [`Effort`] and [`Cadence`] are the engineering layer: none is a property
//! of the system.
//!
//! # The sets
//!
//! A processor is a family of discrete-time systems indexed by controls, each member
//! computed by a recurrence over its state. The sets, with the state-variable filter and
//! the FDN reverb as examples:
//!
//! | Set | Is | SVF | FDN reverb |
//! |---|---|---|---|
//! | `C` | the controls: a product of intervals of typed units | cutoff, resonance | decay, size, damping |
//! | `D ⊆ C` | the [`Domain`]: the part of `C` a node promises to reach | all of `C` | decay up to a [`MaxDecay`] |
//! | `M` | the members: what fixes one system | the response type and its two coefficients | the network |
//! | `Θ` | the coefficients: the part of a member the recurrence reads per frame | two coefficients | gains, filter coefficients, delay lengths |
//! | `X`, `Y` | an input and an output frame (one value per channel) | one in, one out | two in, two out |
//! | `S` | the state, sized by a [`Capacity`] when its size depends on the member | two integrators | the delay lines |
//!
//! # The maps
//!
//! Each map is one function, and a processor is their composition:
//!
//! | Map | Is | In code |
//! |---|---|---|
//! | `φ: C → M` | the family | [`Family::member_into`] |
//! | `κ: M → Θ` | the coefficients a member asks for | [`Recurrence::coefficients_into`] |
//! | `K: Θ × S × X → Y × S` | the per-frame recurrence | [`Recurrence::run`] |
//! | `γ` | the glide of the coefficients in flight toward `κ(m)` | [`Approach`] |
//! | `σ` | the cadence: how controls become one per frame | [`Cadence`] |
//! | `T: M → (X^ℕ → Y^ℕ)` | the system a member denotes | [`Realized`], written once |
//! | `H` | the transfer function, only for a linear time-invariant member | [`Lti`] |
//! | `need`, `reach` | what a member's state needs, what a domain needs | [`Capacity`], [`Grows::reach`] |
//!
//! In plain text, `P(c, x) = T(γ(κ(φ(c))), x)`, with `c = σ(the host's control writes)`:
//! the output of a processor is its recurrence run over the input, at coefficients that
//! glide toward those of the member the controls select. A trait states only the
//! structure a set needs for a map to apply (an affine coefficient space for the glide,
//! a transfer function for the linear queries, a capacity for state that grows), never a
//! lifecycle.
//!
//! # Classes
//!
//! A class of system is a trait over structure, never one data type, and each class
//! offers the queries its mathematics has. [`Lti`] is the one class with a trait; a
//! system of another kind (a nonlinear one, one that varies in time) answers through its
//! own queries and is not made to implement [`Lti`]. Modulation is not a class: a
//! control that moves is a trajectory of the controls, written by a lane or a control
//! source, and the system it moves is the same one.
//!
//! # What is not forced
//!
//! There is no universal model that every processor must expose, and no interpreter
//! that runs a model in place of its recurrence. A recurrence is a generic type run a
//! block at a time, never a `dyn` called per sample. A recurrence whose structure is
//! fused by hand (the Freeverb recurrence) stays fused: the family states what it
//! computes and the contract holds the render to it.

mod cadence;
mod capacity;
mod family;
mod feed;
mod lti;
mod reach;
mod realized;
mod setup;
mod system;

pub use cadence::{Cadence, Step};
pub use capacity::Capacity;
pub use family::{Channels, Family, Grows};
pub use feed::{Feed, MAX_LANES};
pub use lti::Lti;
pub use reach::{Domain, MaxDecay, MaxDecayError, Reach, TAIL_FLOOR};
pub use realized::{Approach, Glide, Lerp, Readout, Realized, Recurrence, Start, Steps};
pub use setup::{Effort, Setup};
pub use system::{Ring, System};
