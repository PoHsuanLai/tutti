//! The operating-system calls of the plugin process edge, each behind a safe
//! function: the server's wait for its host to connect and its check that the
//! host is still running, and the host's peek at a named pipe.
//!
//! Each is a direct call with scalar or borrowed arguments; the policy around
//! it (what a failure to ask means, how long to wait) stays with the caller.

#[cfg(unix)]
use rustix::event::{PollFd, PollFlags};
#[cfg(unix)]
use std::os::fd::BorrowedFd;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, BorrowedHandle};

/// Waits up to `timeout_ms` for `fd` to become readable (for a listener: a
/// connection is queued). `Ok(false)` on a timeout.
///
/// # Errors
///
/// The OS error of `poll`, [`Interrupted`](std::io::ErrorKind::Interrupted)
/// included: a signal during the wait is the caller's to retry.
#[cfg(unix)]
pub fn wait_readable(fd: BorrowedFd<'_>, timeout_ms: i32) -> std::io::Result<bool> {
    let mut polls = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
    Ok(rustix::event::poll(&mut polls, timeout(timeout_ms).as_ref())? > 0)
}

/// A millisecond timeout as `poll`'s: negative waits for ever.
#[cfg(unix)]
fn timeout(ms: i32) -> Option<rustix::event::Timespec> {
    u64::try_from(ms).ok().map(|ms| rustix::event::Timespec {
        tv_sec: (ms / 1000) as _,
        tv_nsec: ((ms % 1000) * 1_000_000) as _,
    })
}

/// The readiness bit for "readable" in [`Poller`]'s interest and answers.
#[cfg(unix)]
pub const READ: u8 = 1 << 0;
/// The readiness bit for "writable".
#[cfg(unix)]
pub const WRITE: u8 = 1 << 1;
/// The readiness bit for "in an error state" (a hung-up or invalid descriptor
/// included).
#[cfg(unix)]
pub const ERROR: u8 = 1 << 2;

/// Waits on one descriptor of the caller's own and any number of others a
/// plugin asked to have watched, in one `poll(2)`.
///
/// A plugin's descriptor arrives as a plain number, and borrowing it is the one
/// `unsafe` of this type.
///
/// Interest and answers are the [`READ`], [`WRITE`] and [`ERROR`] bits.
#[cfg(unix)]
#[derive(Debug, Default)]
pub struct Poller;

#[cfg(unix)]
impl Poller {
    /// Waits up to `timeout_ms` for `own` to be readable (a hang-up counts: the
    /// read reports it) or for any of `watched` (descriptor, interest bits) to
    /// be ready, and writes the ready ones, with only the bits they asked for,
    /// into `ready` (replacing its contents). Returns whether `own` is
    /// readable; `false` on a timeout.
    ///
    /// A descriptor that is not open answers [`ERROR`], never a fault. A zero
    /// timeout never blocks.
    ///
    /// # Errors
    ///
    /// The OS error of `poll`, [`Interrupted`](std::io::ErrorKind::Interrupted)
    /// included: a signal during the wait is the caller's to retry.
    pub fn wait(
        &mut self,
        own: Option<BorrowedFd<'_>>,
        watched: &[(i32, u8)],
        timeout_ms: i32,
        ready: &mut Vec<(i32, u8)>,
    ) -> std::io::Result<bool> {
        ready.clear();
        if own.is_none() && watched.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(
                u64::try_from(timeout_ms).unwrap_or(0),
            ));
            return Ok(false);
        }
        let mut polls: Vec<PollFd<'_>> = Vec::with_capacity(watched.len() + 1);
        polls.extend(own.map(|own| PollFd::from_borrowed_fd(own, PollFlags::IN)));
        let skip = polls.len();
        polls.extend(watched.iter().map(|&(fd, interest)| {
            let mut events = PollFlags::empty();
            if interest & READ != 0 {
                events |= PollFlags::IN;
            }
            if interest & WRITE != 0 {
                events |= PollFlags::OUT;
            }
            // SAFETY: the descriptor is a plain number the plugin registered
            // with the host, borrowed only for this one `poll` call, which its
            // registration keeps open; if it is not open `poll` answers
            // `NVAL` and nothing is read from or written to it.
            let fd = unsafe { BorrowedFd::borrow_raw(fd) };
            PollFd::from_borrowed_fd(fd, events)
        }));
        if rustix::event::poll(&mut polls, timeout(timeout_ms).as_ref())? == 0 {
            return Ok(false);
        }
        let own_ready = skip == 1 && !polls[0].revents().is_empty();
        ready.extend(
            polls[skip..]
                .iter()
                .zip(watched)
                .filter_map(|(poll, &(fd, interest))| {
                    let got = poll.revents();
                    let mut now = 0;
                    if got.intersects(PollFlags::IN | PollFlags::HUP) {
                        now |= READ;
                    }
                    if got.contains(PollFlags::OUT) {
                        now |= WRITE;
                    }
                    if got.intersects(PollFlags::ERR | PollFlags::NVAL) {
                        now |= ERROR;
                    }
                    let now = now & interest;
                    (now != 0).then_some((fd, now))
                }),
        );
        Ok(own_ready)
    }
}

