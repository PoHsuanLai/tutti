//! **An in-process VST2 handle answers a pool thread while the main thread runs
//! `Session::update`.**
//!
//! The snapshot and parameter-read jobs the session hands out run on a pool
//! thread and call `AnyThread::save_state` and `AnyThread::parameter_value` on
//! the handle; the backend posts them to the main thread, and the update's
//! `Phase::Collect` runs them (`run_deferred`). `Session::update` is a
//! main-thread call (it debug-asserts that it is not off a claimed main
//! thread), so a host that does not call it on its main thread must call
//! `tutti_plugin_types::run_deferred` from its main-thread loop instead (the
//! `tutti-plugin` unit tests pump it directly).

use std::time::Duration;

use tutti_plugin::catalog::PluginRef;
use tutti_plugin_types::{MainThread, Normalized, ParamId};
use tutti_session::{PluginLoadRequest, Session};
use tutti_types::SampleRate;

/// **A pool thread asks the handle for its state and a parameter while the
/// main thread runs the session's update.** The answers are real: the state
/// equals one saved on the main thread, and the parameter is the value written
/// first.
#[test]
fn a_pool_thread_is_answered_while_the_main_thread_updates_the_session() {
    let main = MainThread::claim().expect("one test, one process");
    let mut session = Session::builder()
        .outputs_floor(2)
        .build_detached(SampleRate(48_000.0))
        .expect("a detached session builds");
    let node = session
        .add_plugin_blocking(
            &main,
            PluginLoadRequest::new(
                PluginRef::file(tutti_fixtures::cdylib(
                    "tutti_vst2_test_plugin",
                    "tutti-vst2-test-plugin",
                ))
                .expect("a plugin file"),
            ),
        )
        .expect("the in-process VST2 loads");
    let handle = session.plugin(node).expect("hosted").handle().clone();
    let id = ParamId::new(1);
    handle
        .any_thread()
        .set_parameter_value(id, Normalized::of(0.75));
    let expected = handle
        .any_thread()
        .save_state()
        .expect("saved on the main thread");

    let pool = std::thread::spawn(move || {
        (
            handle.any_thread().save_state(),
            handle.any_thread().parameter_value(id),
        )
    });
    while !pool.is_finished() {
        let _ = session.update();
        std::thread::sleep(Duration::from_millis(1));
    }
    let (state, value) = pool.join().expect("the pool thread returns");

    assert_eq!(state.expect("a real state, not the deadline"), expected);
    assert_eq!(value.map(Normalized::get), Some(0.75));
}
