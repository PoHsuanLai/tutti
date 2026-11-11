//! CoreMIDI's host time as an `Instant`.
//!
//! An event packet carries `timeStamp`: mach host time, the ticks of
//! `mach_absolute_time`, where 0 means "now". The tick is not a nanosecond
//! (`mach_timebase_info` gives the ratio), and `Instant` has no constructor from
//! a mach reading, so a callback reads the host clock and `Instant::now()`
//! together once ([`Pairing::now`]) and each packet is placed before that
//! reading by its stamp ([`crate::stamp::arrival`]).
//!
//! The Mach clock itself is `tutti-coremidi-ffi`'s (`host_ticks`, `Timebase`).

use std::time::{Duration, Instant};

use tutti_coremidi_ffi::{Timebase, host_ticks};

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
        let host = base.duration(host_ticks());
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
