//! Linux and Android: the main thread is the thread group leader, whose thread
//! id is the process id.

pub(super) fn is_main_thread() -> bool {
    // SAFETY: `gettid` and `getpid` take no arguments, touch no memory and
    // cannot fail.
    unsafe { libc::gettid() == libc::getpid() }
}
