//! Work other threads hand to the main thread runs on the session's update.
//!
//! A plugin instance dropped off the thread it was made on (a pool thread at the end of an
//! export, say) cannot close itself there, and must not skip the close: it hands the close to
//! the main thread with `tutti_plugin_types::defer_to_main`, and the session's
//! `Phase::Collect` runs it (`run_deferred`) on the claimed thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tutti_plugin_types::{MainThread, defer_to_main};
use tutti_types::SampleRate;

/// **The session's update runs what another thread deferred, on the main thread.**
#[test]
fn an_update_runs_the_work_a_thread_deferred_to_the_main_thread() {
    let main = MainThread::claim().expect("one test, one process");
    let mut session = tutti_session::Session::builder()
        .outputs_floor(2)
        .build_detached(SampleRate(48_000.0))
        .expect("builds");
    let ran_on_main = Arc::new(AtomicBool::new(false));
    let main_id = std::thread::current().id();
    let flag = Arc::clone(&ran_on_main);
    std::thread::spawn(move || {
        defer_to_main(move |_main| {
            flag.store(std::thread::current().id() == main_id, Ordering::SeqCst);
        });
    })
    .join()
    .unwrap();
    assert!(
        !ran_on_main.load(Ordering::SeqCst),
        "not run by the deferring thread"
    );

    session.update();
    assert!(
        ran_on_main.load(Ordering::SeqCst),
        "the update did not run the deferred work on the main thread"
    );
    let _ = main;
}
