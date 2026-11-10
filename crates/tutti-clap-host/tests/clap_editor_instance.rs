//! The editor-only instance an out-of-process host loads
//! ([`OutOfProcess::load_editor`]): its idle is the only main-thread tick it
//! has, so what the plugin registers there must run.

use tutti_clap_host::ClapLoaded;
use tutti_clap_test_plugin::{CMD_REGISTER_TIMER, Site, ThreadCapture};
use tutti_plugin_types::{Editor, Load, MainThread, OutOfProcess};

/// This process's main thread. Each test is its own process under nextest, so
/// the claim cannot be refused.
fn main_thread() -> MainThread {
    MainThread::claim().expect("one test, one process")
}

/// A timer the plugin registers ticks on the editor's idle, which is the only
/// main-thread tick the editor instance has. The host answered
/// `register_timer` with `true` and never called `on_timer`, so a plugin that
/// animates its editor from a timer froze.
///
/// Mutation (run): delete the `Request::Timer(id)` arm from `editor_idle` →
/// the timer never ticks → fails.
#[test]
fn a_timer_the_editor_registers_ticks_on_idle() {
    let real = tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin");
    let path = tutti_fixtures::publish_with_extension(&real, "clap");
    let main = main_thread();
    let mut editor = ClapLoaded::load_editor(&main, &path, &Load::new()).expect("loads");

    // The probe's switches are `extern "C"` symbols in the image this
    // instance loaded: re-open the same path, not the linked rlib.
    // SAFETY: the reference plugin's library, already loaded by the editor; its
    // initialisers have run.
    let lib = unsafe { libloading::Library::new(&path) }.expect("re-opens");
    // SAFETY: the reference plugin exports both symbols with these signatures.
    let command: libloading::Symbol<unsafe extern "C" fn(u32) -> u32> =
        unsafe { lib.get(b"tutti_test_plugin_thread_command\0") }.expect("symbol");
    // SAFETY: as above.
    let capture: libloading::Symbol<unsafe extern "C" fn(*mut ThreadCapture) -> bool> =
        unsafe { lib.get(b"tutti_test_plugin_thread_capture\0") }.expect("symbol");
    let read = || {
        let mut cap = ThreadCapture::default();
        // SAFETY: `cap` is a valid, exclusive `ThreadCapture` for the call.
        assert!(unsafe { capture(&mut cap) });
        cap
    };

    // `on_main_thread` is where the plugin registers it.
    // SAFETY: a plain integer command latched by the plugin.
    unsafe { command(CMD_REGISTER_TIMER) };
    editor.loaded_mut().on_main_thread(&main);
    assert!(read().timer_registered);
    let tick = Site::OnTimer as usize;
    assert_eq!(read().sites[tick].visits, 0);

    editor.editor_idle(&main);

    assert_eq!(read().sites[tick].visits, 1, "a period-0 timer is due");
}
