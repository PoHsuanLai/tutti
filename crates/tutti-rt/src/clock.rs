//! The OS monotonic clock, in nanoseconds: the clock a hosted plugin compares
//! its transport's system time against (VST3 `systemTime`, VST2
//! `nanoSeconds`, Core Audio's host time).
//!
//! - Linux and the other Unixes: `CLOCK_MONOTONIC` (a vDSO read on Linux).
//! - macOS: `CLOCK_UPTIME_RAW`, which is `mach_absolute_time` in nanoseconds:
//!   Core Audio's host time (`AudioTimeStamp::mHostTime`, read through
//!   `AudioConvertHostTimeToNanos`) is that clock.
//! - Windows: `QueryPerformanceCounter`, scaled to nanoseconds by its
//!   frequency (fixed at boot).
//!
//! The OS's epoch, not one this process started: a plugin in another process
//! (a plugin server) reads the same timebase without being handed an origin,
//! which is why `std::time::Instant` (an opaque, process-local value) does not
//! do.

/// The OS monotonic clock now, in nanoseconds since the OS's own epoch (see
/// the module docs). Audio-thread safe: no lock, no allocation and no syscall
/// beyond the vDSO's or QPC's. 2^63 ns is 292 years of uptime, so the
/// saturation is unreachable.
///
/// A bare `i64` of nanoseconds, not `Seconds`: `Seconds` is an `f64`, and
/// every reader hands this exact integer count on (VST3's `systemTime`, the
/// plugin wire), where a float would round it.
#[must_use]
pub fn monotonic_ns() -> i64 {
    os_monotonic_ns()
}

#[cfg(unix)]
fn os_monotonic_ns() -> i64 {
    #[cfg(target_vendor = "apple")]
    const CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_vendor = "apple"))]
    const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable `timespec`, and the clock id is one
    // every Unix (or, on Apple, macOS 10.12+) provides. It cannot fail with
    // those, and a failure would leave `ts` zero, a valid reading.
    unsafe { libc::clock_gettime(CLOCK, &mut ts) };
    // The `timespec` field types are platform-sized; widen to `i64`.
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let (secs, nanos) = (i64::from(ts.tv_sec as i64), i64::from(ts.tv_nsec as i64));
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

#[cfg(windows)]
fn os_monotonic_ns() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    // The frequency is fixed at boot: read once, then cached (0 is unread).
    static FREQUENCY: AtomicI64 = AtomicI64::new(0);
    let mut freq = FREQUENCY.load(Ordering::Relaxed);
    if freq <= 0 {
        // SAFETY: a valid, writable `i64`. It cannot fail on Windows XP and
        // later; a failure leaves it 0, handled below.
        let _ = unsafe { QueryPerformanceFrequency(&mut freq) };
        if freq <= 0 {
            return 0;
        }
        FREQUENCY.store(freq, Ordering::Relaxed);
    }
    let mut count = 0i64;
    // SAFETY: as above.
    let _ = unsafe { QueryPerformanceCounter(&mut count) };
    // Whole seconds and the remainder apart, so the scaling cannot overflow.
    let (secs, rest) = (count / freq, count % freq);
    secs.saturating_mul(1_000_000_000)
        .saturating_add(rest * 1_000_000_000 / freq)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The clock is the OS's**, not one this process started: on Unix a
    /// reading sits between two direct reads of the clock it names.
    ///
    /// Mutation (run in `tutti-plugin`, where this lived): count from a
    /// process-local `Instant` origin → the reading is milliseconds, the OS's
    /// is the uptime → fails. Mutation: `CLOCK_REALTIME` → decades past the
    /// uptime → fails.
    #[cfg(unix)]
    #[test]
    fn the_clock_is_the_os_monotonic_clock() {
        #[cfg(target_vendor = "apple")]
        const CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
        #[cfg(not(target_vendor = "apple"))]
        const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
        let direct = || {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: a valid, writable `timespec`.
            unsafe { libc::clock_gettime(CLOCK, &mut ts) };
            #[allow(clippy::unnecessary_cast)]
            let ns = ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64;
            ns
        };
        let a = direct();
        let x = monotonic_ns();
        let b = direct();
        assert!(a <= x && x <= b, "{a} <= {x} <= {b}");
    }

    /// Two readings never go back.
    ///
    /// Mutation (run): returning `-now` → the second is below the first →
    /// fails.
    #[test]
    fn it_never_goes_back() {
        let a = monotonic_ns();
        let b = monotonic_ns();
        assert!(a > 0 && b >= a, "{a} then {b}");
    }
}