/// Looks, without waiting, at which of `watched` (descriptor, interest bits)
/// are ready, into `ready` (replacing its contents), through a [`Poller`] whose
/// scratch the calling thread keeps. A failed look is none ready: the
/// descriptors are looked at again on the next call.
#[cfg(unix)]
pub fn poll_now(watched: &[(i32, u8)], ready: &mut Vec<(i32, u8)>) {
    ready.clear();
    if !watched.is_empty() && Poller.wait(None, watched, 0, ready).is_err() {
        ready.clear();
    }
}

/// Whether `pid` names a live process, without signalling it.
///
/// `kill(pid, 0)` runs the permission and existence checks and delivers
/// nothing. `EPERM` means it exists but is not ours to signal, which is still
/// "alive"; only `ESRCH` is a definite death. Anything else is a failure to
/// ask, and a failure to ask counts as alive, erring toward waiting rather than
/// toward a server that quits on a live host.
#[cfg(unix)]
#[must_use]
pub fn process_exists(pid: i32) -> bool {
    use rustix::io::Errno;
    // A number that is no process id (zero, negative) is a failure to ask.
    let Some(pid) = rustix::process::Pid::from_raw(pid) else {
        return true;
    };
    !matches!(rustix::process::test_kill_process(pid), Err(Errno::SRCH))
}

/// Whether the process `pid` is still running, on Windows.
///
/// **"Cannot determine" means alive** — only a positive answer counts as death.
/// `OpenProcess` returns null for *access denied* as readily as for *no such
/// process*, so a host at a higher integrity level than its plugin server (a
/// UAC-elevated DAW) would otherwise read as gone. `GetLastError` tells them
/// apart: `ERROR_INVALID_PARAMETER` is what Windows reports for a PID naming no
/// live process, the one unambiguous "it is gone".
#[cfg(windows)]
#[must_use]
pub fn process_exists(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // SAFETY: `OpenProcess` takes only scalars. A null return means the process
    // is gone or inaccessible; the handle is closed on every path below.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        // SAFETY: reads this thread's last-error value, set by the call above.
        let err = unsafe { GetLastError() };
        return err != ERROR_INVALID_PARAMETER;
    }
    // A process handle becomes signalled when the process exits, so a zero-length
    // wait is a liveness probe. Test for the *signalled* result specifically:
    // timing out means alive, but so does `WAIT_FAILED`, a failure to ask.
    // SAFETY: `handle` was just opened above and is still valid.
    let alive = unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0;
    // SAFETY: closing a handle opened above and not yet closed.
    unsafe { CloseHandle(handle) };
    alive
}

