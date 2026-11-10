//! The contract: cases every [`Design`](crate::Design) and its
//! [`Realization`](crate::Realization) are held to (design 024 section 6).
//!
//! Written once per class of system, behind the `contract` feature (off by
//! default; it pulls `rand` for seeded noise). This is the one public module of
//! the crate: everything else is exported at the root, and this module is a
//! harness a family's own tests run, so it has paths of its own.
//!
//! A family implements [`DesignCase`] (the controls to sweep) and, for the
//! measured cases, [`RealizationCase`]. The cases come in **groups**, a family
//! opts into by naming them:
//!
//! - **core**, for every family: the bounds dominate every design, a design is
//!   pure, the realization declares the model's latency and places its first
//!   sound where the model says, a reset or a fresh realization renders as a
//!   freshly built one, the output does not depend on the block length, a
//!   retarget does not click, the first retarget lands at once
//!   (`landed`), the ramp's clock is the stream's (`stream_clock`) and no hook
//!   is left empty without saying why (`vacuity`);
//! - **`Lti`** ([`LtiCase`]), for a model that is [`Lti`](crate::Lti): the
//!   spectral radius is under 1, the response meets the family's closed-form
//!   anchors, the norms bound the measured peak and are not far above it, and
//!   the realization is linear, shift invariant, has the model's impulse
//!   response and decay, and stays under its l1 norm;
//! - **`reverb`** ([`ReverbCase`]), for a [`Network`](crate::Network): the RT60s,
//!   the band order, the tail, the freeze, the densities and the controls a
//!   reverb has and a filter does not.
//!
//! ```text
//! design_contract!(MyDesign, Lti);            // the design cases
//! realization_contract!(MyDesign, Lti);       // the measured cases
//! ```
//!
//! Each macro makes one `#[test]` per case. Every figure is analytic or from the
//! spec, never another implementation's render. Each case is a plain function
//! from a design to a `Result` ([`cases`]), so a test can hand it a deliberately
//! broken design and assert it fails: the mutation record of a case is a test.
//! The broken designs themselves (`Defect`, `Flawed`, `Defective`, `Mutant`,
//! `ToyFdn`) are this crate's own tests'.
//!
//! Allocation-freedom of `design`, `retarget` and `process` needs a counting
//! global allocator, which `unsafe` forbids in this `forbid(unsafe_code)` crate:
//! `tests/network_alloc.rs` runs it.

// Every case returns `Err` with the message of the first figure that fails, and
// says what it holds in its own docs.
#![allow(clippy::missing_errors_doc)]

mod basic;
mod design_case;
#[cfg(test)]
pub(crate) mod flaws;
mod harness;
mod lti;
mod macros;
mod reverb;

pub use design_case::{Anchor, DesignCase, Hook, LtiCase, RealizationCase, ReverbCase};
pub use harness::{
    Burst, MAX_BLOCK, WINDOW, built, burst, energy, noise, render, rms, setup, widths,
};

/// The cases, one function each, by level and group.
///
/// `design` cases need only a design; `realization` cases render it.
pub mod cases {
    /// The design cases (no realization, no audio).
    pub mod design {
        pub use crate::contract::basic::design::*;
        pub use crate::contract::lti::design::*;
        pub use crate::contract::reverb::design::*;
    }

    /// The realization cases, measured on the output.
    pub mod realization {
        pub use crate::contract::basic::realization::*;
        pub use crate::contract::lti::realization::*;
        pub use crate::contract::reverb::realization::*;
    }
}
