//! Windows: the primary thread, the one with the earliest creation time among
//! the process's threads. Windows has no "is the main thread" call; the
//! loader starts the primary thread first, so it is the oldest, and it keeps
//! its id for the life of the process.

use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, GetThreadTimes, OpenThread,
    THREAD_QUERY_LIMITED_INFORMATION,
};

/// The primary thread's id, found once (0 is unfound; no thread has id 0).
static PRIMARY: AtomicU32 = AtomicU32::new(0);

pub(super) fn is_main_thread() -> bool {
    let mut primary = PRIMARY.load(Ordering::Relaxed);
    if primary == 0 {
        primary = oldest_thread();
        PRIMARY.store(primary, Ordering::Relaxed);
    }
    // SAFETY: `GetCurrentThreadId` takes no arguments and cannot fail.
    primary != 0 && primary == unsafe { GetCurrentThreadId() }
}

/// The id of the process's thread with the earliest creation time, or 0 if the
/// threads cannot be listed.
fn oldest_thread() -> u32 {
    // SAFETY: `GetCurrentProcessId` takes no arguments and cannot fail.
    let pid = unsafe { GetCurrentProcessId() };
    // SAFETY: a thread snapshot ignores its process argument.
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else {
        return 0;
    };
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut oldest = (u64::MAX, 0u32);
    // SAFETY: `snapshot` is a live snapshot handle and `entry` is a writable
    // `THREADENTRY32` whose `dwSize` is set.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) }.is_ok();
    while more {
        if entry.th32OwnerProcessID == pid
            && let Some(created) = creation_time(entry.th32ThreadID)
            && created < oldest.0
        {
            oldest = (created, entry.th32ThreadID);
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot, &mut entry) }.is_ok();
    }
    // SAFETY: `snapshot` is a handle this function owns and closes once.
    let _ = unsafe { CloseHandle(snapshot) };
    oldest.1
}

/// A thread's creation time as 100 ns ticks, or `None` if it cannot be read
/// (the thread exited since the snapshot).
fn creation_time(id: u32) -> Option<u64> {
    // SAFETY: opening a thread by id has no precondition; the result is checked.
    let thread = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, id) }.ok()?;
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: `thread` is a live handle with query access and the four
    // `FILETIME`s are writable.
    let read = unsafe { GetThreadTimes(thread, &mut created, &mut exited, &mut kernel, &mut user) };
    // SAFETY: `thread` is a handle this function owns and closes once.
    let _ = unsafe { CloseHandle(thread) };
    read.ok()?;
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}