/// This process's parent PID, from the process table (Windows has no
/// `getppid`). `None` if the snapshot fails or this process is not in it.
#[cfg(windows)]
#[must_use]
pub fn parent_process_id() -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let me = std::process::id();
    // SAFETY: scalar arguments; the returned handle is closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }

    // SAFETY: `PROCESSENTRY32W` is a plain C struct of integers and a `u16`
    // array, for which all-zero is a valid value.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    let mut found = None;
    // SAFETY: `snapshot` is valid and `entry` is a correctly sized, live
    // `PROCESSENTRY32W` for the duration of the walk.
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while ok {
        if entry.th32ProcessID == me {
            found = Some(entry.th32ParentProcessID);
            break;
        }
        // SAFETY: as above.
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: closing a handle opened above and not yet closed.
    unsafe { CloseHandle(snapshot) };
    found
}

/// Bytes the named pipe can hand over right now, without blocking or
/// consuming them (`PeekNamedPipe` as a pure size query).
///
/// `Ok(0)` means "nothing yet", never end of stream. A closed peer makes this
/// fail (`ERROR_BROKEN_PIPE`).
///
/// # Errors
///
/// The OS error of `PeekNamedPipe`.
#[cfg(windows)]
pub fn pipe_bytes_available(pipe: BorrowedHandle<'_>) -> std::io::Result<u32> {
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let mut available: u32 = 0;
    // SAFETY: `pipe` is borrowed for this call. The only out-pointer is
    // `available`, a live local; every other buffer argument is null, which is
    // what makes this a pure size query: nothing is copied out of the pipe and
    // the stream position does not move.
    let ok = unsafe {
        PeekNamedPipe(
            pipe.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(available)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// This process exists; a PID far past `pid_max` does not.
    #[test]
    fn process_exists_tells_a_live_pid_from_a_dead_one() {
        assert!(process_exists(std::process::id() as i32));
        assert!(!process_exists(i32::MAX));
    }

    /// A pipe's read end is not readable until something is written to it.
    #[test]
    fn wait_readable_waits_for_data() {
        use std::io::Write;
        use std::os::fd::AsFd;
        let (reader, mut writer) = std::io::pipe().unwrap();
        assert!(!wait_readable(reader.as_fd(), 0).unwrap());
        writer.write_all(b"x").unwrap();
        assert!(wait_readable(reader.as_fd(), 1000).unwrap());
    }

    /// **A watched descriptor is reported when it is readable and not before,
    /// with the bits it asked for; the caller's own descriptor is reported
    /// apart.** The instant a byte arrives ends a long wait.
    #[test]
    fn a_poller_reports_a_watched_descriptor_when_it_is_readable() {
        use std::io::Write;
        use std::os::fd::{AsFd, AsRawFd};
        let (watched, mut writer) = std::io::pipe().unwrap();
        let (own, _own_writer) = std::io::pipe().unwrap();
        let mut poller = Poller;
        let mut ready = Vec::new();
        let fd = watched.as_raw_fd();
        let own_ready = poller
            .wait(Some(own.as_fd()), &[(fd, READ)], 0, &mut ready)
            .unwrap();
        assert!(!own_ready && ready.is_empty(), "nothing yet");
        writer.write_all(b"x").unwrap();
        let started = std::time::Instant::now();
        let own_ready = poller
            .wait(Some(own.as_fd()), &[(fd, READ)], 5000, &mut ready)
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(1000));
        assert!(!own_ready, "the caller's own descriptor is apart");
        assert_eq!(ready, [(fd, READ)]);
        // A write interest it did not ask for is not reported; a closed
        // descriptor is an error, not a fault.
        poller.wait(None, &[(fd, WRITE)], 0, &mut ready).unwrap();
        assert!(ready.is_empty(), "a pipe's read end is never writable");
        // A descriptor that is not open is an error for whoever asked for
        // errors, never a fault.
        drop(watched);
        poller
            .wait(None, &[(fd, READ | ERROR)], 0, &mut ready)
            .unwrap();
        assert_eq!(ready, [(fd, ERROR)]);
    }
}
