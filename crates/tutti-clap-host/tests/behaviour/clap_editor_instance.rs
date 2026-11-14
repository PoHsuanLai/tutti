//! The editor-only instance an out-of-process host loads
//! ([`OutOfProcess::load_editor`]): its idle is the only main-thread tick it
//! has, so what the plugin registers there must run.

use tutti_clap_host::ClapLoaded;
use tutti_clap_test_plugin::{CMD_REGISTER_TIMER, Site, ThreadCapture};
use tutti_fixtures::main_thread;
use tutti_plugin_types::{Editor, EditorWindow, Load, OutOfProcess};

/// A timer the plugin registers ticks when the host's pump finds it due
/// (`wakeups` and `wake`, which the editor's `editor_idle` then serves: the
/// only main-thread tick the editor instance has). The host answered
/// `register_timer` with `true` and never called `on_timer`, so a plugin that
/// animates its editor from a timer froze. A period of zero is floored for
/// the wake-up (it must not spin the host's loop), so it is due once the floor
/// has passed.
#[test]
fn a_timer_the_editor_registers_ticks_on_idle() {
    let path = tutti_fixtures::clap_probe();
    let main = main_thread();
    let mut editor =
        ClapLoaded::load_editor(main, &tutti_fixtures::detached_work(), path, &Load::new())
            .expect("loads");

    // The probe's switches are `extern "C"` symbols in the image this
    // instance loaded: re-open the same path, not the linked rlib.
    // SAFETY: the reference plugin's library, already loaded by the editor; its
    // initialisers have run.
    let lib = unsafe { libloading::Library::new(path) }.expect("re-opens");
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
    editor.loaded_mut().on_main_thread(main);
    assert!(read().timer_registered);
    let tick = Site::OnTimer as usize;
    assert_eq!(read().sites[tick].visits, 0);

    // Before the floor has passed the loop is not woken for it, and the timer
    // is not latched.
    EditorWindow::wake(&mut editor, main, &[], std::time::Instant::now());
    editor.editor_idle(main);
    assert_eq!(read().sites[tick].visits, 0, "not before its wake-up");

    std::thread::sleep(std::time::Duration::from_millis(30));
    EditorWindow::wake(&mut editor, main, &[], std::time::Instant::now());
    editor.editor_idle(main);
    assert_eq!(read().sites[tick].visits, 1, "a period-0 timer is due");
}

/// **The editor's `save_state` is the plugin's own state**, what the host
/// gives the audio instance when the editor closes.
///
/// The probe keeps its parameter values in process globals, which every
/// instance in this process shares, so a round trip between two instances
/// here cannot tell a carried state from a shared one; the bytes are compared
/// with what the loaded plugin saves instead. That the audio instance takes
/// them is the bridge's (`state_back_tests` in `tutti-plugin`).
#[test]
fn an_editors_state_is_the_plugins_state() {
    use tutti_plugin_types::Control;

    let path = tutti_fixtures::clap_probe();
    let main = main_thread();
    let mut editor =
        ClapLoaded::load_editor(main, &tutti_fixtures::detached_work(), path, &Load::new())
            .expect("loads");

    let through_editor = editor.save_state(main).expect("saves");
    let plugin = Control::save_state(
        editor.loaded_mut(),
        main,
        tutti_plugin_types::StateUse::Project,
    )
    .expect("saves");
    assert!(!plugin.is_empty(), "the probe saves a state");
    assert_eq!(through_editor, plugin);
}

/// A request the plugin makes through the editor instance's host is kept for
/// the application, not dropped by the idle that serves the instance: the
/// plugin's own buttons call the host of the instance that shows them.
/// Spec: clap.ext.transport_control.host.request_start
#[test]
fn a_transport_request_the_editor_instance_makes_is_taken_from_it() {
    use tutti_clap_test_plugin::HOSTEXT_CMD_START;
    use tutti_plugin_types::{Request, TransportRequest};

    let path = tutti_fixtures::clap_probe();
    let main = main_thread();
    let mut editor =
        ClapLoaded::load_editor(main, &tutti_fixtures::detached_work(), path, &Load::new())
            .expect("loads");
    // SAFETY: the reference plugin's library, already loaded by the editor.
    let lib = unsafe { libloading::Library::new(path) }.expect("re-opens");
    // SAFETY: the reference plugin exports this symbol with this signature.
    let command: libloading::Symbol<unsafe extern "C" fn(u32, i64, i64)> =
        unsafe { lib.get(b"tutti_test_plugin_hostext_command\0") }.expect("symbol");

    // SAFETY: plain integers latched by the plugin.
    unsafe { command(HOSTEXT_CMD_START, 0, 0) };
    editor.loaded_mut().on_main_thread(main);
    editor.editor_idle(main);

    let mut asked = Vec::new();
    editor.take_requests(main, &mut asked);
    assert_eq!(asked, [Request::Transport(TransportRequest::Start)]);
    editor.take_requests(main, &mut asked);
    assert_eq!(
        asked,
        [Request::Transport(TransportRequest::Start)],
        "taken once"
    );
}
