//! A target whose OS main-thread query is not wired.

compile_error!(
    "tutti-rt names the main thread through the OS on Linux, Android, Apple platforms and Windows only"
);

pub(super) fn is_main_thread() -> bool {
    false
}
