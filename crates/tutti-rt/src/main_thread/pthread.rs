//! Apple platforms, FreeBSD, DragonFly and OpenBSD: `pthread_main_np` answers whether the calling thread is the
//! process's main thread.

pub(super) fn is_main_thread() -> bool {
    // SAFETY: `pthread_main_np` takes no arguments, touches no memory and
    // cannot fail.
    unsafe { libc::pthread_main_np() != 0 }
}
