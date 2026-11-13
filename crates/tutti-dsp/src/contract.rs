//! The contract: cases every [`Family`](crate::Family) and its
//! [`Realized`](crate::Realized) runner are held to.
//!
//! Written once per class of system, behind the `contract` feature (off by
//! default; it pulls `rand` for seeded noise). It is a feature of the crate, not a
//! crate of its own: a separate test crate that depended on `tutti-dsp` while
//! `tutti-dsp`'s own unit tests depended on it would give those tests two copies of
//! `tutti-dsp`, whose [`Family`](crate::Family) types would not be the same type.
//! This is the one public module of
//! the crate: everything else is exported at the root, and this module is a
//! harness a family's own tests run, so it has paths of its own.
//!
//! A family implements [`FamilyCase`] (the controls to sweep) and, for the
//! measured cases, [`RecurrenceCase`]. The cases come in **groups**, a family
//! opts into by naming them:
//!
//! - **core**, for every family: a member is pure in its controls, the runner
//!   declares the member's latency and places its first sound where the member
//!   says, a reset or a fresh realization renders as a
//!   freshly built one, a
//!   retarget does not click, the first retarget lands at once
//!   (`landed`) and no hook
//!   is left empty without saying why (`vacuity`);
//! - **`Grows`**, for a [`Grows`](crate::Grows) family and only for one (a filter's
//!   capacity is `()`, which has no figure to cover): `reach` covers the `need` of
//!   every member (`reach_covers_need`);
//! - **`BlockIndependent`**, for a family whose `CADENCE` is
//!   `block_independent()` (and only for one: `cadence` fails both ways): blocks
//!   of any length render alike (`block_independent`), also while the controls
//!   move (`stream_clock`);
//! - **`Lti`** ([`LtiCase`]), for a model that is [`Lti`](crate::Lti): the
//!   spectral radius is under 1, the response meets the family's closed-form
//!   figures, the norms bound the measured peak and are not far above it, and
//!   the realization is linear, shift invariant, has the model's impulse
//!   response and decay, and stays under its l1 norm;
//! - **`reverb`** ([`ReverbCase`]), for a [`Network`](crate::Network): the RT60s,
//!   the band order, the tail, the freeze, the densities and the controls a
//!   reverb has and a filter does not.
//!
//! ```text
//! family_contract!(MyFamily, Lti);            // the family cases
//! recurrence_contract!(MyFamily, Lti);        // the measured cases
//! ```
//!
//! Each macro makes one `#[test]` per case. Every figure is analytic or from the
//! spec, never another implementation's render. Each case is a plain function
//! from a family to a `Result` ([`cases`]).
//!
//! `recurrence_contract!` states the three laws, and
//! the macro is laid out by them: the recurrence **realizes the member** (what a
//! render does is what the member's queries say), the **block form is the frame
//! form** (cutting the input into blocks changes nothing), and the **glide lands**
//! (a retarget approaches without a click and rests on the member). `family_contract!`
//! is the diagram of the member's own queries.
//!
//! Allocation-freedom of `member`, `retarget` and `process` needs a counting
//! global allocator, which `unsafe` forbids in this `forbid(unsafe_code)` crate:
//! `tests/network_alloc.rs` runs it.

// Every case returns `Err` with the message of the first figure that fails, and
// says what it holds in its own docs.
#![allow(clippy::missing_errors_doc)]

mod core;
mod family_case;
mod harness;
mod lti;
mod macros;
mod reverb;

pub use family_case::{FamilyCase, Figure, Hook, LtiCase, RecurrenceCase, ReverbCase};
pub use harness::{
    Burst, MAX_BLOCK, WINDOW, Widths, built, burst, energy, noise, render, rms, setup, widths,
};

/// The cases, one function each, by level and group.
///
/// `design` cases need only a family; `realization` cases render it.
pub mod cases {
    /// The family cases (no runner, no audio).
    pub mod design {
        pub use crate::contract::core::design::*;
        pub use crate::contract::lti::design::*;
        pub use crate::contract::reverb::design::*;
    }

    /// The recurrence cases, measured on the output, by three laws:
    ///
    /// 1. the recurrence **realizes the member**: `latency`, `arrival`, `vacuity`,
    ///    and with `Lti` `tail_covers`, `tail_tight`, `stability`, `linear`,
    ///    `response`, `decay`, and with `reverb` `rt60`, `band_order`, `wet_only`,
    ///    `decorrelated`, `line_lengths`, `pre_delay`, `size`, `diffusion`,
    ///    `freeze`, `width`, `wet_gain`, `reverb_vacuity`;
    /// 2. the **block form is the frame form**: `cadence`, `block_independent`,
    ///    `stream_clock`;
    /// 3. the **glide lands**: `landed`, `click_free`, `reset`, `fresh`.
    pub mod realization {
        pub use crate::contract::core::realization::*;
        pub use crate::contract::lti::realization::*;
        pub use crate::contract::reverb::realization::*;
    }
}
