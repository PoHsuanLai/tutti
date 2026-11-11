//! An OS timestamp as an `Instant`: the one rule both backends use.
//!
//! An OS stamps an event on a clock of its own (an ALSA queue's real time since
//! it started, CoreMIDI's mach host time). A backend reads that clock and
//! `Instant::now()` together once, and each event's instant is then as long
//! before that reading as its stamp is before the clock's. Nothing here reads a
//! clock.

use std::time::{Duration, Instant};

/// The instant of an event stamped `stamped` on a clock that read `clock` when
/// the OS's read `os`: as long before `os` as `stamped` is before `clock`. An
/// event stamped after the reading (it should not be), or one that reaches back
/// past what an `Instant` can hold, is at `os`.
#[cfg_attr(
    not(any(target_os = "macos", all(target_os = "linux", alsa_ump))),
    allow(dead_code, reason = "only the OS backends convert a stamp")
)]
pub(crate) fn arrival(os: Instant, clock: Duration, stamped: Duration) -> Instant {
    os.checked_sub(clock.saturating_sub(stamped)).unwrap_or(os)
}

#[cfg(test)]
mod tests;
