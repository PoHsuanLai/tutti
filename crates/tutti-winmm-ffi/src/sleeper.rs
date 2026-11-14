//! One wait that ends at a time or when another thread says so.

use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE,
    SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS, WaitForMultipleObjects,
};

use crate::error::Error;

/// Why [`Sleeper::wait`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Woken {
    /// The time passed (or a timer left over from an earlier wait fired: a
    /// caller looks at the clock again either way).
    Elapsed,
    /// Another thread called [`Sleeper::wake`].
    Signalled,
}

/// A waitable timer, high resolution where Windows has it, and a wake event.
/// One thread waits; any thread wakes it.
#[derive(Debug)]
pub struct Sleeper {
    timer: HANDLE,
    wake: HANDLE,
}

// SAFETY: both handles are kernel objects, valid from any thread for their
// whole life; the calls made on them (`SetEvent`, and the wait and re-arm
// the one waiting thread makes) are thread-safe.
unsafe impl Send for Sleeper {}
// SAFETY: see `Send`.
unsafe impl Sync for Sleeper {}

fn last(operation: &'static str) -> Error {
    // SAFETY: reads the calling thread's last-error value.
    let code = unsafe { GetLastError() };
    Error { operation, code }
}

impl Sleeper {
    /// A sleeper with nothing signalled.
    ///
    /// # Errors
    ///
    /// The failing call, with `GetLastError`'s code.
    pub fn new() -> Result<Self, Error> {
        // SAFETY: null security attributes and name; an auto-reset event that
        // starts unsignalled.
        let wake = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if wake.is_null() {
            return Err(last("CreateEventW"));
        }
        // High resolution needs Windows 10 1803; the call refuses the flag
        // before that, and the plain timer is the fallback.
        // SAFETY: null attributes and name; the flag and access are constants.
        let mut timer = unsafe {
            CreateWaitableTimerExW(
                std::ptr::null(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        if timer.is_null() {
            // SAFETY: as above, without the flag.
            timer = unsafe {
                CreateWaitableTimerExW(std::ptr::null(), std::ptr::null(), 0, TIMER_ALL_ACCESS)
            };
        }
        if timer.is_null() {
            let error = last("CreateWaitableTimerExW");
            // SAFETY: `wake` is the open event made above, closed once.
            unsafe { CloseHandle(wake) };
            return Err(error);
        }
        Ok(Self { timer, wake })
    }

    /// Returns when `timeout` has passed (never, for `None`) or another thread
    /// has called [`wake`](Self::wake). A wake made while nobody waits ends the
    /// next wait at once.
    ///
    /// # Errors
    ///
    /// The failing call, with `GetLastError`'s code.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<Woken, Error> {
        let handles = [self.wake, self.timer];
        let count = match timeout {
            Some(after) => {
                // A negative due time is relative, in 100 ns units; at least
                // one unit, as zero would be absolute time.
                let units = i64::try_from(after.as_nanos() / 100).unwrap_or(i64::MAX);
                let due = -units.max(1);
                // SAFETY: `timer` is the open timer and `due` a live i64; no
                // period and no completion routine.
                let armed = unsafe {
                    SetWaitableTimer(self.timer, &raw const due, 0, None, std::ptr::null(), 0)
                };
                if armed == 0 {
                    return Err(last("SetWaitableTimer"));
                }
                2
            }
            None => 1,
        };
        // SAFETY: `handles` holds `count` open handles, which this sleeper
        // keeps open for the call.
        let result = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, INFINITE) };
        match result.wrapping_sub(WAIT_OBJECT_0) {
            0 => Ok(Woken::Signalled),
            1 => Ok(Woken::Elapsed),
            _ => Err(last("WaitForMultipleObjects")),
        }
    }

    /// Ends the current or next [`wait`](Self::wait).
    ///
    /// # Errors
    ///
    /// `SetEvent`'s, with `GetLastError`'s code.
    pub fn wake(&self) -> Result<(), Error> {
        // SAFETY: `wake` is the open event.
        if unsafe { SetEvent(self.wake) } == 0 {
            return Err(last("SetEvent"));
        }
        Ok(())
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        // SAFETY: both are open handles, each closed once.
        unsafe {
            CloseHandle(self.timer);
            CloseHandle(self.wake);
        }
    }
}
