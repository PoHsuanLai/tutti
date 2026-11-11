//! The operating-system calls of the plugin process edge, each behind a safe
//! function: the server's wait for its host to connect and its check that the
//! host is still running, and the host's peek at a named pipe.
//!
//! Each is a direct call with scalar or borrowed arguments; the policy around
//! it (what a failure to ask means, how long to wait) stays with the caller.

#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd};
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
    let mut pollfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pollfd` is a live, correctly initialised single-element array,
    // and `fd` is borrowed for the duration of this call.
    let n = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(n > 0)
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
    // SAFETY: signal 0 sends nothing; `kill` only performs its checks. Scalar
    // arguments, no memory touched.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
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
    ///
    /// Mutation (run): return `true` unconditionally → the dead PID reads alive
    /// → fails.
    #[test]
    fn process_exists_tells_a_live_pid_from_a_dead_one() {
        assert!(process_exists(std::process::id() as i32));
        assert!(!process_exists(i32::MAX));
    }

    /// A pipe's read end is not readable until something is written to it.
    ///
    /// Mutation (run): return `n >= 0` → the empty pipe reads ready → fails.
    #[test]
    fn wait_readable_waits_for_data() {
        use std::io::Write;
        use std::os::fd::AsFd;
        let (reader, mut writer) = std::io::pipe().unwrap();
        assert!(!wait_readable(reader.as_fd(), 0).unwrap());
        writer.write_all(b"x").unwrap();
        assert!(wait_readable(reader.as_fd(), 1000).unwrap());
    }
}
