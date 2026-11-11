//! The Mach host clock, the one CoreMIDI timestamps its packets with.
//!
//! An event packet carries `timeStamp`: the ticks of `mach_absolute_time`,
//! where 0 means "now". The tick is not a nanosecond (`mach_timebase_info`
//! gives the ratio).

use std::sync::OnceLock;
use std::time::Duration;

use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_time::{mach_absolute_time, mach_timebase_info, mach_timebase_info_data_t};

/// The host clock's tick as a ratio of nanoseconds: `nanos = ticks * numer / denom`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timebase {
    numer: u32,
    denom: u32,
}

impl Timebase {
    /// A ratio of `numer` to `denom`; `None` when `denom` is 0.
    #[must_use]
    pub fn new(numer: u32, denom: u32) -> Option<Self> {
        (denom != 0).then_some(Self { numer, denom })
    }

    /// The machine's, read once.
    #[must_use]
    pub fn get() -> Self {
        static BASE: OnceLock<Timebase> = OnceLock::new();
        *BASE.get_or_init(|| {
            let mut info = mach_timebase_info_data_t { numer: 0, denom: 0 };
            // SAFETY: `info` is a valid out-pointer.
            let rc = unsafe { mach_timebase_info(&mut info) };
            // The call cannot fail on a supported macOS; a 1:1 base is what an
            // Intel Mac reports, and the only sane answer to fall back on.
            if rc != KERN_SUCCESS {
                return Timebase { numer: 1, denom: 1 };
            }
            Timebase::new(info.numer, info.denom).unwrap_or(Timebase { numer: 1, denom: 1 })
        })
    }

    /// `ticks` of host time as a duration, rounded down; `u128` so a machine's
    /// uptime times `numer` cannot overflow.
    #[must_use]
    pub fn duration(self, ticks: u64) -> Duration {
        let nanos = u128::from(ticks) * u128::from(self.numer) / u128::from(self.denom);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

/// The host clock now, in ticks. Allocation-free, so it runs in CoreMIDI's
/// callback.
#[must_use]
pub fn host_ticks() -> u64 {
    // SAFETY: no arguments, no state.
    unsafe { mach_absolute_time() }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Apple Silicon's base is 125/3: 3 ticks are 125 ns, so 24 000 000 ticks (one
    // of its seconds is 24 MHz) are 1 s.
    #[test]
    fn a_tick_is_the_timebases_ratio_of_a_nanosecond() {
        let base = Timebase::new(125, 3).unwrap();
        assert_eq!(base.duration(24_000_000), Duration::from_secs(1));
        assert_eq!(base.duration(3), Duration::from_nanos(125));
        // An uptime of years of ticks does not overflow the product.
        assert!(base.duration(u64::MAX) > Duration::from_secs(1 << 30));
    }

    // A base with no denominator cannot be built, so `duration` never divides by
    // zero.
    #[test]
    fn a_timebase_needs_a_denominator() {
        assert_eq!(Timebase::new(1, 0), None);
    }

    // The machine's base reads a positive duration for a tick count.
    #[test]
    fn the_machines_base_is_usable() {
        assert!(Timebase::get().duration(host_ticks()) > Duration::ZERO);
    }
}
