//! CoreMIDI's host time as an `Instant`.
//!
//! An event packet carries `timeStamp`: mach host time, the ticks of
//! `mach_absolute_time`, where 0 means "now". The tick is not a nanosecond
//! (`mach_timebase_info` gives the ratio), and `Instant` has no constructor from
//! a mach reading, so a callback reads the host clock and `Instant::now()`
//! together once ([`Pairing::now`]) and each packet is placed before that
//! reading by its stamp ([`crate::stamp::arrival`]).
//!
//! Not verified on a Mac: this file was written and compiled against the
//! macOS bindings by reading, not run.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_time::{mach_absolute_time, mach_timebase_info, mach_timebase_info_data_t};

/// The host clock's tick as a ratio of nanoseconds: `nanos = ticks * numer / denom`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Timebase {
    numer: u32,
    denom: u32,
}

impl Timebase {
    /// The machine's, read once.
    pub(super) fn get() -> Self {
        static BASE: OnceLock<Timebase> = OnceLock::new();
        *BASE.get_or_init(|| {
            let mut info = mach_timebase_info_data_t { numer: 0, denom: 0 };
            // SAFETY: `info` is a valid out-pointer.
            let rc = unsafe { mach_timebase_info(&mut info) };
            // The call cannot fail on a supported macOS; a 1:1 base is what an
            // Intel Mac reports, and the only sane answer to fall back on.
            if rc != KERN_SUCCESS || info.denom == 0 {
                return Timebase { numer: 1, denom: 1 };
            }
            Timebase {
                numer: info.numer,
                denom: info.denom,
            }
        })
    }

    /// `ticks` of host time as a duration, rounded down; `u128` so a machine's
    /// uptime times `numer` cannot overflow.
    pub(super) fn duration(self, ticks: u64) -> Duration {
        let nanos = u128::from(ticks) * u128::from(self.numer) / u128::from(self.denom);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

/// The host clock and the OS's read together, to place a callback's packets.
#[derive(Clone, Copy, Debug)]
pub(super) struct Pairing {
    base: Timebase,
    os: Instant,
    host: Duration,
}

impl Pairing {
    /// Reads both clocks now. Allocation-free, so it runs in CoreMIDI's callback.
    pub(super) fn now() -> Self {
        let base = Timebase::get();
        // SAFETY: no arguments, no state.
        let host = base.duration(unsafe { mach_absolute_time() });
        Self {
            base,
            os: Instant::now(),
            host,
        }
    }

    /// The instant of a packet stamped `host_time` ticks; a 0 stamp ("now") is
    /// the pairing's own.
    pub(super) fn instant_of(&self, host_time: u64) -> Instant {
        if host_time == 0 {
            return self.os;
        }
        crate::stamp::arrival(self.os, self.host, self.base.duration(host_time))
    }
}

#[cfg(test)]
mod tests;
