//! The editor-only instance an out-of-process host loads
//! ([`OutOfProcess::load_editor`]): its idle is the only main-thread tick it
//! has, so what the plugin registers there must run.

use tutti_clap_host::ClapLoaded;
use tutti_clap_test_plugin::{CMD_REGISTER_TIMER, Site, ThreadCapture};
use tutti_fixtures::main_thread;
use tutti_plugin_types::{Editor, EditorWindow, Load, OutOfProcess};

/// A timer the plugin registers ticks on the editor's idle, which is the only
/// main-thread tick the editor instance has. The host answered
/// `register_timer` with `true` and never called `on_timer`, so a plugin that
/// animates its editor from a timer froze.
#[test]
fn a_timer_the_editor_registers_ticks_on_idle() {
    let path = tutti_fixtures::clap_probe();
    let main = main_thread();
    let mut editor = ClapLoaded::load_editor(&main, path, &Load::new()).expect("loads");

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
    editor.loaded_mut().on_main_thread(&main);
    assert!(read().timer_registered);
    let tick = Site::OnTimer as usize;
    assert_eq!(read().sites[tick].visits, 0);

    editor.editor_idle(&main);

    assert_eq!(read().sites[tick].visits, 1, "a period-0 timer is due");
}

/// **The editor's `save_state` is the plugin's own state**, what the host
/// gives the audio instance when the editor closes (design 021 Q8).
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
    let mut editor = ClapLoaded::load_editor(&main, path, &Load::new()).expect("loads");

    let through_editor = editor.save_state(&main).expect("saves");
    let plugin = Control::save_state(editor.loaded_mut(), &main).expect("saves");
    assert!(!plugin.is_empty(), "the probe saves a state");
    assert_eq!(through_editor, plugin);
}
