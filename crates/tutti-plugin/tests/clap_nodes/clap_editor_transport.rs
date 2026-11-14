//! **A transport request the plugin's editor makes reaches the host.** A
//! served plugin's editor is a second instance in the host's process
//! (`OutOfProcess::Editor`): the plugin's own buttons call `request_start` and
//! the rest on the host of that instance, not on the server's. The request
//! must come out where the server's own come out, `PluginHandle::subscribe`.
//!
//! Needs `cargo build -p tutti-plugin-server` first.

#![cfg(feature = "clap")]

use crate::clap_probe::{SAMPLE_RATE, load_probe};
use tutti_fixtures::{StandInWindow, machine_lock, main_thread};

use std::sync::{Arc, Mutex};

use tutti_clap_test_plugin::{ClapProbe, GuiMode, HOSTEXT_CMD_START, HOSTEXT_CMD_STOP};
use tutti_plugin_host::Notice;
use tutti_plugin_types::{ContentScale, TransportRequest};

/// **A request made in the served plugin's editor instance is a
/// `Notice::Transport`, each one, in order.** The plugin asks for its
/// `on_main_thread` and calls the host from it; the editor's idle serves the
/// callback and then takes what it raised.
/// Spec: clap.ext.transport_control.host.request_start, clap.ext.transport_control.host.request_stop
#[test]
fn served_a_request_the_editor_instance_makes_reaches_subscribers_in_order() {
    let _lock = machine_lock();
    let probe = ClapProbe::open(
        &tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
        tutti_fixtures::clap_probe(),
    );
    probe.set_gui_mode(GuiMode::Embeddable);
    let loaded = load_probe(SAMPLE_RATE);
    let main = main_thread();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let _subscription = loaded.handle.subscribe({
        let heard = Arc::clone(&heard);
        move |notice| {
            if let Notice::Transport(request) = notice {
                heard.lock().expect("the list").push(request);
            }
        }
    });
    loaded
        .handle
        .open_editor(main, StandInWindow.handle(), ContentScale::IDENTITY)
        .expect("the editor instance opens");

    for cmd in [HOSTEXT_CMD_START, HOSTEXT_CMD_STOP] {
        probe.hostext_command_via_callback(cmd, 0, 0);
        // The first idle serves the callback, the second takes what it raised.
        loaded.handle.editor_idle(main);
        loaded.handle.editor_idle(main);
    }

    assert_eq!(
        *heard.lock().expect("the list"),
        [TransportRequest::Start, TransportRequest::Stop]
    );
}

/// **In process, a request the plugin makes is a `Notice::Transport` too**,
/// through the same subscription: the instance that renders is the one that
/// shows the editor, and its idle is the pass that takes the request.
/// Spec: clap.ext.transport_control.host.request_start
#[test]
fn in_process_a_request_the_plugin_makes_reaches_subscribers() {
    use tutti_clap_host::ClapLoaded;
    use tutti_plugin_types::Load;
    use tutti_types::SampleRate;

    let _lock = machine_lock();
    let probe = ClapProbe::open(
        &tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
        tutti_fixtures::clap_probe(),
    );
    let main = main_thread();
    let (_client, handle) = tutti_plugin_host::load_client::<ClapLoaded>(
        main,
        &tutti_fixtures::detached_work(),
        tutti_fixtures::clap_probe(),
        &Load::new(),
        SampleRate::of(SAMPLE_RATE),
    )
    .expect("the reference plugin loads in process");
    let heard = Arc::new(Mutex::new(Vec::new()));
    let _subscription = handle.subscribe({
        let heard = Arc::clone(&heard);
        move |notice| {
            if let Notice::Transport(request) = notice {
                heard.lock().expect("the list").push(request);
            }
        }
    });

    probe.hostext_command_via_callback(HOSTEXT_CMD_START, 0, 0);
    // The first pass serves the callback, the second takes what it raised.
    handle.editor_idle(main);
    handle.editor_idle(main);

    assert_eq!(*heard.lock().expect("the list"), [TransportRequest::Start]);
}
