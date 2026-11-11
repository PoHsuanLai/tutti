//! Test-only support for this crate's unit tests: the signal helpers, and the
//! deliberately broken designs and realizations the contract's cases are shown to
//! fail on ([`flaws`]).

pub(crate) mod flaws;
mod signals;

pub(crate) use signals::{amplitude_at, lock_in};
