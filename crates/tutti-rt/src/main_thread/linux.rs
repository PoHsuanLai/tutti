//! Linux and Android: the main thread is the thread group leader, whose thread
//! id is the process id.

pub(super) fn is_main_thread() -> bool {
    // SAFETY: `SYS_gettid` takes no arguments, touches no memory and cannot
    // fail. The raw syscall is used because the `gettid` wrapper needs glibc 2.30.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    // SAFETY: `getpid` takes no arguments, touches no memory and cannot fail.
    let pid = unsafe { libc::getpid() };
    // The `as` narrows the syscall's `c_long` return to `pid_t`: `gettid(2)`
    // documents its result as a `pid_t`, so no bits are lost.
    tid as libc::pid_t == pid
}
