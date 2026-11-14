//! What a plugin hands to the main thread, per session: the closes of plugins dropped
//! off the thread that made them.
//!
//! An in-process plugin dropped off its own thread cannot close itself there and must not
//! skip the close. It hands the close to the collector of the session that loaded it
//! (`tutti_plugin_types::MainWork`), which that session's `MainSession::update` drains. The
//! reference VST2 plugin counts the `effClose`s it receives, so each test reads the plugin's
//! own view: how many closes ran, and when.

use std::path::PathBuf;
use tutti_plugin::catalog::PluginRef;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{MainThread, MainWork};
use tutti_session::{PluginLoadRequest, Session};
use tutti_types::{ChannelLayout, SampleRate};

const RATE: SampleRate = SampleRate::SR_48K;

fn probe() -> PathBuf {
    tutti_fixtures::vst2_probe().to_path_buf()
}

/// How many `effClose`s the probe has received in this process, read from the image the
/// host loaded (the linked rlib is another image with other statics); the image stays
/// mapped.
fn closes() -> u32 {
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(probe()) }.expect("re-open the probe");
    // SAFETY: the symbol is the probe's `#[no_mangle]` export.
    let symbol: libloading::Symbol<extern "C" fn() -> u32> =
        unsafe { lib.get(b"tutti_vst2_probe_close_count\0") }.expect("a probe export");
    let count = symbol();
    std::mem::forget(lib);
    count
}

fn session() -> Session {
    Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(RATE)
        .expect("a detached session builds")
}

/// A probe plugin loaded in `session`, then removed from it and let go of by the session:
/// what is left of it is the returned handle, so dropping that is the last release.
fn loaded_and_released(session: &mut Session, main: &MainThread) -> PluginHandle {
    let node = session
        .add_plugin_blocking(
            main,
            PluginLoadRequest::new(PluginRef::file(probe()).expect("a plugin file")),
        )
        .expect("the in-process VST2 loads");
    let handle = session.plugin(node).expect("hosted").handle().clone();
    session.remove(node).expect("the plugin is removed");
    let mut frame = [0.0f32; 2];
    for _ in 0..4 {
        let _ = session.main(*main).update();
        session.render_frame(&mut frame);
    }
    assert_eq!(session.retired_plugins(), 0, "the session let go of it");
    handle
}

/// **Each session runs only its own plugins' closes.** Two sessions load a plugin each and
/// let go of it; the last handle of each is dropped off the thread that made the plugins,
/// so each close goes to the collector of the session it belongs to. One session's update
/// runs one close, and the other's runs the other: the plugin counts them.
#[test]
fn a_sessions_update_runs_only_its_own_plugins_closes() {
    let main = tutti_fixtures::main_thread();
    let (mut a, mut b) = (session(), session());
    let before = closes();
    let (handle_a, handle_b) = (
        loaded_and_released(&mut a, &main),
        loaded_and_released(&mut b, &main),
    );
    assert_eq!(closes(), before, "nothing closed while the handles live");

    std::thread::spawn(move || drop((handle_a, handle_b)))
        .join()
        .expect("the drop does not panic");
    assert_eq!(closes(), before, "no close ran on the dropping thread");

    let _ = a.main(main).update();
    assert_eq!(closes(), before + 1, "a's update closed a's plugin alone");
    let _ = b.main(main).update();
    assert_eq!(closes(), before + 2, "b's update closed b's");
    let _ = a.main(main).update();
    let _ = b.main(main).update();
    assert_eq!(closes(), before + 2, "each close ran once");
    a.close(&main);
    b.close(&main);
}

/// **`Session::close` runs every close that is owed.** A plugin dropped off its own thread
/// has handed its close to the session; `close` runs it, and the plugin counts it.
#[test]
fn close_runs_the_closes_the_session_is_owed() {
    let main = tutti_fixtures::main_thread();
    let mut session = session();
    let before = closes();
    let handle = loaded_and_released(&mut session, &main);
    std::thread::spawn(move || drop(handle)).join().unwrap();
    assert_eq!(closes(), before, "the drop closed nothing");
    session.close(&main);
    assert_eq!(closes(), before + 1, "close ran the owed close");
}

/// **A session dropped off the main thread counts exactly the closes it never ran.** The
/// plugins are dropped with the session on a thread that is not the main thread, each
/// hands its close to a collector that is going away, and the count of stranded work
/// rises by that many.
#[test]
fn a_session_dropped_off_the_main_thread_counts_its_stranded_closes() {
    let main = tutti_fixtures::main_thread();
    let mut session = session();
    for _ in 0..2 {
        session
            .add_plugin_blocking(
                &main,
                PluginLoadRequest::new(PluginRef::file(probe()).expect("a plugin file")),
            )
            .expect("the in-process VST2 loads");
    }
    let (closed, stranded) = (closes(), MainWork::stranded());
    std::thread::spawn(move || drop(session))
        .join()
        .expect("the drop does not panic");
    assert_eq!(closes(), closed, "no close ran off the main thread");
    assert_eq!(
        MainWork::stranded() - stranded,
        2,
        "each plugin's close was counted, not lost"
    );
}

/// **A thread that is not the main thread has no token.** The token is the OS's answer,
/// so a spawned thread cannot make one, whatever a test thread stands in for.
#[test]
fn a_spawned_thread_cannot_make_a_main_thread_token() {
    let none = std::thread::spawn(|| MainThread::new().is_none())
        .join()
        .unwrap();
    assert!(none);
}
