//! What a plugin crate's tests share (feature `plugin`).

use tutti_plugin_types::MainThread;

/// This process's main thread, claimed by the calling thread. Each test is its
/// own process under nextest, so the claim cannot be refused; a test that calls
/// it from two threads panics, which is the point.
pub fn main_thread() -> MainThread {
    MainThread::claim().expect("one test, one process")
}
